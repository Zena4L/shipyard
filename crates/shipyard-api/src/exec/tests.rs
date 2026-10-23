use super::*;

fn exec_config() -> ExecConfig {
    ExecConfig {
        cmd: vec!["sh".to_owned(), "-c".to_owned(), "echo hi".to_owned()],
        env: vec![
            "TOKEN=s3cret".to_owned(),
            "EMPTY=".to_owned(),
            "BARE".to_owned(),
        ],
        user: Some("1000:1000".to_owned()),
        workdir: Some("/srv".to_owned()),
        tty: true,
        stdin: true,
        streams: [true, true],
        detach_keys: DetachKeys::default_keys(),
        detach_keys_text: String::new(),
    }
}

/// The child's argv names each variable's key, never its value: values go
/// in `SHIPYARD_CALLER_ENV`; a bare key (no value) is left out; the
/// container and the command come after `--`.
#[test]
fn a_value_is_never_on_the_child_s_argv() {
    let pid_file = std::path::Path::new("/run/shipyard/.exec-e1.pid");
    let (args, caller_env) = child_command("0123abcd", &exec_config(), Some(pid_file));
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "exec",
            "--interactive",
            "--tty",
            "--user=1000:1000",
            "--workdir=/srv",
            "--env=TOKEN",
            "--env=EMPTY",
            "--pid-file=/run/shipyard/.exec-e1.pid",
            "--",
            "0123abcd",
            "sh",
            "-c",
            "echo hi"
        ]
    );
    assert!(!args.iter().any(|arg| arg.contains("s3cret")));
    let carried = |key| shipyard_runtime::attach::forward::caller_var(&caller_env, key);
    assert_eq!(carried("TOKEN").as_deref(), Some("s3cret"));
    assert_eq!(carried("EMPTY").as_deref(), Some(""));
    assert_eq!(carried("BARE"), None);
}

/// A command is needed; `Privileged` is refused; the rest is read as
/// Docker's client sends it.
#[test]
fn an_exec_config_is_read_from_docker_s() {
    let read = |json: serde_json::Value| config(serde_json::from_value(json).unwrap());
    let config = read(serde_json::json!({
        "AttachStdin": false, "AttachStdout": true, "AttachStderr": true,
        "Tty": false, "Cmd": ["ls", "-l"], "Env": null, "User": "",
        "WorkingDir": "", "Privileged": false, "DetachKeys": ""
    }))
    .unwrap();
    assert_eq!(config.cmd, ["ls", "-l"]);
    assert_eq!((config.user, config.workdir), (None, None));
    assert_eq!((config.stdin, config.streams), (false, [true, true]));
    // Its detach keys: Docker's when none are given (F117), its own read.
    assert_eq!(config.detach_keys, DetachKeys::default_keys());
    let keyed = read(serde_json::json!({"Cmd": ["sh"], "DetachKeys": "ctrl-x,y"})).unwrap();
    assert_eq!(keyed.detach_keys.bytes(), [0x18, b'y']);
    assert_eq!(keyed.detach_keys_text, "ctrl-x,y");
    for refused in [
        serde_json::json!({"Cmd": ["ls"], "DetachKeys": "ctrl-1"}),
        serde_json::json!({"Cmd": ["ls"], "Privileged": true}),
        serde_json::json!({"Cmd": []}),
        serde_json::json!({}),
        serde_json::json!({"Cmd": ["ls"], "Env": [format!("BIG={}", "x".repeat(ENV_BYTES))]}),
        // Under the bound as given, past it once escaped (`"` is `\"`).
        serde_json::json!({"Cmd": ["ls"], "Env": [format!("Q={}", "\"".repeat(ENV_BYTES * 3 / 4))]}),
    ] {
        assert_eq!(
            read(refused.clone()).unwrap_err().status,
            StatusCode::BAD_REQUEST,
            "{refused}"
        );
    }
}

