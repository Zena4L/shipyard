//! The VM's network card's own address, kept in its directory
//! ([`NetworkFile`], `network.json`). Virtualization.framework gives a card
//! with none a new random one at each boot, and the Mac's NAT network
//! (`bootpd`) leases each address for an hour, out of 253: VMs booted
//! often (the VM tests boot 18, and each idle stop is a boot after) used
//! them all up, and no VM on the Mac got a lease (11.5a, found while
//! measuring F132: ~400 boots in a few hours). Made at the first boot, the same
//! one ever after, as a real machine's.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::VmError;

/// The newest `network.json` this Shipyard reads and the one it writes.
const NETWORK_FILE_VERSION: u32 = 1;

/// The most a `network.json` is read of: it holds a few dozen bytes.
const NETWORK_FILE_CAP: u64 = 4096;

/// What `network.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetworkFile {
    /// Its version (H2).
    pub(crate) version: u32,
    /// The card's address: six hex pairs, locally administered unicast.
    pub(crate) mac: Mac,
}

/// A card's address: always unicast and locally administered (the
/// framework's own random ones are too), so it's no maker's and no group's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Mac([u8; 6]);

impl Mac {
    /// `bytes` made a locally administered unicast address.
    #[must_use]
    pub(crate) fn local(mut bytes: [u8; 6]) -> Self {
        bytes[0] = (bytes[0] | 0x02) & !0x01;
        Self(bytes)
    }
}

impl std::fmt::Display for Mac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl From<Mac> for String {
    fn from(mac: Mac) -> Self {
        mac.to_string()
    }
}

impl TryFrom<String> for Mac {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let mut bytes = [0u8; 6];
        let mut octets = text.split(':');
        for byte in &mut bytes {
            *byte = octets
                .next()
                .filter(|octet| octet.len() == 2 && octet.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|octet| u8::from_str_radix(octet, 16).ok())
                .ok_or_else(|| format!("{text:?} isn't a MAC address"))?;
        }
        if octets.next().is_some() {
            return Err(format!("{text:?} isn't a MAC address"));
        }
        let mac = Self(bytes);
        if mac != Self::local(bytes) {
            return Err(format!(
                "{text:?} isn't a locally administered unicast address"
            ));
        }
        Ok(mac)
    }
}

impl NetworkFile {
    /// The card's address kept at `path`, or a new random one written
    /// there first if there's none, or only an empty file (a crash before
    /// the first write reached the disk). Called by the VM's host alone,
    /// under its lock: one writer.
    ///
    /// # Errors
    ///
    /// [`VmError::Io`]: unreadable, not this format or a newer version
    /// (each says removing it makes a new address), or no randomness to
    /// make one with.
    pub(crate) fn kept_or_made(path: &Path) -> Result<Mac, VmError> {
        if let Some(kept) = Self::read(path)? {
            return Ok(kept.mac);
        }
        let bytes =
            shipyard_sys::random::bytes().map_err(|e| VmError::io(Path::new("/dev/urandom"), e))?;
        let made = Self {
            version: NETWORK_FILE_VERSION,
            mac: Mac::local(bytes),
        };
        made.write(path)?;
        Ok(made.mac)
    }

    /// Writes it at `path`, whole and synced: it's written once, and a
    /// crash must leave it whole or absent.
    fn write(&self, path: &Path) -> Result<(), VmError> {
        crate::state_file::write(path, self, shipyard_sys::fs::Durability::Synced)
    }

    /// Reads it from `path`: `None` if there's none, or it's empty.
    fn read(path: &Path) -> Result<Option<Self>, VmError> {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() == 0) {
            return Ok(None);
        }
        crate::state_file::read(
            path,
            NETWORK_FILE_CAP,
            NETWORK_FILE_VERSION,
            " (remove it: a new address is made)",
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("shipyard-vm-network-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("network.json")
    }

    fn remove(path: &Path) {
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The address is made once and the same at every boot after: each new
    /// one would take another of the NAT network's 253 leases for an hour.
    #[test]
    fn the_address_is_made_once_and_kept() {
        let path = scratch("kept");
        let first = NetworkFile::kept_or_made(&path).unwrap();
        assert_eq!(NetworkFile::kept_or_made(&path).unwrap(), first);
        assert_eq!(
            NetworkFile::read(&path).unwrap().unwrap(),
            NetworkFile {
                version: 1,
                mac: first
            }
        );
        // A second directory's is another.
        let other_path = scratch("other");
        let other = NetworkFile::kept_or_made(&other_path).unwrap();
        assert_ne!(other, first);
        remove(&path);
        remove(&other_path);
    }

    /// Whatever the random bytes, the address is unicast and locally
    /// administered: never a maker's, never a group's.
    #[test]
    fn made_addresses_are_local_unicast() {
        assert_eq!(Mac::local([0xff; 6]).to_string(), "fe:ff:ff:ff:ff:ff");
        assert_eq!(Mac::local([0; 6]).to_string(), "02:00:00:00:00:00");
        let path = scratch("local");
        let made = NetworkFile::kept_or_made(&path).unwrap();
        assert_eq!(made.0[0] & 0x03, 0x02);
        remove(&path);
    }

    /// A file that names another kind of address, or isn't one, or is
    /// newer, is refused, never taken or replaced.
    #[test]
    fn a_wrong_file_is_refused() {
        for text in [
            r#"{"version":1,"mac":"00:11:22:33:44:55"}"#, // a maker's
            r#"{"version":1,"mac":"03:11:22:33:44:55"}"#, // a group's
            r#"{"version":1,"mac":"02:11:22:33:44"}"#,
            r#"{"version":1,"mac":"02:11:22:33:44:55:66"}"#,
            r#"{"version":1,"mac":"02:11:22:33:44:5"}"#,
            r#"{"version":1,"mac":"+2:11:22:33:44:55"}"#,
            r#"{"version":2,"mac":"02:11:22:33:44:55"}"#,
            "not json",
        ] {
            let path = scratch("wrong");
            std::fs::write(&path, text).unwrap();
            assert!(
                matches!(NetworkFile::kept_or_made(&path), Err(VmError::Io { .. })),
                "{text}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
            remove(&path);
        }
        let path = scratch("right");
        std::fs::write(&path, r#"{"version":1,"mac":"02:11:22:33:44:55"}"#).unwrap();
        assert_eq!(
            NetworkFile::kept_or_made(&path).unwrap().to_string(),
            "02:11:22:33:44:55"
        );
        remove(&path);
    }

    /// An empty file (a crash before the first write reached the disk) is
    /// none: a new address is made, not the VM refused for good.
    #[test]
    fn an_empty_file_makes_a_new_address() {
        let path = scratch("empty");
        std::fs::write(&path, "").unwrap();
        let made = NetworkFile::kept_or_made(&path).unwrap();
        assert_eq!(NetworkFile::read(&path).unwrap().unwrap().mac, made);
        remove(&path);
    }
}
