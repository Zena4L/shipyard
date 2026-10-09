//! Shipyard's tar reader against the `tar` crate (H8): archives the `tar`
//! crate writes, read by both, must give the same entries. Generated
//! archives (proptest) in GNU and ustar form, and hand-made PAX ones (long
//! names, fractional times, extended attributes). The `tar` crate applies a
//! PAX path and link itself; its `mtime`, `uid`, `gid` and extended
//! attributes it only hands over, so for those the reference is this test's
//! reading of them.
//!
//! Where Shipyard refuses on purpose (`..`, absolute paths, its limits), the
//! `tar` crate reads on: those are tested in `tar_reader.rs`'s own tests,
//! not compared here. One known difference the other way: an empty numeric
//! field (a header that leaves the owner blank) is 0 to Shipyard, as to GNU
//! tar, and an error to the `tar` crate.

use std::ffi::OsString;
use std::path::{Component, PathBuf};

use proptest::prelude::*;

use super::{Entry, Kind, TarLimits, TarReader};

/// What's compared of an entry: path, kind, link, mode, owner, group,
/// mtime (seconds, nanoseconds), size, extended attributes.
type View = (
    PathBuf,
    Kind,
    Option<OsString>,
    u32,
    u32,
    u32,
    (i64, u32),
    u64,
    Vec<(OsString, Vec<u8>)>,
);

fn view(entry: Entry) -> View {
    (
        entry.path,
        entry.kind,
        entry.link,
        entry.mode,
        entry.uid,
        entry.gid,
        entry.mtime,
        entry.size,
        entry.xattrs,
    )
}

/// Only the normal components: `./a/` is `a`, as Shipyard cleans paths.
fn clean(path: &std::path::Path) -> PathBuf {
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .collect()
}

fn ours(archive: &[u8]) -> Vec<View> {
    let mut reader = TarReader::new(archive, TarLimits::default());
    let mut entries = Vec::new();
    while let Some(entry) = reader.next_entry().unwrap() {
        entries.push(view(entry));
    }
    entries
}

/// The same archive as the `tar` crate reads it.
fn theirs(archive: &[u8]) -> Vec<View> {
    let mut archive = tar::Archive::new(archive);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let kind = match header.entry_type() {
                tar::EntryType::Regular | tar::EntryType::Continuous => Kind::File,
                tar::EntryType::Directory => Kind::Directory,
                tar::EntryType::Symlink => Kind::Symlink,
                tar::EntryType::Link => Kind::Hardlink,
                tar::EntryType::Fifo => Kind::Fifo,
                tar::EntryType::Char => Kind::CharDevice,
                tar::EntryType::Block => Kind::BlockDevice,
                other => panic!("an entry type the tests don't make: {other:?}"),
            };
            let mut mtime = (i64::try_from(header.mtime().unwrap()).unwrap(), 0);
            let mut uid = u32::try_from(header.uid().unwrap()).unwrap();
            let mut gid = u32::try_from(header.gid().unwrap()).unwrap();
            let mut xattrs = Vec::new();
            if let Some(extensions) = entry.pax_extensions().unwrap() {
                for extension in extensions {
                    let extension = extension.unwrap();
                    let key = extension.key().unwrap();
                    if key == "mtime" {
                        let value = extension.value().unwrap();
                        let (seconds, fraction) = value.split_once('.').unwrap_or((value, ""));
                        let nanos = format!("{fraction:0<9}");
                        mtime = (seconds.parse().unwrap(), nanos[..9].parse().unwrap());
                    } else if key == "uid" {
                        uid = extension.value().unwrap().parse().unwrap();
                    } else if key == "gid" {
                        gid = extension.value().unwrap().parse().unwrap();
                    } else if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
                        xattrs.push((OsString::from(name), extension.value_bytes().to_vec()));
                    }
                }
            }
            let link = entry.link_name().unwrap().map(|link| {
                if kind == Kind::Hardlink {
                    clean(&link).into_os_string()
                } else {
                    link.into_owned().into_os_string()
                }
            });
            let path = clean(&entry.path().unwrap());
            (
                path,
                kind,
                link,
                header.mode().unwrap() & 0o7777,
                uid,
                gid,
                mtime,
                if kind == Kind::File {
                    header.size().unwrap()
                } else {
                    0
                },
                xattrs,
            )
        })
        .collect()
}

/// An entry to write: its path, kind, mode, owner, group, mtime and data
/// (a file's) or link target.
#[derive(Debug, Clone)]
struct Spec {
    path: String,
    kind: Kind,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    data: Vec<u8>,
    link: String,
}

