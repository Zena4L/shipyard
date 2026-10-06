//! Away from Linux there are no containers: every call says so.

use std::path::Path;

use super::{ContainerError, Created, Result, State};
use crate::process::Signal;

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn create(_root: &Path, _id: &str, _bundle: &Path) -> Result<Created> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn start(_root: &Path, _id: &str) -> Result<()> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn state(_root: &Path, _id: &str) -> Result<State> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn kill(_root: &Path, _id: &str, _signal: Signal) -> Result<()> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn delete(_root: &Path, _id: &str, _force: bool) -> Result<()> {
    Err(ContainerError::LinuxOnly)
}

/// Nothing to do: there are no monitors without containers.
pub fn monitor_entry_point() {}
