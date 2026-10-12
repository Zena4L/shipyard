#!/usr/bin/env bash
# Runs the OCI runtime conformance suite (runtime-tools' validation/) against
# shipyard-runtime and compares each test's outcome with expected.txt (stage 8,
# H5). Run by `scripts/test-linux.sh --conformance [NAME…]`, as root, in the
# conformance image (scripts/conformance/Dockerfile): never on a real host.
#
#   run.sh             every test expected.txt doesn't skip
#   run.sh NAME…       only these (a `skip` one too, when named: to recheck it)
#   run.sh --list      the suite's tests
#
# A test passes when it exits 0 and:
# - it printed results: none failing (`not ok`, unless TODO), and a plan
#   (`1..N`) that counts them (a result skipped, `ok N # SKIP`, is runtimetest
#   leaving out what the config doesn't set);
# - it printed none: only for the tests that report that way (SILENT below),
#   and with no diagnostic carrying an "error" (how they report a failure).
# Each outcome is compared with expected.txt. A `pass` must pass. A `fail`
# must fail, and for its reason: its /PATTERN/ must match the test's output.
# A timeout, or a test of the suite expected.txt doesn't list, is always a
# deviation. Any deviation exits 1, and the output of each is printed. The
# TAP and stderr of every test are kept in $CONFORMANCE_LOGS.
set -uo pipefail

if [ ! -e /.dockerenv ] || [ "${MODE:-}" != conformance ]; then
    echo "run.sh: only through test-linux.sh --conformance (it runs containers as root)" >&2
    exit 2
fi

here=$(cd "$(dirname "$0")" && pwd)
tree=/opt/runtime-tools
expected=$here/expected.txt
runtime=${RUNTIME:-/target/debug/shipyard-runtime}
logs=${CONFORMANCE_LOGS:-/target/conformance}
limit=${CONFORMANCE_TIMEOUT:-120}

# The tests that print no TAP results of their own: the hook tests (they
# print only a plan, `1..0`, and a diagnostic on failure), and those that
# only expect `create` to refuse a config.
SILENT=" hooks prestart poststart poststop prestart_fail poststart_fail poststop_fail "
SILENT+=" process_capabilities_fail process_rlimits_fail "

suite=$(cd "$tree" && find validation -mindepth 2 -maxdepth 2 -name '*.t' \
    | sed 's|validation/\([^/]*\)/.*|\1|' | sort)
if [ "${1:-}" = --list ]; then
    echo "$suite"
    exit 0
fi

# expected.txt: `NAME pass`, `NAME fail /PATTERN/ REASON…`, `NAME skip
# REASON…`; `#` comments. `NAME@ARCH …` (Debian's names: amd64, arm64) is
# for that architecture only, and wins over the plain line.
arch=$(dpkg --print-architecture)
declare -A want pattern reason
while read -r name status rest; do
    case "$name" in "" | \#*) continue ;; esac
    if [[ "$name" == *@* ]]; then
        [ "${name#*@}" = "$arch" ] || continue
        name=${name%@*}
    elif [ -n "${want[$name]:-}" ]; then
        continue
    fi
    case "$status" in
        pass) ;;
        fail)
            if [[ ! "$rest" =~ ^/([^/]+)/\ +(.+)$ ]]; then
                echo "run.sh: expected.txt: $name: a fail needs /PATTERN/ and its reason" >&2
                exit 2
            fi
            pattern[$name]=${BASH_REMATCH[1]}
            rest=${BASH_REMATCH[2]}
            ;;
        skip)
            if [ -z "$rest" ]; then
                echo "run.sh: expected.txt: $name: a skip needs its reason" >&2
                exit 2
            fi
            ;;
        *)
            echo "run.sh: expected.txt: $name: unknown status '$status'" >&2
            exit 2
            ;;
    esac
    want[$name]=$status
    reason[$name]=$rest
done <"$expected"

deviations=0
for name in $suite; do
    if [ -z "${want[$name]:-}" ]; then
        echo "DEVIATION $name: in the suite, not in expected.txt" >&2
        deviations=$((deviations + 1))
    fi
done
for name in "${!want[@]}"; do
    if ! grep -qx "$name" <<<"$suite"; then
        echo "DEVIATION $name: in expected.txt, not in the suite" >&2
        deviations=$((deviations + 1))
    fi
