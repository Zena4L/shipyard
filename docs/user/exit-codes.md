# Exit codes

Shipyard's exit codes follow Docker's, so existing scripts keep working.


When the command runs and exits, its exit code passes through unchanged,
whatever it is (0–255). The codes below have a special meaning:

| Code | Meaning |
|---|---|
| `125` | **Shipyard failed.** The command never ran. Causes include an unknown option, an invalid value (signal, limit, user ID, environment variable), a relative working directory, or a host problem. The message on stderr says which. |
| `126` | **The command could not be executed.** It isn't executable, is a directory, isn't a program this system can run (a binary built for another CPU, or a script with no `#!` line), or the user hit its process limit (`nproc`) as it started. If Shipyard itself can't create the process (the whole system is out of processes), that's `125`. |
| `127` | **The command was not found.** Also when the file exists but its `#!` interpreter (or, for a binary, its loader) is missing; the message says so. |
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
