//! A seccomp profile, as JSON: Docker's format, which is OCI's
//! `linux.seccomp` plus conditions on a rule (`includes` and `excludes`:
//! capabilities, architectures, a kernel version) and `archMap`.
//!
//! Unknown fields are refused, not ignored: a field Shipyard doesn't know
//! might be a restriction, and a container must never be less confined than
//! its profile says. A bundle's are the exception ([`Profile::from_oci`]):
//! the OCI spec says to ignore them, so they're dropped and named.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::syscall_known;
use crate::security::{Capability, CapabilityError};
use crate::small_file::{SmallFileError, read_small_file};

/// A seccomp profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct Profile {
    /// What a call no rule matches gets.
    pub default_action: Action,
    /// The errno for `defaultAction: SCMP_ACT_ERRNO` (EPERM if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_errno_ret: Option<u32>,
    /// Other architectures the profile is written for (`SCMP_ARCH_X86`, …).
    /// Only the native one runs under a filter: any other is killed.
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub architectures: Vec<String>,
    /// Docker's per-architecture form of `architectures`; same meaning.
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub arch_map: Vec<ArchMap>,
    /// `SECCOMP_FILTER_FLAG_*` names.
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub flags: Vec<String>,
    /// A listener for `SCMP_ACT_NOTIFY`: refused (no listener).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener_path: Option<String>,
    /// Data for that listener: refused with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener_metadata: Option<String>,
    /// The rules.
    #[serde(default, deserialize_with = "nullable")]
    pub syscalls: Vec<Rule>,
}

/// One entry of Docker's `archMap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct ArchMap {
    /// The architecture (`SCMP_ARCH_X86_64`).
    pub architecture: String,
    /// The ones that run on it (`SCMP_ARCH_X86`, `SCMP_ARCH_X32`).
    #[serde(default, deserialize_with = "nullable")]
    pub sub_architectures: Vec<String>,
}

/// What a rule applies to its calls, if its arguments match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct Rule {
    /// The calls, by name.
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub names: Vec<String>,
    /// One call (Docker's older spelling of `names`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The verdict.
    pub action: Action,
    /// The errno for `SCMP_ACT_ERRNO` (EPERM if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errno_ret: Option<u32>,
    /// Conditions on the arguments, all of which must hold.
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub args: Vec<Arg>,
    /// A note for the reader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// The rule applies only if all of these hold (Docker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub includes: Option<Condition>,
    /// The rule doesn't apply if any of these holds (Docker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excludes: Option<Condition>,
}

impl Rule {
    /// The calls it names.
    pub(crate) fn all_names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().chain(&self.name).map(String::as_str)
    }
}

/// A condition on one argument, compared as a 64-bit number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct Arg {
    /// Which argument, 0 to 5.
    pub index: u32,
    /// The value compared with (`SCMP_CMP_MASKED_EQ`: the mask).
    pub value: u64,
    /// `SCMP_CMP_MASKED_EQ` only: what the masked argument must equal.
    #[serde(default)]
    pub value_two: u64,
    /// How.
    pub op: Operator,
}

/// A Docker condition on a rule: the container's bounding capabilities, the
/// architecture, the kernel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct Condition {
    /// Capabilities (`CAP_SYS_ADMIN`).
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub caps: Vec<String>,
    /// Architectures, as Docker names them (`amd64`, `arm64`, …).
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub arches: Vec<String>,
    /// A kernel version, `MAJOR.MINOR` (`4.8`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_kernel: Option<String>,
}

