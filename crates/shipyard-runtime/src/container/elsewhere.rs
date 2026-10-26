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
pub fn stop(
    _root: &Path,
    _id: &str,
    _signal: Signal,
    _timeout: std::time::Duration,
) -> Result<bool> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn wait(_root: &Path, _id: &str) -> Result<Option<i32>> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn hold(_root: &Path, _id: &str, _label: &str, _fd: std::os::fd::BorrowedFd<'_>) -> Result<()> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn release(_root: &Path, _id: &str, _label: &str) -> Result<()> {
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

/// A created container's init, for a caller that follows it: Linux only.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn init_process(_root: &Path, _id: &str) -> Result<super::InitProcess> {
    Err(ContainerError::LinuxOnly)
}

/// The network namespace of a created container's init: Linux only.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn network_namespace(_root: &Path, _id: &str) -> Result<std::os::fd::OwnedFd> {
    Err(ContainerError::LinuxOnly)
}

/// The network namespace of a running container's init: Linux only.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn network_namespace_live(_root: &Path, _id: &str) -> Result<std::os::fd::OwnedFd> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn exit_record(_root: &Path, _id: &str) -> Result<Option<super::ExitRecord>> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn status(_root: &Path, _id: &str) -> Result<Option<super::Status>> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn peek_status(_root: &Path, _id: &str) -> Result<Option<super::Status>> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn peek_init(_root: &Path, _id: &str) -> Result<Option<(i32, u64)>> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn peek_monitor(_root: &Path, _id: &str) -> Result<Option<(i32, u64)>> {
    Err(ContainerError::LinuxOnly)
}

/// No `/proc` to read a start time from: none.
#[must_use]
pub fn process_started(_pid: i32) -> Option<u64> {
    None
}

/// No containers, so no process is a container's: false.
#[must_use]
pub fn is_process(_pid: i32, _started: u64) -> bool {
    false
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn create_planned(
    _root: &Path,
    _id: &str,
    _plan: &super::Plan,
    _options: &super::CreateOptions<'_>,
) -> Result<super::Created> {
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

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn start_helper_alone(
    _root: &Path,
    _id: &str,
    _named: (&str, &'static str),
    _args: &[u8],
    _streams: (&std::os::fd::OwnedFd, &std::os::fd::OwnedFd),
) -> Result<super::RunningHelper> {
    Err(ContainerError::LinuxOnly)
}

/// Containers need Linux.
///
/// # Errors
///
/// Always [`ContainerError::LinuxOnly`].
pub fn start_helper_in_view(
    _view: &super::RootView,
    _name: &str,
    _args: &[u8],
    _stdin: &std::os::fd::OwnedFd,
    _stdout: &std::os::fd::OwnedFd,
) -> Result<super::RunningHelper> {
    Err(ContainerError::LinuxOnly)
}
