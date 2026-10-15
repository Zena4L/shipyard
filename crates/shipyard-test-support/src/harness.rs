//! Test selection for the `harness = false` test binaries (the terminal
//! tests, the engine's detached ones): their arguments read as libtest
//! reads them, so `cargo test -- NAME`, `--exact`, `--skip`, `--ignored`,
//! `--include-ignored` and `--list` mean what they do for any other test.

/// libtest options that take a value; their value is skipped so it isn't
/// taken for a name filter.
pub const OPTIONS_WITH_VALUES: &[&str] = &[
    "--test-threads",
    "--color",
    "--format",
    "--logfile",
    "--shuffle-seed",
    "-Z",
];

/// The tests to run, as libtest would pick them from `args`: names
/// containing any filter (or equal to one, with `--exact`), minus `--skip`
/// matches (each one also a substring, or the name with `--exact`).
/// `ignored` count as libtest's `#[ignore]`d tests: `--ignored` picks only
/// them, `--include-ignored` adds them, and so does `ignored_anyway` (the
/// caller's own reason to run them). Other libtest options are accepted and
/// ignored. In order: `ordinary`'s, then `ignored`'s.
#[must_use]
pub fn select<'a, T>(
    args: &[String],
    ordinary: &'a [(&'static str, T)],
    ignored: &'a [(&'static str, T)],
    ignored_anyway: bool,
) -> Vec<&'a (&'static str, T)> {
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--skip" => skips.extend(rest.next()),
            option if OPTIONS_WITH_VALUES.contains(&option) => {
                rest.next();
            }
            option if option.starts_with('-') => {}
            filter => filters.push(filter),
        }
    }
    let exact = args.iter().any(|arg| arg == "--exact");
    let only_ignored = args.iter().any(|arg| arg == "--ignored");
    let with_ignored =
        only_ignored || ignored_anyway || args.iter().any(|arg| arg == "--include-ignored");
    let matches = |name: &str, pattern: &str| {
        if exact {
            name == pattern
        } else {
            name.contains(pattern)
        }
    };
    let ordinary = ordinary.iter().filter(|_| !only_ignored);
    let ignored = ignored.iter().filter(|_| with_ignored);
    ordinary
        .chain(ignored)
        .filter(|(name, _)| {
            (filters.is_empty() || filters.iter().any(|f| matches(name, f)))
                && !skips.iter().any(|skip| matches(name, skip))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::select;

    const ORDINARY: &[(&str, u8)] = &[("one_two", 1), ("two", 2), ("three", 3)];
    const IGNORED: &[(&str, u8)] = &[("root_two", 4)];

    fn picked(args: &[&str], anyway: bool) -> Vec<u8> {
        let args: Vec<String> = args.iter().map(|&arg| arg.to_owned()).collect();
        select(&args, ORDINARY, IGNORED, anyway)
            .into_iter()
            .map(|&(_, n)| n)
            .collect()
    }

    #[test]
    fn arguments_pick_as_libtests_do() {
        assert_eq!(picked(&[], false), [1, 2, 3]);
        assert_eq!(picked(&["two"], false), [1, 2]);
        assert_eq!(picked(&["two", "--exact"], false), [2]);
        assert_eq!(picked(&["--skip", "two"], false), [3]);
        assert_eq!(picked(&["--skip", "two", "--exact"], false), [1, 3]);
        assert_eq!(picked(&["--include-ignored", "two"], false), [1, 2, 4]);
        assert_eq!(picked(&["--ignored"], false), [4]);
        assert_eq!(picked(&[], true), [1, 2, 3, 4]);
        // A value is never a filter.
        assert_eq!(picked(&["--test-threads", "1", "three"], false), [3]);
        assert_eq!(picked(&["--format", "terse"], false), [1, 2, 3]);
    }
}