/// A verdict on a system call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Action {
    /// Let it run.
    #[serde(rename = "SCMP_ACT_ALLOW")]
    Allow,
    /// Fail it with an errno, without running it.
    #[serde(rename = "SCMP_ACT_ERRNO")]
    Errno,
    /// Let it run, and log it (the kernel's audit log).
    #[serde(rename = "SCMP_ACT_LOG")]
    Log,
    /// Send the thread SIGSYS, which may catch it.
    #[serde(rename = "SCMP_ACT_TRAP")]
    Trap,
    /// Kill the thread that made it (libseccomp's meaning).
    #[serde(rename = "SCMP_ACT_KILL")]
    Kill,
    /// Kill the thread that made it.
    #[serde(rename = "SCMP_ACT_KILL_THREAD")]
    KillThread,
    /// Kill the whole process.
    #[serde(rename = "SCMP_ACT_KILL_PROCESS")]
    KillProcess,
    /// Hand it to a tracer: refused (there's none).
    #[serde(rename = "SCMP_ACT_TRACE")]
    Trace,
    /// Hand it to a listener: refused (there's none).
    #[serde(rename = "SCMP_ACT_NOTIFY")]
    Notify,
}

impl Action {
    /// Its name in a profile.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Allow => "SCMP_ACT_ALLOW",
            Self::Errno => "SCMP_ACT_ERRNO",
            Self::Log => "SCMP_ACT_LOG",
            Self::Trap => "SCMP_ACT_TRAP",
            Self::Kill => "SCMP_ACT_KILL",
            Self::KillThread => "SCMP_ACT_KILL_THREAD",
            Self::KillProcess => "SCMP_ACT_KILL_PROCESS",
            Self::Trace => "SCMP_ACT_TRACE",
            Self::Notify => "SCMP_ACT_NOTIFY",
        }
    }
}

/// How an argument is compared with a rule's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Operator {
    /// Not equal.
    #[serde(rename = "SCMP_CMP_NE")]
    NotEqual,
    /// Less than.
    #[serde(rename = "SCMP_CMP_LT")]
    Less,
    /// Less than or equal.
    #[serde(rename = "SCMP_CMP_LE")]
    LessOrEqual,
    /// Equal.
    #[serde(rename = "SCMP_CMP_EQ")]
    Equal,
    /// Greater than or equal.
    #[serde(rename = "SCMP_CMP_GE")]
    GreaterOrEqual,
    /// Greater than.
    #[serde(rename = "SCMP_CMP_GT")]
    Greater,
    /// `argument & value == valueTwo`.
    #[serde(rename = "SCMP_CMP_MASKED_EQ")]
    MaskedEqual,
}

/// The flags a profile may ask for, and their bits
/// (`SECCOMP_FILTER_FLAG_*`).
const FLAGS: [(&str, u32); 3] = [
    // The command is single-threaded when the filter goes on (it applies to
    // every thread anyway): nothing to synchronize.
    ("SECCOMP_FILTER_FLAG_TSYNC", 0),
    ("SECCOMP_FILTER_FLAG_LOG", 1 << 1),
    ("SECCOMP_FILTER_FLAG_SPEC_ALLOW", 1 << 2),
];

/// The largest profile file read.
const MAX_PROFILE_SIZE: u64 = 1024 * 1024;

/// The largest errno the kernel passes on (`MAX_ERRNO`).
const MAX_ERRNO: u32 = 4095;

/// The fields Shipyard reads, at each level of a profile (what
/// [`Profile::from_oci`] keeps).
const PROFILE_FIELDS: &[&str] = &[
    "defaultAction",
    "defaultErrnoRet",
    "architectures",
    "archMap",
    "flags",
    "listenerPath",
    "listenerMetadata",
    "syscalls",
];
const RULE_FIELDS: &[&str] = &[
    "names", "name", "action", "errnoRet", "args", "comment", "includes", "excludes",
];
const ARG_FIELDS: &[&str] = &["index", "value", "valueTwo", "op"];
const CONDITION_FIELDS: &[&str] = &["caps", "arches", "minKernel"];
const ARCH_MAP_FIELDS: &[&str] = &["architecture", "subArchitectures"];