fn write(specs: &[Spec], gnu: bool) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for spec in specs {
        let mut header = if gnu {
            tar::Header::new_gnu()
        } else {
            tar::Header::new_ustar()
        };
        header.set_entry_type(match spec.kind {
            Kind::File => tar::EntryType::Regular,
            Kind::Directory => tar::EntryType::Directory,
            Kind::Symlink => tar::EntryType::Symlink,
            Kind::Hardlink => tar::EntryType::Link,
            Kind::Fifo => tar::EntryType::Fifo,
            Kind::CharDevice => tar::EntryType::Char,
            Kind::BlockDevice => tar::EntryType::Block,
        });
        header.set_mode(spec.mode);
        header.set_uid(spec.uid.into());
        header.set_gid(spec.gid.into());
        header.set_mtime(spec.mtime);
        let data: &[u8] = if spec.kind == Kind::File {
            &spec.data
        } else {
            &[]
        };
        header.set_size(data.len() as u64);
        match spec.kind {
            Kind::Symlink | Kind::Hardlink => {
                builder.append_link(&mut header, &spec.path, &spec.link)?;
            }
            _ => builder.append_data(&mut header, &spec.path, data)?,
        }
    }
    builder.into_inner()
}

/// A path of 1–4 normal components, short or long (past ustar's 100 and
/// 155 bytes, which the GNU form writes as long-name entries and ustar
/// splits into a prefix, or refuses).
fn path() -> impl Strategy<Value = String> {
    prop::collection::vec("[a-zA-Z0-9_.-]{1,60}", 1..4)
        .prop_filter("normal components only", |parts| {
            parts.iter().all(|part| part != "." && part != "..")
        })
        .prop_map(|parts| parts.join("/"))
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        path(),
        prop_oneof![
            Just(Kind::File),
            Just(Kind::Directory),
            Just(Kind::Symlink),
            Just(Kind::Hardlink),
            Just(Kind::Fifo),
        ],
        0u32..=0o7777,
        0u32..70_000,
        0u32..70_000,
        0u64..8_000_000_000,
        prop::collection::vec(any::<u8>(), 0..600),
        path(),
    )
        .prop_map(|(path, kind, mode, uid, gid, mtime, data, link)| Spec {
            path,
            kind,
            mode,
            uid,
            gid,
            mtime,
            data,
            link,
        })
}

proptest! {
    #[test]
    fn both_read_generated_archives_the_same(
        specs in prop::collection::vec(spec(), 1..12),
        gnu in any::<bool>(),
    ) {
        // A ustar header can't hold every long path: the `tar` crate
        // refuses to write those, and they're not compared.
        let archive = write(&specs, gnu);
        prop_assume!(archive.is_ok());
        let archive = archive.unwrap();
        prop_assert_eq!(ours(&archive), theirs(&archive));
    }
}

/// PAX extended headers: a long path and link, a fractional mtime, extended
/// attributes, a big owner.
#[test]
fn both_read_pax_extensions_the_same() {
    let mut builder = tar::Builder::new(Vec::new());
    let long = "dir/".repeat(40) + "file";
    builder
        .append_pax_extensions([
            ("path", long.as_bytes()),
            ("mtime", b"1700000000.25".as_slice()),
            ("uid", b"3000000".as_slice()),
            ("SCHILY.xattr.user.note", b"hello".as_slice()),
            (
                "SCHILY.xattr.security.capability",
                [1u8, 0, 0, 2].as_slice(),
            ),
        ])
        .unwrap();
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o4755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(3);
    header.set_mtime(1_700_000_000);
    builder
        .append_data(&mut header, "short", b"abc".as_slice())
        .unwrap();
    builder
        .append_pax_extensions([("linkpath", long.as_bytes())])
        .unwrap();
    let mut link = tar::Header::new_ustar();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_mode(0o777);
    link.set_uid(0);
    link.set_gid(0);
    link.set_mtime(0);
    link.set_size(0);
    builder
        .append_link(&mut link, "to-it", "placeholder")
        .unwrap();
    let mut device = tar::Header::new_gnu();
    device.set_entry_type(tar::EntryType::Char);
    device.set_mode(0o666);
    device.set_uid(0);
    device.set_gid(0);
    device.set_mtime(0);
    device.set_size(0);
    device.set_device_major(1).unwrap();
    device.set_device_minor(3).unwrap();
    builder
        .append_data(&mut device, "null", [].as_slice())
        .unwrap();
    let archive = builder.into_inner().unwrap();
    let ours = ours(&archive);
    assert_eq!(ours, theirs(&archive));
    assert_eq!(ours[0].0, PathBuf::from(&long));
    assert_eq!(ours[0].6, (1_700_000_000, 250_000_000));
    assert_eq!(ours[1].2.as_deref(), Some(std::ffi::OsStr::new(&long)));
}
