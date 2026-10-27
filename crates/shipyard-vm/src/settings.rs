//! The VM's size (step 13.2, D13.2-1): its memory and CPUs. A default is
//! made from the Mac's RAM and cores ([`defaults`]); `shipyard vm set` keeps
//! others in the VM directory's `vm.json` ([`VmSettings`], v1), which the
//! host takes at its next start ([`resolve`]). The memory isn't given back
//! to macOS while the VM runs (no balloon: 13.1 measured that macOS takes
//! nothing back from one), so the default is a quarter of the Mac's.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::VmError;

/// The newest `vm.json` this Shipyard reads and the one it writes.
const SETTINGS_VERSION: u32 = 1;

/// The most a `vm.json` is read of: it holds a few dozen bytes.
const SETTINGS_CAP: u64 = 4096;

/// One MiB.
const MIB: u64 = 1 << 20;

/// The default's least and most memory, in MiB.
const DEFAULT_MEMORY_MIB: (u64, u64) = (4 << 10, 16 << 10);

/// The least memory a VM may be given, in MiB: its guest and a few
/// containers.
const LEAST_MEMORY_MIB: u64 = 2 << 10;

/// What the Mac keeps for itself at the most memory a VM may be given, in
/// MiB.
const MAC_KEEPS_MIB: u64 = 2 << 10;

/// The default's most CPUs.
const DEFAULT_CPUS: u32 = 8;

/// What `vm.json` holds: the size `vm set` asked for, each part left to
/// the default when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSettings {
    /// Its version (H2).
    pub version: u32,
    /// The memory, in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    /// The CPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
}

/// What a Mac has, for its VM's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mac {
    /// Its RAM, in MiB.
    pub ram_mib: u64,
    /// Its cores.
    pub cores: u32,
    /// The most CPUs Virtualization.framework lets a VM have.
    pub max_cpus: u32,
}

/// Where a part of the size came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Made from the Mac's.
    Default,
    /// `vm.json` (`vm set`).
    Set,
}

/// The VM's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Its memory, in MiB, and where that came from.
    pub memory_mib: (u64, Source),
    /// Its CPUs, and where they came from.
    pub cpus: (u32, Source),
}

/// The default size for `mac`: a quarter of its RAM, from 4 GiB to 16 GiB
/// (never past half of it); half its cores, from 1 to 8 (never past the
/// framework's most).
#[must_use]
pub fn defaults(mac: Mac) -> (u64, u32) {
    let memory = (mac.ram_mib / 4)
        .clamp(DEFAULT_MEMORY_MIB.0, DEFAULT_MEMORY_MIB.1)
        .min(mac.ram_mib / 2);
    let cpus = (mac.cores / 2)
        .clamp(1, DEFAULT_CPUS)
        .min(mac.max_cpus.max(1));
    (memory, cpus)
}

/// The memory and CPUs `mac` may give a VM: from 2 GiB to its RAM less
/// 2 GiB; from 1 CPU to the lesser of its cores and the framework's most.
#[must_use]
pub fn bounds(mac: Mac) -> ((u64, u64), (u32, u32)) {
    let most_memory = mac
        .ram_mib
        .saturating_sub(MAC_KEEPS_MIB)
        .max(LEAST_MEMORY_MIB);
    let most_cpus = mac.cores.min(mac.max_cpus).max(1);
    ((LEAST_MEMORY_MIB, most_memory), (1, most_cpus))
}

/// The size a VM on `mac` starts with: what `settings` asks for (kept
/// within [`bounds`]: a `vm.json` copied from a bigger Mac is made to fit),
/// else the default.
#[must_use]
pub fn resolve(settings: Option<&VmSettings>, mac: Mac) -> Size {
    let (memory, cpus) = defaults(mac);
    let ((least, most), (fewest, most_cpus)) = bounds(mac);
    let memory_mib = settings
        .and_then(|settings| settings.memory_mib)
        .map_or((memory, Source::Default), |mib| {
            (mib.clamp(least, most), Source::Set)
        });
    let cpus = settings
        .and_then(|settings| settings.cpus)
        .map_or((cpus, Source::Default), |set| {
            (set.clamp(fewest, most_cpus), Source::Set)
        });
    Size { memory_mib, cpus }
}

/// What `vm set` changes: each part a new value, the default again, or
/// left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change<T> {
    /// Left as it is.
    Keep,
    /// The default again.
    Default,
    /// This.
    To(T),
}

impl VmSettings {
    /// `vm.json` at `path`: `None` if there's none.
    ///
    /// # Errors
    ///
    /// [`VmError::Io`]: unreadable, not this format, or a newer version.
    pub fn read(path: &Path) -> Result<Option<Self>, VmError> {
        crate::state_file::read(
            path,
            SETTINGS_CAP,
            SETTINGS_VERSION,
            " (the default size is used: `shipyard vm set` writes a new one)",
        )
    }

    /// Applies `memory` (in bytes) and `cpus` to `vm.json` at `path`, each
    /// checked against `mac`'s [`bounds`]: what it now holds.
    ///
    /// A `vm.json` that can't be read (corrupt, or a newer Shipyard's) is
    /// replaced: what's asked now is all it holds.
    ///
    /// # Errors
    ///
    /// [`VmError::Size`] for a size out of bounds (said with them);
    /// [`VmError::Io`] if `vm.json` can't be written.
    pub fn change(
        path: &Path,
        mac: Mac,
        memory: Change<u64>,
        cpus: Change<u32>,
    ) -> Result<Self, VmError> {
        let mut settings = Self::read(path).ok().flatten().unwrap_or_default();
        settings.version = SETTINGS_VERSION;
        let ((least, most), (fewest, most_cpus)) = bounds(mac);
        match memory {
            Change::Keep => {}
            Change::Default => settings.memory_mib = None,
            Change::To(bytes) => {
                let mib = bytes / MIB;
                if !(least..=most).contains(&mib) {
                    return Err(VmError::Size(format!(
                        "a VM on this Mac takes from {} to {} of memory (it keeps {} for \
                         itself)",
                        shown(least),
                        shown(most),
                        shown(MAC_KEEPS_MIB)
                    )));
                }
                settings.memory_mib = Some(mib);
            }
        }
        match cpus {
            Change::Keep => {}
            Change::Default => settings.cpus = None,
            Change::To(count) => {
                if !(fewest..=most_cpus).contains(&count) {
                    return Err(VmError::Size(format!(
                        "a VM on this Mac takes from {fewest} to {most_cpus} CPUs"
                    )));
                }
                settings.cpus = Some(count);
            }
        }
        crate::state_file::write(path, &settings, shipyard_sys::fs::Durability::Synced)?;
        Ok(settings)
    }
}

/// A size in MiB as a person reads it: whole GiB as `6 GiB`, else MiB.
#[must_use]
pub fn shown(mib: u64) -> String {
    if mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

/// This Mac's RAM, cores and the framework's most CPUs.
///
/// # Errors
///
/// [`VmError::Unsupported`] if the RAM can't be read.
#[cfg(target_os = "macos")]
pub fn this_mac() -> Result<Mac, VmError> {
    let ram = crate::sysctl::u64_named(c"hw.memsize").ok_or_else(|| {
        VmError::Unsupported("this Mac's memory can't be read (hw.memsize)".to_owned())
    })?;
    let cores = std::thread::available_parallelism()
        .map_or(2, |n| u32::try_from(n.get()).unwrap_or(u32::MAX));
    Ok(Mac {
        ram_mib: ram / MIB,
        cores,
        max_cpus: crate::machine::max_cpus(),
    })
}

#[cfg(test)]
mod tests;
