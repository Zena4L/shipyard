//! Signal numbers: parsing names like `TERM` or `SIGRTMIN+3`, and naming them
//! back (TLPI ch. 20).

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use nix::sys::signal::Signal as NixSignal;

/// A signal number that is valid on this platform.
///
/// Kept as a raw number rather than nix's enum because Linux real-time signals
/// (`SIGRTMIN`..`SIGRTMAX`) have no enum entries.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signal(i32);

/// Shows the name (`Signal(SIGTERM)`), which is what logs need.
impl fmt::Debug for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Signal")
            .field(&format_args!("{self}"))
            .finish()
    }
}

/// Why a signal name or number was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SignalError {
    /// Nothing was given.
    #[error("signal name is empty")]
    Empty,
    /// Not a signal name this platform knows.
    #[error("unknown signal \"{0}\"")]
    Unknown(String),
    /// A number outside this platform's signal range.
    #[error("signal {number} is out of range (valid signals are 1 to {max})")]
    OutOfRange {
        /// The number given.
        number: i32,
        /// The highest valid signal on this platform.
        max: i32,
    },
}

impl Signal {
    /// `SIGTERM`: please exit. The default stop signal.
    pub const TERM: Self = Self(libc::SIGTERM);
    /// `SIGKILL`: exit now. Can't be caught or ignored.
    pub const KILL: Self = Self(libc::SIGKILL);

    /// Accepts a signal number if this platform has it.
    pub fn from_raw(number: i32) -> Result<Self, SignalError> {
        let max = max_signal();
        if (1..=max).contains(&number) {
            Ok(Self(number))
        } else {
            Err(SignalError::OutOfRange { number, max })
        }
    }

    /// The signal number, as the kernel uses it.
    #[must_use]
    pub fn as_raw(self) -> i32 {
        self.0
    }

    /// The conventional name (`SIGTERM`, `SIGRTMIN+3`), if the signal has one.
    #[must_use]
    pub fn name(self) -> Option<Cow<'static, str>> {
        if let Ok(sig) = NixSignal::try_from(self.0) {
            return Some(Cow::Borrowed(sig.as_str()));
        }
        realtime_name(self.0).map(Cow::Owned)
    }
}

/// Prints the name, or the bare number for signals without one, so the output
/// always parses back to the same signal.
impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(&name),
            None => write!(f, "{}", self.0),
        }
    }
}

/// Accepts `15`, `TERM`, `SIGTERM` and `sigterm`, plus the aliases `IOT`,
/// `CLD` and `POLL`. On Linux also `RTMIN`, `RTMIN+n`, `RTMAX` and `RTMAX-n`.
impl FromStr for Signal {
    type Err = SignalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(SignalError::Empty);
        }
        if let Some(number) = parse_digits(s) {
            return Self::from_raw(number);
        }
        let upper = s.to_ascii_uppercase();
        let name = upper.strip_prefix("SIG").unwrap_or(&upper);
        named(name)
            .or_else(|| alias(name))
            .or_else(|| realtime(name))
            .map(Self)
            .ok_or_else(|| SignalError::Unknown(s.to_owned()))
    }
}

