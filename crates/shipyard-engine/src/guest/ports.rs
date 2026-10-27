//! `__guest-ports` (step 10.3c1): what the guest's containers publish, as
//! a [`PortsAnswer`] on stdout, for the VM's host (through the agent, which
//! runs it as any command) to listen on the Mac.
//!
//! [`PortsAnswer`]: shipyard_runtime::attach::forward::PortsAnswer

use std::io::Write as _;
use std::sync::Arc;

/// Prints the answer, at the version its argument asks (none: v1, an older
/// host's); its exit code (1: it couldn't be had, said on stderr).
pub(super) fn print() -> i32 {
    let engine = crate::Engine::new(crate::Paths::from_env(), Arc::new(crate::report::Quiet));
    let asked = std::env::args().nth(2);
    let answer = crate::containers::published(&engine)
        .map(|answer| at_version(answer, asked.as_deref()))
        .map_err(|e| e.to_string());
    let bytes = answer.and_then(|answer| answer.encode().map_err(|e| e.to_string()));
    let written = bytes.and_then(|bytes| {
        let mut out = std::io::stdout().lock();
        out.write_all(&bytes)
            .and_then(|()| out.flush())
            .map_err(|e| e.to_string())
    });
    match written {
        Ok(()) => 0,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "shipyard: the guest's ports: {why}");
            1
        }
    }
}

/// `answer` at the version `asked` names: v3 (or newer) as it is, v2 as
/// v2 says it, else v1 (an older host reads only what it knows).
fn at_version(
    answer: shipyard_runtime::attach::forward::PortsAnswer,
    asked: Option<&str>,
) -> shipyard_runtime::attach::forward::PortsAnswer {
    use shipyard_runtime::attach::forward::{PORTS_COMES_BACK, PORTS_LIST_UDP};
    match asked.and_then(|asked| asked.parse::<u32>().ok()) {
        Some(asked) if asked >= PORTS_COMES_BACK => answer,
        Some(asked) if asked >= PORTS_LIST_UDP => answer.as_v2(),
        _ => answer.as_v1(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shipyard_runtime::attach::forward::{PortsAnswer, Protocol, PublishedPort};

    /// Asked `3`, the answer as it is; `2`, without what comes back; asked
    /// nothing (an older host), or something else, v1.
    #[test]
    fn the_answer_is_at_the_version_asked() {
        let mut answer = PortsAnswer::new();
        answer.ports.push(PublishedPort {
            ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
            port: 53,
            protocol: Protocol::Udp,
        });
        answer.comes_back = Some(2);
        assert_eq!(at_version(answer.clone(), Some("3")), answer);
        let v2 = at_version(answer.clone(), Some("2"));
        assert_eq!((v2.version, v2.comes_back, v2.ports.len()), (2, None, 1));
        // What a v2 reader (a 13.6b1 host, `deny_unknown_fields`) reads.
        let said = String::from_utf8(v2.encode().unwrap()).unwrap();
        assert!(!said.contains("comes_back"), "{said}");
        for older in [None, Some("1"), Some("x")] {
            let v1 = at_version(answer.clone(), older);
            assert_eq!((v1.version, v1.udp, v1.ports.len()), (1, 1, 0), "{older:?}");
        }
    }
}
