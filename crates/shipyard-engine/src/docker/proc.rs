//! What `top` and `stats` read of `/proc`: the clock's ticks, and a
//! process's files with a bound (a container's processes write them).

use std::io::Read as _;

/// Clock ticks a second (`sysconf(_SC_CLK_TCK)`), the unit of `/proc`'s
/// CPU times: 100 if it can't be had.
pub(crate) fn clock_ticks() -> u64 {
    nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .and_then(|ticks| u64::try_from(ticks).ok())
        .filter(|&ticks| ticks > 0)
        .unwrap_or(100)
}

/// A `/proc` file's text (lossy), at most `max` bytes of it; `None` if
/// it's gone.
pub(crate) fn read(path: &str, max: u64) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}