/// Why a profile can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SeccompError {
    /// A name no Linux architecture has.
    #[error("\"{0}\" isn't a system call (on any architecture)")]
    UnknownSystemCall(String),
    /// An action Shipyard doesn't apply.
    #[error("{action} isn't supported ({why})")]
    UnsupportedAction {
        /// Its name.
        action: &'static str,
        /// Why not.
        why: &'static str,
    },
    /// An argument past the sixth.
    #[error("argument index {0} is out of range (a system call has 6, 0 to 5)")]
    ArgumentIndex(u32),
    /// An errno the kernel can't return.
    #[error("errno {0} is out of range (0 to 4095)")]
    Errno(u32),
    /// A flag Shipyard doesn't know or apply.
    #[error("flag \"{0}\" isn't supported (known: {known})", known = known_flags())]
    Flag(String),
    /// `listenerPath`: there's no listener.
    #[error("listenerPath isn't supported (there's no SCMP_ACT_NOTIFY)")]
    Listener,
    /// `name` and `names` together.
    #[error("both name and names are given; use names")]
    NameAndNames,
    /// `architectures` and `archMap` together.
    #[error("both architectures and archMap are given; use one")]
    ArchitecturesAndArchMap,
    /// A capability in a condition.
    #[error(transparent)]
    Capability(#[from] CapabilityError),
    /// A `minKernel` that isn't `MAJOR.MINOR`.
    #[error("minKernel \"{0}\" isn't a kernel version like 5.10")]
    MinKernel(String),
    /// A filter longer than the kernel takes.
    #[error("the filter is {0} instructions long, more than the kernel's 4096")]
    TooLong(usize),
    /// An architecture Shipyard has no system call table for.
    #[error("seccomp filters aren't supported on this architecture (only aarch64 and x86-64)")]
    Architecture,
    /// A profile file can't be read.
    #[error("{0}")]
    Read(String),
    /// A profile file isn't a regular file (a FIFO would block the read).
    #[error("it isn't a regular file")]
    NotAFile,
    /// A profile file is larger than 1 MiB.
    #[error("it's larger than 1 MiB")]
    TooLarge,
    /// A profile file isn't JSON of the right shape.
    #[error("{0}")]
    Json(String),
    /// The built-in profile can't be read (a bug).
    #[error("the built-in profile can't be read: {0}")]
    BuiltIn(String),
    /// Where in the profile.
    #[error("syscalls[{index}]: {error}")]
    InRule {
        /// The rule's index in `syscalls`.
        index: usize,
        /// What's wrong with it. (Not the error's `source()`: the message
        /// already says it, and a cause would be printed twice.)
        error: Box<SeccompError>,
    },
}

