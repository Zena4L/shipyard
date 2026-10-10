#!/usr/bin/env bash
# Runs a test command for CI with a time limit, its output going to a file
# that's printed afterwards, then lists what it left running.
#
# Why a file: a test that leaves processes behind (in a session of their
# own, out of the test's reach) keeps a pipe open, and whoever reads the
# pipe (cargo, the step's log) waits for them forever: the job runs into its
# own time limit, and GitHub keeps no log of a job it cancelled. A file is
# never waited for: the step ends when the command does, or when `timeout`
# ends it, and its log is kept.
#
# usage: run-logged.sh SECONDS NAME COMMAND [ARGS...]
set -uo pipefail
seconds=$1
name=$2
shift 2
log="${RUNNER_TEMP:-/tmp}/$name.log"
status=0
timeout --kill-after=10s "$seconds" "$@" >"$log" 2>&1 || status=$?
cat "$log"
if [ "$status" = 124 ] || [ "$status" = 137 ]; then
    echo "::error::$name took over ${seconds}s: stopped (the last test named above is the one that hung)"
fi
# What the run left behind: a hang's culprits, or a leak.
left=$(ps -eo pid,ppid,pgid,sid,stat,etime,args |
    grep -E 'shipyard|pasta|terminal-|sleep' | grep -vE 'grep|run-logged' || true)
if [ -n "$left" ]; then
    echo "--- processes left after $name ---"
    echo "$left"
fi
exit "$status"