/// `GET /exec/{id}/json` says it as Docker does: its exit code `null`
/// while it runs.
#[test]
fn an_exec_is_inspected_as_docker_shows_it() {
    let instance = Instance {
        container: "0123abcd".to_owned(),
        config: ExecConfig {
            detach_keys_text: "ctrl-x".to_owned(),
            ..exec_config()
        },
        state: Mutex::new(State {
            started: true,
            running: true,
            pid: Some(42),
            ..State::new(Instant::now())
        }),
        pid_file: None,
    };
    let shown = serde_json::to_value(inspected("e1".to_owned(), &instance)).unwrap();
    assert_eq!(
        shown,
        serde_json::json!({
            "ID": "e1", "Running": true, "ExitCode": null,
            "ProcessConfig": {"privileged": false, "user": "1000:1000", "tty": true,
                              "entrypoint": "sh", "arguments": ["-c", "echo hi"]},
            "OpenStdin": true, "OpenStdout": true, "OpenStderr": true, "CanRemove": false,
            "ContainerID": "0123abcd", "DetachKeys": "ctrl-x", "Pid": 42
        })
    );
    {
        let mut state = instance.state();
        state.running = false;
        state.exit_code = Some(3);
    }
    let ended = serde_json::to_value(inspected("e1".to_owned(), &instance)).unwrap();
    assert_eq!(
        (ended["Running"].clone(), ended["ExitCode"].clone()),
        (false.into(), 3.into())
    );
}

/// Kept while it runs; once made and not started, or ended, for `KEPT`.
#[test]
fn instances_are_kept_for_a_while() {
    let now = Instant::now();
    let instance = |running, since| Instance {
        container: String::new(),
        config: exec_config(),
        state: Mutex::new(State {
            running,
            ..State::new(since)
        }),
        pid_file: None,
    };
    let long_ago = now.checked_sub(KEPT * 2).unwrap();
    assert!(instance(false, long_ago).expired(now));
    assert!(!instance(true, long_ago).expired(now), "running");
    assert!(!instance(false, now).expired(now), "just made");
}

/// An output is drained once its exec has ended, and given up after a
/// quiet look even while something still holds it open (a process the exec
/// left in the background): what came before its end is passed on.
#[test]
fn an_output_held_open_is_given_up_after_the_end() {
    use std::io::Write as _;
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(b"said").unwrap();
    let ended = Arc::new(AtomicBool::new(false));
    let (got, given) = std::sync::mpsc::channel();
    let pumped = {
        let ended = Arc::clone(&ended);
        std::thread::spawn(move || {
            pump(&OwnedFd::from(reader), &ended, |bytes| {
                let _ = got.send(bytes.to_vec());
                Ok(())
            });
        })
    };
    assert_eq!(
        given.recv_timeout(Duration::from_secs(10)).unwrap(),
        b"said"
    );
    ended.store(true, Ordering::Release);
    let began = Instant::now();
    while !pumped.is_finished() {
        assert!(
            began.elapsed() < DRAIN_WAIT + Duration::from_secs(5),
            "still pumping a pipe held open"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    pumped.join().unwrap();
    drop(writer);
}

/// What execs left in the runtime root goes at the service's start, once
/// older than `KEPT`: their PID files and their writes' temporaries; a
/// fresh one (another service's, live) and anything else stay.
#[test]
fn old_pid_files_are_swept() {
    let dir = std::env::temp_dir().join(format!("exec-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = std::time::SystemTime::now() - KEPT * 2;
    let make = |name: &str, aged: bool| {
        let file = std::fs::File::create(dir.join(name)).unwrap();
        if aged {
            file.set_modified(old).unwrap();
        }
    };
    make(".exec-gone.pid", true);
    make("..exec-cut.pid.42.0.tmp", true);
    make(".exec-live.pid", false);
    make("api.sock.lock", true);
    sweep_pid_files(&dir);
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [".exec-live.pid", "api.sock.lock"]);
    std::fs::remove_dir_all(&dir).unwrap();
}
