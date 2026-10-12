//! Away from Linux there are no containers: every call says so.

use std::path::Path;

use super::{ContainerError, CreateOptions, Created, LogsOf, Result, State};
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
    _options: &CreateOptions<'_>,
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
pub fn logs_of(_root: &Path, _id: &str) -> Result<LogsOf> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux: there are no logs to remove.
#[must_use]
pub fn remove_failed_logs(_logs: &LogsOf) -> Option<String> {
    None
}

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

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn attach_stream(_root: &Path, _id: &str) -> Result<std::os::unix::net::UnixStream> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn exec(
    _root: &Path,
    _id: &str,
    _options: &super::ExecOptions,
    _passed: Option<crate::process::passed::PassedFds>,
) -> Result<super::ExecOutcome> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn exit_code(_root: &Path, _id: &str) -> Result<Option<i32>> {
    Err(ContainerError::LinuxOnly)
}

/// Helpers need Linux: nothing to run.
pub fn helper_entry_point(_helpers: &[super::Helper]) {}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn start_helper(
    _root: &Path,
    _id: &str,
    _name: &str,
    _action: &'static str,
    _args: &[u8],
    _stdin: &std::os::fd::OwnedFd,
    _stdout: &std::os::fd::OwnedFd,
) -> Result<super::RunningHelper> {
    Err(ContainerError::LinuxOnly)
}