impl Profile {
    /// Reads a profile file (`--security-opt seccomp=FILE`): a regular file
    /// of at most 1 MiB (Docker's profile is 13 KiB), strictly (an unknown
    /// field is an error), and checks it ([`Profile::validate`]).
    ///
    /// # Errors
    ///
    /// It can't be read, isn't JSON of this shape, or isn't valid.
    pub fn from_file(path: &Path) -> Result<Self, SeccompError> {
        let text = read_small_file(path, MAX_PROFILE_SIZE).map_err(|err| match err {
            SmallFileError::Io(err) => SeccompError::Read(err.to_string()),
            SmallFileError::NotAFile => SeccompError::NotAFile,
            SmallFileError::TooLarge => SeccompError::TooLarge,
        })?;
        let profile: Self =
            serde_json::from_slice(&text).map_err(|err| SeccompError::Json(err.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Reads a bundle's `linux.seccomp`. Fields Shipyard doesn't know are
    /// dropped, as the OCI spec requires, and returned by name
    /// (`syscalls[2].foo`) so they can be reported.
    ///
    /// # Errors
    ///
    /// What's left isn't a profile.
    pub fn from_oci(mut value: Value) -> Result<(Self, Vec<String>), serde_json::Error> {
        let mut dropped = Vec::new();
        keep_only(&mut value, PROFILE_FIELDS, "", &mut dropped);
        if let Some(rules) = value.get_mut("syscalls").and_then(Value::as_array_mut) {
            for (index, rule) in rules.iter_mut().enumerate() {
                let at = format!("syscalls[{index}].");
                keep_only(rule, RULE_FIELDS, &at, &mut dropped);
                if let Some(args) = rule.get_mut("args").and_then(Value::as_array_mut) {
                    for (arg_index, arg) in args.iter_mut().enumerate() {
                        let at = format!("{at}args[{arg_index}].");
                        keep_only(arg, ARG_FIELDS, &at, &mut dropped);
                    }
                }
                for key in ["includes", "excludes"] {
                    if let Some(condition) = rule.get_mut(key) {
                        keep_only(
                            condition,
                            CONDITION_FIELDS,
                            &format!("{at}{key}."),
                            &mut dropped,
                        );
                    }
                }
            }
        }
        if let Some(maps) = value.get_mut("archMap").and_then(Value::as_array_mut) {
            for (index, map) in maps.iter_mut().enumerate() {
                keep_only(
                    map,
                    ARCH_MAP_FIELDS,
                    &format!("archMap[{index}]."),
                    &mut dropped,
                );
            }
        }
        Ok((serde_json::from_value(value)?, dropped))
    }

    /// Checks what can be checked without the container: names, actions,
    /// argument indexes, errnos, flags and conditions. A name another
    /// architecture has is fine (skipped here); one none has is a mistake.
    ///
    /// # Errors
    ///
    /// The first problem, with the rule it's in.
    pub fn validate(&self) -> Result<(), SeccompError> {
        check_action(self.default_action, self.default_errno_ret)?;
        if self.listener_path.is_some() || self.listener_metadata.is_some() {
            return Err(SeccompError::Listener);
        }
        if !self.architectures.is_empty() && !self.arch_map.is_empty() {
            return Err(SeccompError::ArchitecturesAndArchMap);
        }
        self.flag_bits()?;
        for (index, rule) in self.syscalls.iter().enumerate() {
            check_rule(rule).map_err(|error| SeccompError::InRule {
                index,
                error: Box::new(error),
            })?;
        }
        Ok(())
    }

    /// The `SECCOMP_FILTER_FLAG_*` bits its flags ask for.
    pub(crate) fn flag_bits(&self) -> Result<u32, SeccompError> {
        self.flags.iter().try_fold(0, |bits, flag| {
            FLAGS
                .iter()
                .find(|(name, _)| name == flag)
                .map(|&(_, bit)| bits | bit)
                .ok_or_else(|| SeccompError::Flag(flag.clone()))
        })
    }
}

fn check_rule(rule: &Rule) -> Result<(), SeccompError> {
    if rule.name.is_some() && !rule.names.is_empty() {
        return Err(SeccompError::NameAndNames);
    }
    if let Some(unknown) = rule.all_names().find(|name| !syscall_known(name)) {
        return Err(SeccompError::UnknownSystemCall(unknown.to_owned()));
    }
    check_action(rule.action, rule.errno_ret)?;
    if let Some(arg) = rule.args.iter().find(|arg| arg.index > 5) {
        return Err(SeccompError::ArgumentIndex(arg.index));
    }
    for condition in [&rule.includes, &rule.excludes].into_iter().flatten() {
        for cap in &condition.caps {
            cap.parse::<Capability>()?;
        }
        if let Some(version) = &condition.min_kernel {
            crate::kernel::version(version)
                .ok_or_else(|| SeccompError::MinKernel(version.clone()))?;
        }
    }
    Ok(())
}

pub(super) fn check_action(action: Action, errno: Option<u32>) -> Result<(), SeccompError> {
    match action {
        Action::Trace => Err(SeccompError::UnsupportedAction {
            action: action.name(),
            why: "there's no tracer",
        }),
        Action::Notify => Err(SeccompError::UnsupportedAction {
            action: action.name(),
            why: "there's no listener",
        }),
        _ => match errno {
            Some(errno) if errno > MAX_ERRNO => Err(SeccompError::Errno(errno)),
            _ => Ok(()),
        },
    }
}

/// Removes from `value` (an object) every field not in `known`, adding each
/// one's name, after `at`, to `dropped`.
fn keep_only(value: &mut Value, known: &[&str], at: &str, dropped: &mut Vec<String>) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| {
            let keep = known.contains(&key.as_str());
            if !keep {
                dropped.push(format!("{at}{key}"));
            }
            keep
        });
    }
}

