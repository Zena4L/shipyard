//! Away from Linux there are no containers: every call says so.

use std::os::fd::BorrowedFd;
use std::path::Path;

use super::{ContainerError, Created, Result, State};
use crate::process::Signal;

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn create(
    _root: &Path,
    _id: &str,
    _bundle: &Path,
    _console: Option<BorrowedFd<'_>>,
) -> Result<Created> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn start(_root: &Path, _id: &str) -> Result<Vec<String>> {
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
pub fn kill_all(_root: &Path, _id: &str, _signal: Signal) -> Result<()> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn delete(_root: &Path, _id: &str, _force: bool) -> Result<Vec<String>> {
    Err(ContainerError::LinuxOnly)
}

/// Nothing to do: there are no monitors without containers.
pub fn monitor_entry_point() {}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn pause(_root: &Path, _id: &str) -> Result<()> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn resume(_root: &Path, _id: &str) -> Result<()> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn update(
    _root: &Path,
    _id: &str,
    _resources: &crate::resources::Resources,
) -> Result<Vec<String>> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn ps(_root: &Path, _id: &str) -> Result<Vec<i32>> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn stats(_root: &Path, _id: &str) -> Result<(crate::cgroup::Stats, u64)> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn memory_limit(_root: &Path, _id: &str) -> Result<Option<u64>> {
    Err(ContainerError::LinuxOnly)
}
