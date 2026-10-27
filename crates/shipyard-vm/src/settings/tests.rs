#![allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]

use super::*;

/// A Mac with `ram` GiB and `cores` cores, the framework allowing 64 CPUs.
fn mac(ram: u64, cores: u32) -> Mac {
    Mac {
        ram_mib: ram << 10,
        cores,
        max_cpus: 64,
    }
}

/// The default is a quarter of the RAM, from 4 GiB to 16 GiB and never past
/// half of it; half the cores, from 1 to 8 and never past the framework's
/// most.
#[test]
fn the_default_size_follows_the_mac() {
    assert_eq!(defaults(mac(8, 8)), (4 << 10, 4), "the floor");
    assert_eq!(defaults(mac(24, 8)), (6 << 10, 4), "a quarter");
    assert_eq!(defaults(mac(128, 24)), (16 << 10, 8), "the caps");
    assert_eq!(defaults(mac(6, 1)), (3 << 10, 1), "never past half the RAM");
    let mut small = mac(24, 16);
    small.max_cpus = 3;
    assert_eq!(defaults(small).1, 3, "the framework's most");
}

/// What `vm set` asked for is taken, kept within this Mac's bounds (a
/// `vm.json` from a bigger Mac made to fit); what it didn't ask for is the
/// default.
#[test]
fn a_set_size_is_taken_within_bounds() {
    let mac = mac(24, 8);
    let none = resolve(None, mac);
    assert_eq!(none.memory_mib, (6 << 10, Source::Default));
    assert_eq!(none.cpus, (4, Source::Default));
    let set = VmSettings {
        version: 1,
        memory_mib: Some(10 << 10),
        cpus: None,
    };
    let size = resolve(Some(&set), mac);
    assert_eq!(size.memory_mib, (10 << 10, Source::Set));
    assert_eq!(size.cpus, (4, Source::Default));
    let too_big = VmSettings {
        version: 1,
        memory_mib: Some(64 << 10),
        cpus: Some(32),
    };
    let size = resolve(Some(&too_big), mac);
    assert_eq!(
        size.memory_mib,
        (22 << 10, Source::Set),
        "the RAM less 2 GiB"
    );
    assert_eq!(size.cpus, (8, Source::Set), "the cores");
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shipyard-vm-settings-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("vm.json")
}

/// `vm set` writes what it's asked within bounds, puts a part back to the
/// default, and refuses a size out of them, writing nothing.
#[test]
fn vm_set_keeps_what_it_is_asked() {
    let path = scratch("set");
    let mac = mac(24, 8);
    assert_eq!(VmSettings::read(&path).unwrap(), None);
    let set = VmSettings::change(&path, mac, Change::To(8 << 30), Change::To(6)).unwrap();
    assert_eq!(
        set,
        VmSettings {
            version: 1,
            memory_mib: Some(8 << 10),
            cpus: Some(6)
        }
    );
    assert_eq!(VmSettings::read(&path).unwrap(), Some(set));
    let back = VmSettings::change(&path, mac, Change::Default, Change::Keep).unwrap();
    assert_eq!((back.memory_mib, back.cpus), (None, Some(6)));
    for (memory, cpus) in [
        (Change::To(1 << 30), Change::Keep),
        (Change::To(23 << 30), Change::Keep),
        (Change::Keep, Change::To(0)),
        (Change::Keep, Change::To(9)),
    ] {
        let refused = VmSettings::change(&path, mac, memory, cpus).unwrap_err();
        assert!(matches!(refused, VmError::Size(_)), "{refused}");
    }
    assert_eq!(VmSettings::read(&path).unwrap(), Some(back), "unchanged");
    // One that can't be read (a newer Shipyard's) is replaced by what's asked.
    std::fs::write(&path, r#"{"version":2,"memory_mib":4096}"#).unwrap();
    let replaced = VmSettings::change(&path, mac, Change::Keep, Change::To(3)).unwrap();
    assert_eq!(
        (replaced.version, replaced.memory_mib, replaced.cpus),
        (1, None, Some(3))
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A `vm.json` of a newer version, or with what this one doesn't know, is
/// refused, never guessed at.
#[test]
fn a_newer_or_unknown_vm_json_is_refused() {
    for text in [
        r#"{"version":2,"memory_mib":4096}"#,
        r#"{"version":1,"memory_mib":4096,"balloon":true}"#,
        "not json",
    ] {
        let path = scratch("wrong");
        std::fs::write(&path, text).unwrap();
        assert!(
            matches!(VmSettings::read(&path), Err(VmError::Io { .. })),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
    let path = scratch("right");
    std::fs::write(&path, r#"{"version":1,"cpus":2}"#).unwrap();
    assert_eq!(VmSettings::read(&path).unwrap().unwrap().cpus, Some(2));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Sizes are said as a person reads them.
#[test]
fn a_size_is_shown_plainly() {
    assert_eq!(shown(6 << 10), "6 GiB");
    assert_eq!(shown(1536), "1536 MiB");
}