/// A list that may be `null`, as Go writes an empty one (Docker's own
/// profile has `"subArchitectures": null`): empty.
fn nullable<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

fn known_flags() -> String {
    FLAGS.map(|(name, _)| name).join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(json: &str) -> Result<Profile, String> {
        let profile: Profile = serde_json::from_str(json).map_err(|err| err.to_string())?;
        profile.validate().map_err(|err| err.to_string())?;
        Ok(profile)
    }

    fn rule(json: &str) -> Result<Profile, String> {
        profile(&format!(
            r#"{{"defaultAction": "SCMP_ACT_ERRNO", "syscalls": [{json}]}}"#
        ))
    }

    #[test]
    fn a_minimal_profile_reads() {
        let p = profile(r#"{"defaultAction": "SCMP_ACT_ALLOW"}"#).unwrap();
        assert_eq!(p.default_action, Action::Allow);
        assert!(p.syscalls.is_empty());
    }

    #[test]
    fn an_oci_rule_reads() {
        let p = rule(
            r#"{"names": ["personality"], "action": "SCMP_ACT_ALLOW",
                "args": [{"index": 0, "value": 8, "op": "SCMP_CMP_EQ"}]}"#,
        )
        .unwrap();
        let arg = p.syscalls[0].args[0];
        assert_eq!((arg.index, arg.value, arg.value_two), (0, 8, 0));
        assert_eq!(arg.op, Operator::Equal);
    }

    #[test]
    fn names_other_architectures_have_are_fine_and_unknown_ones_arent() {
        rule(r#"{"names": ["arm_fadvise64_64", "arch_prctl"], "action": "SCMP_ACT_ALLOW"}"#)
            .unwrap();
        let err = rule(r#"{"names": ["read", "wirte"], "action": "SCMP_ACT_ALLOW"}"#).unwrap_err();
        assert_eq!(
            err,
            "syscalls[0]: \"wirte\" isn't a system call (on any architecture)"
        );
        let err = rule(r#"{"name": "opne", "action": "SCMP_ACT_ALLOW"}"#).unwrap_err();
        assert!(err.contains("\"opne\""), "{err}");
    }

    /// The rule's error is in the message, not also its cause: a caller
    /// printing the whole chain (the CLI does) would say it twice.
    #[test]
    fn a_rules_error_is_said_once() {
        let err = SeccompError::InRule {
            index: 0,
            error: Box::new(SeccompError::UnknownSystemCall("wirte".into())),
        };
        assert!(std::error::Error::source(&err).is_none());
        assert_eq!(err.to_string().matches("wirte").count(), 1);
    }

    #[test]
    fn trace_and_notify_are_refused() {
        let err = rule(r#"{"names": ["read"], "action": "SCMP_ACT_NOTIFY"}"#).unwrap_err();
        assert_eq!(
            err,
            "syscalls[0]: SCMP_ACT_NOTIFY isn't supported (there's no listener)"
        );
        let err = profile(r#"{"defaultAction": "SCMP_ACT_TRACE"}"#).unwrap_err();
        assert_eq!(err, "SCMP_ACT_TRACE isn't supported (there's no tracer)");
        let err = profile(r#"{"defaultAction": "SCMP_ACT_ERRNO", "listenerPath": "/run/l"}"#)
            .unwrap_err();
        assert!(err.contains("listenerPath"), "{err}");
    }

    #[test]
    fn an_unknown_action_operator_or_field_is_an_error() {
        let err = profile(r#"{"defaultAction": "SCMP_ACT_MAYBE"}"#).unwrap_err();
        assert!(err.contains("SCMP_ACT_MAYBE"), "{err}");
        let err = rule(
            r#"{"names": ["read"], "action": "SCMP_ACT_ALLOW",
                "args": [{"index": 0, "value": 1, "op": "SCMP_CMP_ABOUT"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("SCMP_CMP_ABOUT"), "{err}");
        let err =
            profile(r#"{"defaultAction": "SCMP_ACT_ALLOW", "allowEverything": true}"#).unwrap_err();
        assert!(err.contains("allowEverything"), "{err}");
    }

    #[test]
    fn indexes_errnos_and_flags_are_checked() {
        let err = rule(
            r#"{"names": ["read"], "action": "SCMP_ACT_ALLOW",
                "args": [{"index": 6, "value": 1, "op": "SCMP_CMP_EQ"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("argument index 6"), "{err}");
        let err = rule(r#"{"names": ["read"], "action": "SCMP_ACT_ERRNO", "errnoRet": 4096}"#)
            .unwrap_err();
        assert!(err.contains("errno 4096"), "{err}");
        let p = profile(
            r#"{"defaultAction": "SCMP_ACT_ALLOW",
                "flags": ["SECCOMP_FILTER_FLAG_LOG", "SECCOMP_FILTER_FLAG_TSYNC"]}"#,
        )
        .unwrap();
        assert_eq!(p.flag_bits(), Ok(2));
        let err = profile(
            r#"{"defaultAction": "SCMP_ACT_ALLOW", "flags": ["SECCOMP_FILTER_FLAG_NEW_LISTENER"]}"#,
        )
        .unwrap_err();
        assert!(err.contains("SECCOMP_FILTER_FLAG_NEW_LISTENER"), "{err}");
    }

    #[test]
    fn conditions_are_checked() {
        rule(
            r#"{"names": ["bpf"], "action": "SCMP_ACT_ALLOW",
                "includes": {"caps": ["CAP_SYS_ADMIN"], "arches": ["amd64"], "minKernel": "4.8"}}"#,
        )
        .unwrap();
        let err = rule(
            r#"{"names": ["bpf"], "action": "SCMP_ACT_ALLOW", "includes": {"caps": ["CAP_ROOT"]}}"#,
        )
        .unwrap_err();
        assert!(err.contains("CAP_ROOT"), "{err}");
        let err = rule(
            r#"{"names": ["bpf"], "action": "SCMP_ACT_ALLOW", "excludes": {"minKernel": "four"}}"#,
        )
        .unwrap_err();
        assert!(err.contains("minKernel \"four\""), "{err}");
    }

    #[test]
    fn name_and_names_and_both_architecture_forms_are_refused() {
        let err = rule(r#"{"name": "read", "names": ["write"], "action": "SCMP_ACT_ALLOW"}"#)
            .unwrap_err();
        assert!(err.contains("both name and names"), "{err}");
        let err = profile(
            r#"{"defaultAction": "SCMP_ACT_ALLOW", "architectures": ["SCMP_ARCH_X86_64"],
                "archMap": [{"architecture": "SCMP_ARCH_X86_64", "subArchitectures": []}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("archMap"), "{err}");
    }

    #[test]
    fn null_lists_are_empty() {
        let p = profile(
            r#"{"defaultAction": "SCMP_ACT_ALLOW", "flags": null, "syscalls": [
                {"names": ["read"], "action": "SCMP_ACT_ALLOW", "args": null,
                 "includes": {"caps": null, "arches": null}}]}"#,
        )
        .unwrap();
        assert!(p.flags.is_empty() && p.syscalls[0].args.is_empty());
    }

    #[test]
    fn a_bundles_unknown_fields_are_dropped_and_named() {
        let value = serde_json::json!({
            "defaultAction": "SCMP_ACT_ERRNO",
            "future": 1,
            "syscalls": [
                {"names": ["read"], "action": "SCMP_ACT_ALLOW", "priority": 2,
                 "args": [{"index": 0, "value": 1, "op": "SCMP_CMP_EQ", "width": 32}],
                 "includes": {"caps": [], "hosts": []}}
            ],
            "archMap": [{"architecture": "SCMP_ARCH_AARCH64", "endian": "little"}]
        });
        let (profile, dropped) = Profile::from_oci(value).unwrap();
        profile.validate().unwrap();
        assert_eq!(
            dropped,
            [
                "future",
                "syscalls[0].priority",
                "syscalls[0].args[0].width",
                "syscalls[0].includes.hosts",
                "archMap[0].endian",
            ]
        );
    }

    /// The field lists `from_oci` keeps are exactly what the types read: a
    /// field added to one but not the other would be dropped, or refused.
    #[test]
    fn the_kept_fields_are_the_types_fields() {
        let full = serde_json::json!({
            "defaultAction": "SCMP_ACT_ERRNO", "defaultErrnoRet": 1,
            "architectures": ["SCMP_ARCH_X86"],
            "flags": ["SECCOMP_FILTER_FLAG_LOG"], "listenerPath": "/l", "listenerMetadata": "m",
            "syscalls": [{
                "names": ["read"], "name": "read", "action": "SCMP_ACT_ALLOW", "errnoRet": 1,
                "args": [{"index": 0, "value": 1, "valueTwo": 2, "op": "SCMP_CMP_EQ"}],
                "comment": "c",
                "includes": {"caps": ["CAP_KILL"], "arches": ["arm64"], "minKernel": "4.8"},
                "excludes": {"caps": ["CAP_KILL"], "arches": ["arm64"], "minKernel": "4.8"}
            }]
        });
        let fields =
            |value: &Value| -> Vec<String> { value.as_object().unwrap().keys().cloned().collect() };
        let sorted = |known: &[&str]| {
            let mut known: Vec<String> = known.iter().map(|&k| k.to_owned()).collect();
            known.sort();
            known
        };
        let without_arch_map: Vec<String> = sorted(PROFILE_FIELDS)
            .into_iter()
            .filter(|key| key != "archMap")
            .collect();
        assert_eq!(fields(&full), without_arch_map);
        let rule = &full["syscalls"][0];
        assert_eq!(fields(rule), sorted(RULE_FIELDS));
        assert_eq!(fields(&rule["args"][0]), sorted(ARG_FIELDS));
        assert_eq!(fields(&rule["includes"]), sorted(CONDITION_FIELDS));
        // Every one of them is read (none refused as unknown).
        let profile: Profile = serde_json::from_value(full.clone()).unwrap();
        assert_eq!(profile.listener_path.as_deref(), Some("/l"));
        let map = serde_json::json!({"architecture": "SCMP_ARCH_X86_64", "subArchitectures": []});
        assert_eq!(fields(&map), sorted(ARCH_MAP_FIELDS));
        serde_json::from_value::<ArchMap>(map).unwrap();
    }

    /// A profile file that isn't a small regular file is refused at once:
    /// a FIFO's open would wait for a writer, `/dev/zero` never ends.
    #[test]
    fn profile_files_are_small_regular_files() {
        let dir = std::env::temp_dir().join(format!("shipyard-profile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert_eq!(Profile::from_file(&fifo), Err(SeccompError::NotAFile));
        assert_eq!(
            Profile::from_file(Path::new("/dev/zero")),
            Err(SeccompError::NotAFile)
        );
        assert_eq!(Profile::from_file(&dir), Err(SeccompError::NotAFile));
        let big = dir.join("big.json");
        std::fs::write(&big, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert_eq!(Profile::from_file(&big), Err(SeccompError::TooLarge));
        let good = dir.join("good.json");
        std::fs::write(&good, r#"{"defaultAction": "SCMP_ACT_ALLOW"}"#).unwrap();
        assert_eq!(
            Profile::from_file(&good).unwrap().default_action,
            Action::Allow
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
