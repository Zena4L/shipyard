use super::*;

/// `--tmpfs PATH[:OPTIONS]`: Docker's defaults (`noexec,nosuid,nodev`),
/// the options given winning in order, the data options as `mount(2)`'s
/// data; what isn't one refused.
#[test]
fn a_tmpfs_is_docker_s() {
    let plain = TmpfsMount::parse("/run").unwrap();
    assert!(plain.options().is_empty());
    let mount = plain.to_runtime();
    assert_eq!(
        mount.flags,
        [MountFlag::NoExec, MountFlag::NoSuid, MountFlag::NoDev]
    );
    assert_eq!(mount.data, None);
    let given = TmpfsMount::parse("/scratch:size=64m,exec,mode=1770,ro,uid=1000").unwrap();
    assert_eq!(given.options_text(), "size=64m,exec,mode=1770,ro,uid=1000");
    let mount = given.to_runtime();
    assert!(!mount.has(MountFlag::NoExec), "exec given");
    assert!(mount.has(MountFlag::ReadOnly) && mount.has(MountFlag::NoSuid));
    assert_eq!(mount.data.as_deref(), Some("size=64m,mode=1770,uid=1000"));
    let flip = TmpfsMount::parse("/t:ro,rw").unwrap().to_runtime();
    assert!(TmpfsMount::parse("/t:mode=01777").is_ok(), "leading zeros");
    assert!(TmpfsMount::parse("/t:ro").unwrap().read_only());
    assert!(!flip.has(MountFlag::ReadOnly), "the last of a pair wins");
    for refused in [
        "run",
        "/t:size=64m,mode=777,x=1",
        "/t:size=1x",
        "/t:mode=0999",
        "/t:mode=17777",
        "/t:mode=",
        "/t:uid=-1",
        "/t:size=",
        "/t:tmpcopyup",
    ] {
        assert_eq!(
            TmpfsMount::parse(refused).unwrap_err().kind(),
            ErrorKind::InvalidRequest,
            "{refused}"
        );
    }
}

/// `--mount`: a bind, a named volume and an anonymous one as `--volume`
/// strings, a tmpfs as a tmpfs; Docker's keys and spellings; what isn't
/// taken refused.
#[test]
fn a_mount_is_docker_s() {
    let parsed = |spec: &str| parse_mount(spec).unwrap();
    assert_eq!(
        parsed("type=bind,source=/srv/data,target=/data,readonly"),
        DockerMount::Volume("/srv/data:/data:ro".to_owned())
    );
    assert_eq!(
        parsed("type=BIND,src=/srv,dst=/s,readonly=TRUE"),
        DockerMount::Volume("/srv:/s:ro".to_owned()),
        "Docker's spellings"
    );
    assert_eq!(
        parsed("type=bind,src=/srv,dst=/s,ro=false,bind-propagation=rprivate"),
        DockerMount::Volume("/srv:/s".to_owned())
    );
    assert_eq!(
        parsed("source=db,target=/var/lib/db,volume-nocopy"),
        DockerMount::Volume("db:/var/lib/db:nocopy".to_owned()),
        "a volume by default"
    );
    assert_eq!(
        parsed("type=tmpfs,destination=/t,tmpfs-size=1m,tmpfs-mode=1770"),
        DockerMount::Tmpfs(
            TmpfsMount::new("/t", ["size=1m".to_owned(), "mode=1770".to_owned()]).unwrap()
        )
    );
    assert_eq!(
        parsed("type=volume,target=/data,readonly"),
        DockerMount::Volume("/data:ro".to_owned()),
        "no source: an anonymous volume, as `-v /data:ro`"
    );
    assert_eq!(
        parsed("source=v,target=/v,readonly,volume-nocopy=true"),
        DockerMount::Volume("v:/v:ro,nocopy".to_owned())
    );
    assert_eq!(
        parsed("source=v,target=/v,volume-nocopy=false"),
        DockerMount::Volume("v:/v".to_owned()),
        "no-copy off: Docker's default"
    );
    assert_eq!(
        parsed("target=/cache"),
        DockerMount::Volume("/cache".to_owned())
    );
    for refused in [
        "type=volume,target=data",
        "type=volume,target=/a:b",
        "type=bind,source=relative,target=/d",
        "type=volume,source=/abs,target=/d",
        "type=bind,source=/a:b,target=/d",
        "type=bind,source=/a",
        "type=tmpfs,source=x,target=/t",
        "type=bind,source=/a,target=/d,tmpfs-size=1m",
        "type=npipe,source=x,target=/d",
        "type=bind,source=/a,target=/d,bind-propagation=shared",
        "type=volume,source=v,target=/d,volume-opt=type=nfs",
        "type=bind,source=/a,target=/d,volume-nocopy",
        "type=bind,source=/a,target=/d,readonly=maybe",
        "type=bind,source=/a,target=/d,bind-propagation=rslave",
        "type=tmpfs,target=/t,tmpfs-mode=999",
    ] {
        assert_eq!(
            parse_mount(refused).unwrap_err().kind(),
            ErrorKind::InvalidRequest,
            "{refused}"
        );
    }
}

/// A bind asks for its source to be made only with `create_mountpoint`;
/// a volume never (refused).
#[test]
fn a_bind_s_source_is_made_only_when_asked() {
    let mut bind = MountRequest::new("bind", "/d");
    bind.source = Some("/srv/new".to_owned());
    assert_eq!(bind.source_to_make(), None);
    bind.create_mountpoint = true;
    assert_eq!(
        bind.source_to_make(),
        Some(std::path::Path::new("/srv/new"))
    );
    let mut volume = MountRequest::new("volume", "/d");
    volume.source = Some("v".to_owned());
    volume.create_mountpoint = true;
    assert_eq!(volume.source_to_make(), None);
    assert_eq!(
        volume.resolve().unwrap_err().kind(),
        ErrorKind::InvalidRequest
    );
}