/// A small, unsigned decimal number. Strict on purpose: no sign, no spaces, and
/// at most three digits, so nothing surprising parses and nothing overflows.
fn parse_digits(s: &str) -> Option<i32> {
    if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A name from this platform's table (nix's enum differs per OS, so macOS gets
/// `INFO` and `EMT`, Linux gets `PWR` and `STKFLT`).
fn named(name: &str) -> Option<i32> {
    NixSignal::iterator()
        .find(|sig| sig.as_str().strip_prefix("SIG") == Some(name))
        .map(|sig| sig as i32)
}

fn alias(name: &str) -> Option<i32> {
    match name {
        "IOT" => Some(libc::SIGABRT),
        #[cfg(target_os = "linux")]
        "CLD" => Some(libc::SIGCHLD),
        #[cfg(target_os = "linux")]
        "POLL" => Some(libc::SIGIO),
        _ => None,
    }
}

/// Real-time signal names use the kernel's numbering, not the C library's.
///
/// The kernel's real-time range is 32–64, but glibc reserves 32–33 (so its
/// `SIGRTMIN` is 34) and musl reserves 32–34 (35). Asking libc would make
/// `RTMIN+3` mean a different signal depending on how shipyard was built. Fixed
/// numbers, the same ones Docker and runc use, keep a name meaning one signal.
#[cfg(target_os = "linux")]
const RTMIN: i32 = 34;
#[cfg(target_os = "linux")]
const RTMAX: i32 = 64;

#[cfg(target_os = "linux")]
fn max_signal() -> i32 {
    RTMAX
}

/// `NSIG` is 32 in `<sys/signal.h>`, so the highest signal is 31 (`SIGUSR2`).
#[cfg(not(target_os = "linux"))]
fn max_signal() -> i32 {
    31
}

#[cfg(target_os = "linux")]
fn realtime(name: &str) -> Option<i32> {
    let number = match name {
        "RTMIN" => RTMIN,
        "RTMAX" => RTMAX,
        _ => {
            if let Some(offset) = name.strip_prefix("RTMIN+") {
                RTMIN.checked_add(parse_digits(offset)?)?
            } else {
                RTMAX.checked_sub(parse_digits(name.strip_prefix("RTMAX-")?)?)?
            }
        }
    };
    (RTMIN..=RTMAX).contains(&number).then_some(number)
}

#[cfg(not(target_os = "linux"))]
fn realtime(_name: &str) -> Option<i32> {
    None
}

#[cfg(target_os = "linux")]
fn realtime_name(number: i32) -> Option<String> {
    match number {
        RTMAX => Some("SIGRTMAX".to_owned()),
        RTMIN => Some("SIGRTMIN".to_owned()),
        n if (RTMIN..RTMAX).contains(&n) => Some(format!("SIGRTMIN+{}", n - RTMIN)),
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn realtime_name(_number: i32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn parse(s: &str) -> Result<Signal, SignalError> {
        s.parse()
    }

    #[test]
    fn accepts_numbers_and_every_name_form() {
        for form in ["15", "TERM", "SIGTERM", "sigterm", "Term", "SigTerm"] {
            assert_eq!(parse(form), Ok(Signal::TERM), "{form}");
        }
        assert_eq!(parse("KILL"), Ok(Signal::KILL));
        assert_eq!(parse("IOT").map(Signal::as_raw), Ok(libc::SIGABRT));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse(""), Err(SignalError::Empty));
        for bad in [
            "0", "999", "-1", "+15", " 15", "15 ", "SIG", "FOO", "SIGTERMX",
        ] {
            assert!(parse(bad).is_err(), "{bad} should be rejected");
        }
        assert!(matches!(
            Signal::from_raw(0),
            Err(SignalError::OutOfRange { number: 0, .. })
        ));
    }

    #[test]
    fn display_uses_the_conventional_name() {
        assert_eq!(Signal::TERM.to_string(), "SIGTERM");
        assert_eq!(Signal::KILL.to_string(), "SIGKILL");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_specific_names() {
        assert_eq!(parse("INFO").map(Signal::as_raw), Ok(libc::SIGINFO));
        assert!(parse("32").is_err());
        assert!(parse("RTMIN").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_realtime_signals_use_fixed_kernel_numbers() {
        assert_eq!(parse("RTMIN").map(Signal::as_raw), Ok(34));
        assert_eq!(parse("SIGRTMIN+3").map(Signal::as_raw), Ok(37));
        assert_eq!(parse("rtmax-2").map(Signal::as_raw), Ok(62));
        assert_eq!(parse("RTMAX").map(Signal::as_raw), Ok(64));
        // 32 and 33 are real kernel signals (libc keeps them for itself), so
        // they are accepted by number but have no name.
        assert_eq!(parse("32").map(|s| s.to_string()), Ok("32".to_owned()));
        assert!(parse("RTMIN+100").is_err());
        assert!(parse("RTMIN-1").is_err());
        assert!(parse("RTMIN++3").is_err());
        assert_eq!(Signal(37).to_string(), "SIGRTMIN+3");
        assert_eq!(parse("PWR").map(Signal::as_raw), Ok(libc::SIGPWR));
        assert_eq!(parse("CLD").map(Signal::as_raw), Ok(libc::SIGCHLD));
    }

    proptest! {
        #[test]
        fn parsing_never_panics(s in any::<String>()) {
            let _ = parse(&s);
        }

        #[test]
        fn every_valid_signal_round_trips(number in 1..=max_signal()) {
            let signal = Signal::from_raw(number).unwrap();
            prop_assert_eq!(parse(&signal.to_string()), Ok(signal));
        }
    }
}
