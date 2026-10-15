# Contributing

Thanks for your interest. Shipyard is built in stages (the [roadmap](docs/ROADMAP.md)),
each from first principles, with *The Linux Programming Interface* as the reference;
contributions that fit the current stage are the easiest to take.

## Getting set up

[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) covers the toolchain, the commands, and
running the Linux-only code from macOS (`scripts/test-linux.sh`, or a Lima VM).

## What a change needs

- **Tests**, written with the change: unit tests, and end-to-end tests for what a user
  sees. Linux behaviour is run on Linux, not only compiled.
- **Clean checks**, as CI runs them:

  ```sh
  cargo fmt --all --check
  cargo clippy --workspace --all-targets
  cargo clippy --workspace --all-targets --target aarch64-unknown-linux-gnu   # from macOS
  cargo test --workspace
  scripts/test-linux.sh            # Linux, as a normal user and as root
  ```

  Clippy's `pedantic` lints are on, with `-D warnings`. Fix a lint rather than
  allowing it; an `#[allow]` needs a `reason`. Every `unsafe` block needs a
  `// SAFETY:` comment, and every public item a doc comment.
- **Docs**, as part of the change: what users see goes in [`docs/user/`](docs/user/),
  and the command reference is regenerated with
  `UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference`.
- **Commits** in [Conventional Commits](https://www.conventionalcommits.org/) form
  (`feat:`, `fix:`, `docs:`, `chore:`, …).

The conventions (platform gating, errors, the rules for code that runs after `fork`)
are in [AGENTS.md](AGENTS.md), which guides human and AI contributors alike.

## Security issues

Report them privately, as [SECURITY.md](SECURITY.md) says, not in an issue.

## License

By contributing, you agree that your contributions are licensed under the project's
terms: either of the [Apache License 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option.