done

named=false
if [ $# -gt 0 ]; then
    named=true
    selected=("$@")
    for name in "${selected[@]}"; do
        if ! grep -qx "$name" <<<"$suite"; then
            echo "run.sh: no test $name in the suite (see --list)" >&2
            exit 2
        fi
    done
else
    mapfile -t selected <<<"$suite"
fi

# Removes whatever a test left: its containers (and with them their cgroups),
# and its bundles (a test that stops on a fatal error leaves its own).
cleanup() {
    local dir
    for dir in /run/shipyard/*/; do
        [ -d "$dir" ] || continue
        "$runtime" delete --force "$(basename "$dir")" >/dev/null 2>&1 || true
    done
    rm -rf /tmp/ocitest*
}

# The outcome of test $1, whose TAP is $2: pass, fail or timeout (exit $3).
outcome() {
    local name=$1 tap=$2 code=$3 results failing planned
    if [ "$code" -eq 124 ] || [ "$code" -eq 137 ]; then
        echo timeout
        return
    fi
    [ "$code" -eq 0 ] || { echo fail; return; }
    results=$(grep -c '^\(not \)\?ok\b' "$tap")
    if [ "$results" -eq 0 ]; then
        if [[ "$SILENT" == *" $name "* ]] && ! grep -q '"error":' "$tap"; then
            echo pass
        else
            echo fail
        fi
        return
    fi
    failing=$(grep '^not ok' "$tap" | grep -vi '#[[:space:]]*todo\b' || true)
    planned=$(sed -n 's/^1\.\.\([0-9]\+\).*/\1/p' "$tap" | head -1)
    if [ -n "$failing" ] || [ "$planned" != "$results" ]; then
        echo fail
    else
        echo pass
    fi
}

rm -rf "$logs"
mkdir -p "$logs"
ran=0
printf '%-32s %-6s %-8s %s\n' TEST WANT GOT NOTE
for name in "${selected[@]}"; do
    status=${want[$name]:-}
    [ -n "$status" ] || continue
    if [ "$status" = skip ] && ! $named; then
        printf '%-32s %-6s %-8s %s\n' "$name" skip - "${reason[$name]:0:100}"
        continue
    fi
    cleanup
    (cd "$tree" && RUNTIME=$runtime timeout -k 10 "$limit" "./validation/$name/$name.t") \
        >"$logs/$name.tap" 2>"$logs/$name.err"
    code=$?
    ran=$((ran + 1))
    got=$(outcome "$name" "$logs/$name.tap" "$code")
    note=""
    if [ "$got" != pass ]; then
        note=$(grep -m1 '^not ok' "$logs/$name.tap" || true)
        [ -n "$note" ] || note=$(grep -m1 '"error":' "$logs/$name.tap" | sed 's/^ *//' || true)
        [ -n "$note" ] || note="exit $code: $(tail -1 "$logs/$name.err")"
    fi
    deviant=false
    case "$status/$got" in
        pass/pass | skip/*) ;;
        fail/fail)
            # It must fail for its stated reason.
            # (Not `cat | grep -q`: grep stops at the first match, and
            # with pipefail cat's broken pipe would fail the check.)
            if ! grep -qE -- "${pattern[$name]}" "$logs/$name.tap" "$logs/$name.err"; then
                deviant=true
                note="not for its reason (/${pattern[$name]}/): $note"
            fi
            ;;
        *) deviant=true ;;
    esac
    if $deviant; then
        deviations=$((deviations + 1))
        printf '%-32s %-6s %-8s DEVIATION %s\n' "$name" "$status" "$got" "${note:0:120}"
        [ "$got" != pass ] && {
            echo "---- $name: the end of its TAP and stderr"
            tail -25 "$logs/$name.tap"
            tail -10 "$logs/$name.err"
            echo "----"
        } >&2
    else
        printf '%-32s %-6s %-8s %s\n' "$name" "$status" "$got" "${note:0:120}"
    fi
done
cleanup

if [ "$ran" -eq 0 ]; then
    echo "run.sh: no test ran" >&2
    exit 1
fi
echo "ran $ran; the TAP and stderr of each are in $logs"
if [ "$deviations" -gt 0 ]; then
    echo "$deviations deviation(s) from expected.txt" >&2
    exit 1
fi
echo "conformance as expected"
