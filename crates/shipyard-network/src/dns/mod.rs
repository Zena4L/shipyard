//! A network's DNS server: containers on a network made with `shipyard
//! network create` find each other by name.
//!
//! Each such network has one server, `shipyard __shipyard-dns`, on its
//! gateway's port 53 (UDP and TCP), which the containers' `resolv.conf`
//! names. It answers the names of the network's live containers (their
//! `--name`, their `--network-alias`es and their run's ID) and the names for
//! their addresses, from the network's leases, read for each query; anything
//! else goes to the host's own resolvers. It's started by the first run that
//! needs it, gives up root once its sockets are bound, and exits once the
//! network has had no container for a while, or is removed.
//!
//! [`message`] reads and writes DNS messages and [`answer`] decides what to
//! say; both build and are tested on any UNIX. The server itself is Linux's.

pub mod answer;
pub mod message;
#[cfg(target_os = "linux")]
pub mod server;

/// The longest container name: a DNS label's.
const MAX_NAME: usize = 63;

/// Checks a container's name on a network (`--name`, `--network-alias`): a
/// DNS label, so the network's server can answer it: letters, digits, `_`
/// and `-`, starting with a letter or digit, at most 63 characters.
pub fn parse_name(text: &str) -> Result<String, String> {
    let valid = !text.is_empty()
        && text.len() <= MAX_NAME
        && text
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    if valid {
        Ok(text.to_owned())
    } else {
        Err(format!(
            "{text:?} can't be a container's name: letters, digits, _ and -, starting with a \
             letter or digit, at most 63 characters"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_dns_labels() {
        for good in ["db", "web-1", "a_b", "0", &"x".repeat(63)] {
            assert_eq!(parse_name(good).as_deref(), Ok(good), "{good}");
        }
        for bad in ["", "-db", "_db", "db.app", "d b", "é", &"x".repeat(64)] {
            assert!(parse_name(bad).is_err(), "{bad}");
        }
    }
}
