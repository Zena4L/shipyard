#!/bin/sh
# Step 13.1's probe, the guest's side (run by probe-init from the share). Results to
# results.txt; each measurement the Mac makes waits on a stage marker (stage-NAME, then
# the Mac's ack-NAME).
P=/mnt/probe
say() { echo "$*" >>"$P/results.txt"; echo "$*"; }
stage() {
    : >"$P/stage-$1"
    while [ ! -e "$P/ack-$1" ]; do sleep 0.2; done
}
now() { cut -d' ' -f1 /proc/uptime; } # seconds since boot, to 0.01 (busybox's date has no %N)
timed() { # NAME COMMAND…: its wall time
    name=$1; shift
    start=$(now)
    "$@" >/dev/null 2>&1
    code=$?
    end=$(now)
    say "$(awk -v s="$start" -v e="$end" -v n="$name" -v c="$code" 'BEGIN { printf "%s: %.2f s (exit %d)", n, e - s, c }')"
}

say "== guest: $(uname -r), $(nproc) CPUs, $(grep MemTotal /proc/meminfo)"

say "== rosetta"
if mount -t virtiofs rosetta /mnt/rosetta 2>/dev/null; then
    say "the share mounted: $(ls /mnt/rosetta | tr '\n' ' ')"
    mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc 2>/dev/null
    # Apple's documented registration (an x86-64 ELF), F: opened now, so it works from any
    # mount namespace (a container's).
    printf '%s' ':rosetta:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00:\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/mnt/rosetta/rosetta:CF' \
        >/proc/sys/fs/binfmt_misc/register && say "binfmt registered: $(head -1 /proc/sys/fs/binfmt_misc/rosetta)"
    say "amd64 busybox uname -m: $("$P/amd64/busybox" uname -m 2>&1)"
    say "amd64 Go (yq --version): $("$P/amd64/yq" --version 2>&1 | head -1)"
    say "amd64 Rust (threads, syscalls): $("$P/amd64/threads" 2>&1 | tr '\n' ' ')"
    timed "amd64 busybox: 2000 runs of true" sh -c "i=0; while [ \$i -lt 2000 ]; do $P/amd64/busybox true; i=\$((i+1)); done"
    timed "arm64 busybox: 2000 runs of true" sh -c 'i=0; while [ $i -lt 2000 ]; do /bin/busybox true; i=$((i+1)); done'
else
    say "no rosetta share"
fi

say "== disk (TRIM)"
say "discard_max_bytes: $(cat /sys/block/vda/queue/discard_max_bytes 2>/dev/null), discard_granularity: $(cat /sys/block/vda/queue/discard_granularity 2>/dev/null)"
mkfs.ext4 -q -F /dev/vda && mount /dev/vda /mnt/disk || say "the disk didn't mount"
stage disk-empty
dd if=/dev/urandom of=/mnt/disk/fill bs=1M count=1024 2>/dev/null; sync
stage disk-written
rm /mnt/disk/fill; sync
stage disk-deleted
say "fstrim: $(fstrim -v /mnt/disk 2>&1)"
sync
stage disk-trimmed
mount -o remount,discard /mnt/disk && say "remounted with discard"
dd if=/dev/urandom of=/mnt/disk/fill bs=1M count=512 2>/dev/null; sync
rm /mnt/disk/fill; sync; sleep 2
stage disk-discard-mount

say "== file sharing (virtiofs) against the ext4 disk"
mkdir -p "$P/bench" /mnt/disk/bench
for where in "$P/bench" /mnt/disk/bench; do
    timed "$where: write 10000 small files" sh -c "cd $where && i=0; while [ \$i -lt 10000 ]; do echo \$i >f\$i; i=\$((i+1)); done"
    timed "$where: stat-walk them (find)" find "$where" -type f
    timed "$where: ls -l them" ls -l "$where"
    timed "$where: write 1 GiB" dd if=/dev/zero of="$where/big" bs=1M count=1024 conv=fsync
    sync; echo 3 >/proc/sys/vm/drop_caches
    timed "$where: read 1 GiB" dd if="$where/big" of=/dev/null bs=1M
    timed "$where: remove them" rm -rf "$where"
done

say "== memory (the balloon)"
stage mem-base
mount -o remount,size=3G /tmp
dd if=/dev/zero of=/tmp/fill bs=1M count=2048 2>/dev/null
say "after filling 2 GiB: $(grep -E 'MemFree' /proc/meminfo | tr -s ' ')"
stage mem-filled
rm /tmp/fill
say "after freeing it: $(grep -E 'MemFree' /proc/meminfo | tr -s ' ')"
stage mem-freed
stage mem-balloon
for wait in 5 10 15; do
    sleep 5
    say "balloon at 1 GiB, +${wait} s: $(grep -E 'MemTotal|MemFree|MemAvailable' /proc/meminfo | tr -s ' ' | tr '\n' ' ')"
done
stage mem-measured
say "after the reset: $(grep -E 'MemTotal|MemFree' /proc/meminfo | tr -s ' ' | tr '\n' ' ')"
say "balloon in /sys: $(ls /sys/bus/virtio/drivers/virtio_balloon 2>/dev/null | tr '\n' ' ')"

say "== clock (a 60 s pause)"
before=$(date +%s); up_before=$(now)
stage clock
after=$(date +%s); up_after=$(now)
say "guest wall clock before $before, after $after (moved $((after - before)) s); uptime moved $(awk -v a="$up_before" -v b="$up_after" 'BEGIN { printf "%.1f", b - a }') s"
sleep 5
say "5 s later the guest's wall clock: $(date +%s)"
say "== done"
