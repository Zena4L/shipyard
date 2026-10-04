# Exit codes

Shipyard's exit codes follow Docker's, so existing scripts keep working.

> **Status:** this version validates commands but can't start them yet, so
> today you'll only see `0` (for `--help` and `--version`) and `125`. The rest of
> this page describes the contract that applies once commands run; it won't
> change when that lands.

When the command runs and exits, its exit code passes through unchanged,
whatever it is (0–255). The codes below have a special meaning:

| Code | Meaning |
|---|---|
| `125` | **Shipyard failed.** The command never ran. Causes include an unknown option, an invalid value (signal, limit, user ID, environment variable), a relative working directory, or a host problem. The message on stderr says which. |
| `126` | **The command could not be executed.** It exists but isn't executable, or isn't a valid program. |
| `127` | **The command was not found.** This is also the code when a script's `#!` interpreter is missing. |
| `128 + N` | **The command was killed by signal N.** |

A command can also exit with 125–127, or above 128, by itself. If you need to
tell the cases apart, check stderr: when the code comes from Shipyard, the
message starts with `shipyard:` or `error:`.

## Common signal codes

| Code | Signal | Usually means |
|---|---|---|
| `130` | `SIGINT` (2) | Interrupted with Ctrl-C |
| `134` | `SIGABRT` (6) | The program aborted itself |
| `137` | `SIGKILL` (9) | Killed forcibly, for example after `--stop-timeout` expired |
| `139` | `SIGSEGV` (11) | The program crashed (segmentation fault) |
| `143` | `SIGTERM` (15) | Asked to stop, and it did |

## In scripts

```sh
shipyard run my-job
case $? in
  0)   echo "succeeded" ;;
  125) echo "shipyard rejected the job; see the message above" ;;
  126) echo "my-job is not executable" ;;
  127) echo "my-job was not found" ;;
  137) echo "my-job was killed" ;;
  *)   echo "my-job failed" ;;
esac
```

`--help` and `--version` exit with `0`.
