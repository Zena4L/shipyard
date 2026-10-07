#!/usr/bin/env bash
# Reclaims the Docker space Shipyard's Linux test workflows build up: the
# cargo target volumes (`CARGO_TARGET_DIR`) that `test-linux.sh`, the x86-64
# run, the mutation checks and the review runs each fill, plus the test image
# the Dockerfile builds. These hold only compiled artifacts and toolchains,
# all regenerable; the repository itself is mounted read-only and never lives
# in a volume (see docs/DEVELOPMENT.md).
#
# Levels, least to most (each is a superset of the one before):
#
#   scripts/clean-docker.sh                # scratch only: mutation + review target volumes
#   scripts/clean-docker.sh --phase        # scratch, and the amd64/ci targets (keeps the main
#                                          #   shipyard-linux-* dev loop and all toolchains)
#   scripts/clean-docker.sh --images       # also the shipyard-test image + dangling images
#   scripts/clean-docker.sh --all          # every shipyard-* volume and image, including the
#                                          #   main dev loop and toolchains (full rebuild next run)
#
#   scripts/clean-docker.sh --dry-run [level]   # show what each level would remove, remove nothing
#
# "scratch" (the default) is what the mutation and review runs create and are
# meant to throw away after each session: `shipyard-mutation-*`, `shipyard-mut-*`
# and `shipyard-review-*`. These are the volumes that grow without bound, since
# each mutation is a fresh scratch copy built in its own cache. The default
# leaves the everyday caches (`shipyard-linux-target/cargo/rustup`) and the
# pinned toolchains alone, so the next `test-linux.sh` stays fast and needs no
# network.
#
# Only Shipyard's own resources are touched: volumes named `shipyard-*` and
# images named `shipyard-test:*`. Nothing matching another project is removed.
set -euo pipefail

if ! command -v docker >/dev/null; then
    echo "clean-docker.sh: docker not found on PATH" >&2
    exit 1
fi

dry_run=false
level=scratch
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=true ;;
        --phase) level=phase ;;
        --images) level=images ;;
        --all) level=all ;;
        scratch | phase | images | all) level="$arg" ;; # a level after --dry-run
        -h | --help)
            sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "clean-docker.sh: unknown argument '$arg'" >&2
            echo "usage: $0 [--dry-run] [--phase | --images | --all]" >&2
            exit 2
            ;;
    esac
done

# All shipyard volumes that exist, filtered to the ones a level removes. The
# main dev loop (shipyard-linux-*) is only ever in the `all` set.
all_volumes=$(docker volume ls -q | grep '^shipyard-' || true)

volumes_for() {
    case "$1" in
        scratch)
            # Throwaway caches from mutation and review runs.
            printf '%s\n' "$all_volumes" | grep -E '^shipyard-(mutation|mut|review)' || true
            ;;
        phase)
            # Scratch, plus the secondary target volumes (not the main dev loop,
            # not the toolchains: those stay for a fast next run).
            printf '%s\n' "$all_volumes" |
                grep -E '^shipyard-(mutation|mut|review|amd64-target|amd64-cargo|ci-target)' || true
            ;;
        images)
            volumes_for phase
            ;;
        all)
            printf '%s\n' "$all_volumes"
            ;;
    esac
}

# Human-readable size of a volume (empty if gone). Best effort: a tiny Alpine
# container runs `du` against the volume's mountpoint.
volume_size() {
    docker run --rm -v "$1":/vol alpine du -sh /vol 2>/dev/null | awk '{print $1}'
}

images_to_remove() {
    case "$1" in
        images) docker image ls -q 'shipyard-test' | sort -u ;;
        all) docker image ls -q 'shipyard-test' | sort -u ;;
        *) : ;;
    esac
}

volumes=$(volumes_for "$level")

echo "level: $level"
if [ -z "$volumes" ] && { [ "$level" = scratch ] || [ "$level" = phase ]; }; then
    echo "nothing to remove (no matching shipyard volumes)."
fi

if [ -n "$volumes" ]; then
    echo "volumes:"
    while IFS= read -r v; do
        [ -z "$v" ] && continue
        printf '  %-45s %s\n' "$v" "$(volume_size "$v")"
    done <<<"$volumes"
fi

if [ "$level" = images ] || [ "$level" = all ]; then
    imgs=$(images_to_remove "$level")
    if [ -n "$imgs" ]; then
        echo "images (shipyard-test):"
        docker image ls 'shipyard-test' --format '  {{.Repository}}:{{.Tag}}  {{.Size}}'
    fi
fi

if "$dry_run"; then
    echo "(dry run: nothing removed)"
    exit 0
fi

removed_any=false
if [ -n "$volumes" ]; then
    # xargs: remove them in one call; `docker volume rm` skips any in use with a
    # clear error rather than failing the whole script.
    echo "$volumes" | tr '\n' '\0' | xargs -0 -r docker volume rm || true
    removed_any=true
fi

if [ "$level" = images ] || [ "$level" = all ]; then
    imgs=$(images_to_remove "$level")
    if [ -n "$imgs" ]; then
        echo "$imgs" | tr '\n' '\0' | xargs -0 -r docker image rm -f || true
        removed_any=true
    fi
    echo "pruning dangling images"
    docker image prune -f >/dev/null || true
fi

if "$removed_any"; then
    echo
    echo "after:"
    docker system df
fi
