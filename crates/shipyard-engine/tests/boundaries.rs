//! The engine's boundaries, checked in its source (stage 9.1's threat model):
//! it never prints (what it has to say goes to the caller's `Reporter`), and
//! it reads the environment in one place (`Paths::from_env`), so a caller
//! (the CLI, an API, a test) decides where things are.
//!
//! A helper's entry point is the exception: it's a process of its own (this
//! binary, re-executed), with its own arguments and stderr.

#![allow(clippy::expect_used, reason = "a panic in a test is a clear failure")]

use std::path::{Path, PathBuf};

/// Files that may print and read their arguments and environment: helper
/// entry points.
const ENTRY_POINTS: &[&str] = &[
    "image/helper.rs",
    "network/dns_server.rs",
    "run/cleanup.rs",
    "run/health.rs",
];

/// The one file that reads the environment for places.
const PATHS: &str = "paths.rs";

fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, found: &mut Vec<(String, String)>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("reading the engine's sources")
            .map(|entry| entry.expect("reading the engine's sources").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, root, found);
            } else if path.extension().is_some_and(|extension| extension == "rs")
                // A module of tests alone (`mod tests;`) is test code whole.
                && path.file_name().is_some_and(|name| name != "tests.rs")
            {
                let name = path
                    .strip_prefix(root)
                    .expect("a source under src")
                    .to_string_lossy()
                    .into_owned();
                let text = std::fs::read_to_string(&path).expect("reading a source");
                found.push((name, text));
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    walk(&root, &root, &mut found);
    assert!(
        found.iter().any(|(name, _)| name == PATHS),
        "the walk finds the sources: {found:?}",
        found = found.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    found
}

/// Lines of `text` that hold any of `tokens`, with their numbers: up to its
/// tests (a test may make a temporary directory, or print).
fn lines_with<'a>(text: &'a str, tokens: &[&str]) -> Vec<(usize, &'a str)> {
    text.lines()
        .take_while(|line| line.trim() != "#[cfg(test)]")
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| tokens.iter().any(|token| line.contains(token)))
        .map(|(number, line)| (number + 1, line))
        .collect()
}

#[test]
fn the_engine_never_prints() {
    const PRINTING: &[&str] = &[
        "stdout()",
        "stderr()",
        "print!(",
        "println!(",
        "eprint!(",
        "eprintln!(",
        "dbg!(",
    ];
    let mut found = Vec::new();
    for (name, text) in sources() {
        if ENTRY_POINTS.contains(&name.as_str()) {
            continue;
        }
        for (number, line) in lines_with(&text, PRINTING) {
            // Asking whether a stream is a terminal isn't printing to it.
            if !line.contains(".is_terminal()") {
                found.push(format!("{name}:{number}: {}", line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the engine reports, it doesn't print:\n{}",
        found.join("\n")
    );
}

#[test]
fn only_paths_reads_the_environment() {
    let mut found = Vec::new();
    for (name, text) in sources() {
        let allowed: &[&str] = if name == PATHS {
            &["env::var", "env::temp_dir"]
        } else if ENTRY_POINTS.contains(&name.as_str()) {
            &["env::args", "env::var"]
        } else {
            &[]
        };
        let reads = [
            "env::var",
            "env::args",
            "env::temp_dir",
            "env::home_dir",
            "env::current_dir",
        ];
        for (number, line) in lines_with(&text, &reads) {
            if !allowed.iter().any(|token| line.contains(token)) {
                found.push(format!("{name}:{number}: {}", line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the environment is read in Paths::from_env:\n{}",
        found.join("\n")
    );
}

/// The checks above see what they're meant to: a line that prints, and a
/// line that reads the environment, are found.
#[test]
fn the_checks_find_what_they_look_for() {
    let text = "fn f() {\n    let _ = writeln!(std::io::stderr(), \"x\");\n    // stderr() in a comment\n    std::env::var_os(\"X\");\n}\n#[cfg(test)]\nmod tests { fn g() { std::env::temp_dir(); } }\n";
    assert_eq!(
        lines_with(text, &["stderr()"]),
        [(2, "    let _ = writeln!(std::io::stderr(), \"x\");")]
    );
    assert_eq!(lines_with(text, &["env::var"]).len(), 1);
    assert!(
        lines_with(text, &["env::temp_dir"]).is_empty(),
        "a test's own"
    );
}
