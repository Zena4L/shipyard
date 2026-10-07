/*
 * Kernel checks behind a container's resource control (stage 5): the cgroup
 * v2 tree, placing a process at birth, the limits (memory, pids, CPU, I/O),
 * the freezer, cgroup.kill, cgroup namespaces, delegation to an ordinary
 * user, and a device-access eBPF program. Each line of output answers one
 * question; docs/stages/05-resources.md records the results.
 *
 * Every check runs in a forked child, inside a scratch cgroup of the probe's
 * own (removed at the end), so it leaves the rest of the tree alone. Run it as
 * root in a privileged container:
 *
 *   docker run --rm --privileged -v "$PWD/scripts/probes":/p:ro rust:1-bookworm \
 *       sh -c 'gcc -Wall -Wextra -o /tmp/cgroups /p/cgroups.c && /tmp/cgroups'
 *
 * One exception, for that container only: a container's cgroup namespace
 * root is an ordinary cgroup, so it can't hand controllers to children while
 * it holds processes (the "no internal processes" rule), and the probe moves
 * them into a leaf first, as docker-in-docker does. The host's real root
 * cgroup is exempt from that rule, so on a host this never happens.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <linux/bpf.h>
#include <linux/magic.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ROOT "/sys/fs/cgroup"
#define NOBODY 65534

/* The scratch cgroup every check works in: ROOT/shipyard-probe.<pid>. */
static char scratch[64];

static const char *err(int rc) { return rc == 0 ? "ok" : strerror(errno); }

static void die(const char *what) { printf("probe setup failed: %s: %s\n", what, strerror(errno)); exit(2); }

/* A path inside the scratch cgroup (four in use at once, at most). */
static const char *at(const char *sub) {
    static char path[4][256];
    static int next;
    char *p = path[next++ % 4];
    snprintf(p, sizeof path[0], "%s/%s", scratch, sub);
    return p;
}

/* Writes `text` to `path` in one write: 0, or -1 with errno. */
static int put(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    ssize_t n = write(fd, text, strlen(text));
    int saved = errno;
    close(fd);
    errno = saved;
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

/* Reads `path` into `out` (trailing newline kept off); "?" if it can't. */
static void get(const char *path, char *out, size_t size) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    snprintf(out, size, "?");
    if (fd < 0) return;
    ssize_t n = read(fd, out, size - 1);
    close(fd);
    if (n < 0) n = 0;
    out[n] = 0;
    while (n > 0 && out[n - 1] == '\n') out[--n] = 0;
}

/* The value of `key` in a flat-keyed file such as memory.events. */
static long long keyed(const char *path, const char *key) {
    char text[4096], *line;
    get(path, text, sizeof text);
    size_t k = strlen(key);
    for (line = strtok(text, "\n"); line; line = strtok(NULL, "\n"))
        if (strncmp(line, key, k) == 0 && line[k] == ' ') return atoll(line + k + 1);
    return -1;
}

static void mk(const char *sub) { if (mkdir(at(sub), 0755) && errno != EEXIST) die(at(sub)); }

/* Moves the calling process into `sub` ("0" is "myself"). */
static int join(const char *sub) {
    char procs[256];
    snprintf(procs, sizeof procs, "%s/cgroup.procs", at(sub));
    return put(procs, "0");
}

static int become(uid_t uid) {
    if (setgroups(0, NULL)) return -1;
    if (setresgid(uid, uid, uid)) return -1;
    return setresuid(uid, uid, uid);
}

static void in_child(void (*check)(void)) {
    pid_t c = fork();
    if (c < 0) die("fork");
    if (c == 0) { check(); _exit(0); }
    waitpid(c, NULL, 0);
}

/* "exited N", "killed by SIGNAME", from a wait status. */
static const char *how(int status) {
    static char text[64];
    if (WIFEXITED(status)) snprintf(text, sizeof text, "exited %d", WEXITSTATUS(status));
    else if (WIFSIGNALED(status)) snprintf(text, sizeof text, "killed by %s", sigabbrev_np(WTERMSIG(status)) ? sigabbrev_np(WTERMSIG(status)) : "?");
    else snprintf(text, sizeof text, "status %#x", status);
    return text;
}

static void sleep_ms(long ms) {
    struct timespec t = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&t, NULL);
}

/* --- The tree ------------------------------------------------------------- */

static void check_tree(void) {
    struct statfs fs;
    int rc = statfs(ROOT, &fs);
    printf("G1 %s is cgroup2: %s\n", ROOT, rc ? strerror(errno) : fs.f_type == CGROUP2_SUPER_MAGIC ? "yes" : "NO (cgroup v1 or hybrid)");
    char text[512];
    get("/proc/self/cgroup", text, sizeof text);
    printf("G1 /proc/self/cgroup: %s\n", text);
    get(ROOT "/cgroup.controllers", text, sizeof text);
    printf("G1 controllers available: %s\n", text);
}

/* G2: "no internal processes": a non-root cgroup can't both hold processes and
 * hand a *domain* controller (memory, io) to children. Which step does the
 * kernel refuse? (Threaded controllers, like pids, are exempt.) */
static void check_internal_processes(void) {
    char control[256], type[64];
    /* Controllers first, then a child for the process. */
    mk("first");
    if (join("first")) die("join first");
    snprintf(control, sizeof control, "%s/cgroup.subtree_control", at("first"));
    int rc = put(control, "+pids");
    printf("G2 enabling pids (threaded) below a cgroup that holds a process: %s\n", err(rc));
    rc = put(control, "+memory");
    printf("G2 enabling memory (a domain controller) there: %s\n", err(rc));
    mk("first/leaf");
    get(at("first/leaf/cgroup.type"), type, sizeof type);
    rc = join("first/leaf");
    printf("G2 then moving the process into a new child (its type: %s): %s\n", type, err(rc));
    /* A child first, then controllers. */
    mk("second");
    mk("second/leaf");
    if (join("second")) die("join second");
    snprintf(control, sizeof control, "%s/cgroup.subtree_control", at("second"));
    rc = put(control, "+memory");
    printf("G2 enabling memory below a cgroup that holds a process and has a child: %s\n", err(rc));
    rc = join("second/leaf");
    printf("G2 moving the process into the child: %s\n", err(rc));
    rc = put(control, "+memory");
    printf("G2 enabling memory, once the process sits in the child: %s\n", err(rc));
}

/* --- Placement ------------------------------------------------------------ */

static void check_clone_into_cgroup(void) {
    mk("born");
    int fd = open(at("born"), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) die("open born");
    struct clone_args args = { .flags = CLONE_INTO_CGROUP, .exit_signal = SIGCHLD, .cgroup = (uint64_t)fd };
    long pid = syscall(SYS_clone3, &args, sizeof args);
    if (pid == 0) {
        char text[256];
        get("/proc/self/cgroup", text, sizeof text);
        printf("G3 a child of clone3(CLONE_INTO_CGROUP) starts in: %s\n", text);
        _exit(0);
    }
    if (pid < 0) {
        printf("G3 clone3 with CLONE_INTO_CGROUP: %s\n", strerror(errno));
    } else {
        waitpid(pid, NULL, 0);
    }
    /* The fallback: the parent writes the child's PID before letting it go. */
    int go[2];
    if (pipe(go)) die("pipe");
    pid_t c = fork();
    if (c == 0) {
        char byte, text[256];
        close(go[1]);
        if (read(go[0], &byte, 1) != 1) _exit(1);
        get("/proc/self/cgroup", text, sizeof text);
        printf("G3 a forked child moved by its parent (cgroup.procs) before going on: %s\n", text);
        _exit(0);
    }
    char procs[256], number[32];
    snprintf(procs, sizeof procs, "%s/cgroup.procs", at("born"));
    snprintf(number, sizeof number, "%d", c);
    if (put(procs, number)) printf("G3 writing the child's PID: %s\n", strerror(errno));
    close(go[0]);
    if (write(go[1], "g", 1) != 1) die("go");
    waitpid(c, NULL, 0);
}

/* --- Limits --------------------------------------------------------------- */

/* Touches `mb` MiB of memory, one page at a time. */
static void eat(size_t mb) {
    size_t size = mb << 20;
    char *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) _exit(3);
    for (size_t i = 0; i < size; i += 4096) p[i] = 1;
}

static void check_memory(void) {
    mk("mem");
    int rc = put(at("mem/memory.max"), "33554432");
    printf("G4 memory.max = 32 MiB: %s\n", err(rc));
    rc = put(at("mem/memory.swap.max"), "0");
    printf("G4 memory.swap.max = 0: %s\n", err(rc));
    pid_t c = fork();
    if (c == 0) { if (join("mem")) _exit(2); eat(128); _exit(0); }
    int status;
    waitpid(c, &status, 0);
    printf("G4 a process touching 128 MiB under it: %s\n", how(status));
    printf("G4 memory.events: oom %lld, oom_kill %lld\n", keyed(at("mem/memory.events"), "oom"), keyed(at("mem/memory.events"), "oom_kill"));

    /* memory.oom.group: the whole cgroup goes, not just the biggest process. */
    mk("group");
    put(at("group/memory.max"), "33554432");
    put(at("group/memory.swap.max"), "0");
    rc = put(at("group/memory.oom.group"), "1");
    printf("G4 memory.oom.group = 1: %s\n", err(rc));
    pid_t idle = fork();
    if (idle == 0) { if (join("group")) _exit(2); pause(); _exit(0); }
    sleep_ms(100);
    pid_t hog = fork();
    if (hog == 0) { if (join("group")) _exit(2); eat(128); _exit(0); }
    waitpid(hog, &status, 0);
    printf("G4 with it, the process over the limit: %s\n", how(status));
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    pid_t got;
    for (int i = 0; i < 20 && (got = waitpid(idle, &status, WNOHANG)) == 0; i++) sleep_ms(50);
    if (got == idle) printf("G4 and an idle process in the same cgroup: %s\n", how(status));
    else { printf("G4 and an idle process in the same cgroup: still running\n"); kill(idle, SIGKILL); waitpid(idle, NULL, 0); }
}

static void check_pids(void) {
    mk("pids");
    int rc = put(at("pids/pids.max"), "5");
    printf("G5 pids.max = 5: %s\n", err(rc));
    pid_t c = fork();
    if (c == 0) {
        if (join("pids")) _exit(2);
        int made = 0;
        pid_t kids[16];
        for (; made < 16; made++) {
            kids[made] = fork();
            if (kids[made] == 0) { pause(); _exit(0); }
            if (kids[made] < 0) break;
        }
        printf("G5 a process there forks %d children, then: %s\n", made, made < 16 ? strerror(errno) : "no limit?!");
        for (int i = 0; i < made; i++) kill(kids[i], SIGKILL);
        for (int i = 0; i < made; i++) waitpid(kids[i], NULL, 0);
        _exit(0);
    }
    waitpid(c, NULL, 0);
    printf("G5 pids.events: max %lld\n", keyed(at("pids/pids.events"), "max"));
}

static void check_cpu(void) {
    mk("cpu");
    int rc = put(at("cpu/cpu.max"), "10000 100000");
    printf("G6 cpu.max = 10%% of one CPU (10000 of every 100000 us): %s\n", err(rc));
    pid_t c = fork();
    if (c == 0) {
        if (join("cpu")) _exit(2);
        struct timespec start, now;
        clock_gettime(CLOCK_MONOTONIC, &start);
        do clock_gettime(CLOCK_MONOTONIC, &now);
        while ((now.tv_sec - start.tv_sec) * 1000 + (now.tv_nsec - start.tv_nsec) / 1000000 < 500);
        _exit(0);
    }
    waitpid(c, NULL, 0);
    printf("G6 after 0.5 s of busy loop there: nr_throttled %lld, usage_usec %lld\n",
           keyed(at("cpu/cpu.stat"), "nr_throttled"), keyed(at("cpu/cpu.stat"), "usage_usec"));
    rc = put(at("cpu/cpu.weight"), "50");
    printf("G6 cpu.weight = 50: %s\n", err(rc));
    rc = put(at("cpu/cpuset.cpus"), "0");
    char text[64];
    get(at("cpu/cpuset.cpus.effective"), text, sizeof text);
    printf("G6 cpuset.cpus = 0: %s (effective: %s)\n", err(rc), text);
}

static void check_io(void) {
    DIR *d = opendir("/sys/block");
    printf("G7 block devices:");
    struct dirent *e;
    char first[32] = "";
    while (d && (e = readdir(d))) {
        if (e->d_name[0] == '.') continue;
        char path[300], dev[32];
        snprintf(path, sizeof path, "/sys/block/%s/dev", e->d_name);
        get(path, dev, sizeof dev);
        printf(" %s (%s)", e->d_name, dev);
        if (!first[0] && strncmp(e->d_name, "loop", 4) && strncmp(e->d_name, "ram", 3)) snprintf(first, sizeof first, "%s", dev);
    }
    if (d) closedir(d);
    printf("\n");
    mk("io");
    if (!first[0]) { printf("G7 no disk to limit\n"); return; }
    char rule[64];
    snprintf(rule, sizeof rule, "%s rbps=1048576 wiops=100", first);
    int rc = put(at("io/io.max"), rule);
    char text[256];
    get(at("io/io.max"), text, sizeof text);
    printf("G7 io.max \"%s\": %s; it reads back: %s\n", rule, err(rc), text);
    rc = put(at("io/io.max"), "999:999 rbps=1");
    printf("G7 io.max for a device that doesn't exist: %s\n", err(rc));
    rc = put(at("io/io.weight"), "default 200");
    printf("G7 io.weight = 200: %s\n", err(rc));
}

/* --- Control -------------------------------------------------------------- */

static void check_freezer(void) {
    mk("frozen");
    volatile long *count = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (count == MAP_FAILED) die("mmap");
    pid_t c = fork();
    if (c == 0) { if (join("frozen")) _exit(2); for (;;) (*count)++; }
    sleep_ms(100);
    int rc = put(at("frozen/cgroup.freeze"), "1");
    for (int i = 0; i < 20 && keyed(at("frozen/cgroup.events"), "frozen") != 1; i++) sleep_ms(10);
    printf("G8 cgroup.freeze = 1: %s; cgroup.events says frozen %lld\n", err(rc), keyed(at("frozen/cgroup.events"), "frozen"));
    long before = *count;
    sleep_ms(200);
    printf("G8 a busy loop there, frozen, over 200 ms: %s\n", *count == before ? "made no progress" : "STILL RAN");
    kill(c, SIGKILL);
    int status;
    waitpid(c, &status, 0);
    printf("G8 SIGKILL to the frozen process: %s\n", how(status));
}

static void check_kill(void) {
    mk("kill");
    pid_t a = fork();
    if (a == 0) { if (join("kill")) _exit(2); pause(); _exit(0); }
    pid_t b = fork();
    if (b == 0) { if (join("kill")) _exit(2); signal(SIGTERM, SIG_IGN); pause(); _exit(0); }
    sleep_ms(100);
    int rc = rmdir(at("kill"));
    printf("G9 rmdir of a cgroup with processes: %s\n", err(rc));
    rc = put(at("kill/cgroup.kill"), "1");
    int sa, sb;
    waitpid(a, &sa, 0);
    waitpid(b, &sb, 0);
    printf("G9 cgroup.kill = 1: %s; one process %s, one ignoring SIGTERM %s\n", err(rc), how(sa), how(sb));
    for (int i = 0; i < 20 && keyed(at("kill/cgroup.events"), "populated") != 0; i++) sleep_ms(10);
    rc = rmdir(at("kill"));
    printf("G9 rmdir once it's empty: %s\n", err(rc));
}

/* --- Namespaces and delegation -------------------------------------------- */

static void check_cgroup_namespace(void) {
    mk("ns");
    mk("ns/inner");
    if (join("ns")) die("join ns");
    if (unshare(CLONE_NEWCGROUP | CLONE_NEWNS)) { printf("G10 unshare: %s\n", strerror(errno)); return; }
    char text[256];
    get("/proc/self/cgroup", text, sizeof text);
    printf("G10 in a new cgroup namespace, /proc/self/cgroup: %s\n", text);
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("private");
    if (mount("tmpfs", "/mnt", "tmpfs", 0, NULL)) die("tmpfs");
    mkdir("/mnt/cg", 0755);
    int rc = mount("cgroup2", "/mnt/cg", "cgroup2", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    printf("G10 mounting cgroup2 there (root): %s\n", err(rc));
    if (rc == 0) {
        struct stat st;
        printf("G10 the mount shows the namespace's own subtree: inner/ %s, shipyard-probe dir %s\n",
               stat("/mnt/cg/inner", &st) == 0 ? "present" : "missing",
               stat("/mnt/cg/ns", &st) == 0 ? "PRESENT (not namespaced!)" : "absent");
    }
}

/* The same, by root of a user namespace that owns the cgroup namespace. */
static void check_cgroup_namespace_in_userns(void) {
    mk("uns");
    if (join("uns")) die("join uns");
    uid_t uid = NOBODY;
    if (become(uid)) die("become");
    /* A change of user makes a process non-dumpable, which locks its
     * /proc/self files, the maps among them (stage 4's U2). */
    prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
    if (unshare(CLONE_NEWUSER)) { printf("G10 user namespace: %s\n", strerror(errno)); return; }
    char map[32];
    snprintf(map, sizeof map, "0 %d 1", uid);
    if (put("/proc/self/setgroups", "deny") || put("/proc/self/uid_map", map) || put("/proc/self/gid_map", map)) {
        printf("G10 maps: %s\n", strerror(errno));
        return;
    }
    if (unshare(CLONE_NEWCGROUP | CLONE_NEWNS)) { printf("G10 unshare in the user namespace: %s\n", strerror(errno)); return; }
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("private");
    if (mount("tmpfs", "/mnt", "tmpfs", 0, NULL)) { printf("G10 tmpfs: %s\n", strerror(errno)); return; }
    mkdir("/mnt/cg", 0755);
    int rc = mount("cgroup2", "/mnt/cg", "cgroup2", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    printf("G10 mounting cgroup2 as root of a user namespace (uid %d outside): %s\n", uid, err(rc));
    if (rc == 0) {
        int w = put("/mnt/cg/cgroup.subtree_control", "+pids");
        printf("G10 writing its cgroup files without owning them on the host: %s\n", err(w));
    }
}

/* Gives `sub` (the directory and the three files systemd's Delegate= does) to `uid`. */
static void delegate(const char *sub, uid_t uid) {
    const char *files[] = { "", "/cgroup.procs", "/cgroup.subtree_control", "/cgroup.threads" };
    for (size_t i = 0; i < sizeof files / sizeof *files; i++) {
        char path[256];
        snprintf(path, sizeof path, "%s%s", at(sub), files[i]);
        if (chown(path, uid, uid)) die(path);
    }
}

static void check_delegation(void) {
    mk("deleg");
    mk("deleg/session");
    mk("other");
    if (put(at("deleg/cgroup.subtree_control"), "+pids +memory")) printf("G11 root enabling pids and memory below the delegated cgroup: %s\n", strerror(errno));
    delegate("deleg", NOBODY);
    /* The delegator (root, as systemd) places the user's first process. */
    if (join("deleg/session")) die("join session");
    if (become(NOBODY)) die("become");
    printf("G11 uid %d, given a cgroup as systemd's Delegate= does:\n", NOBODY);
    int rc = mkdir(at("deleg/box"), 0755);
    printf("G11   makes a child cgroup: %s\n", err(rc));
    rc = put(at("deleg/box/pids.max"), "10");
    printf("G11   sets a limit in it (pids.max): %s\n", err(rc));
    rc = put(at("deleg/pids.max"), "max");
    printf("G11   changes the limit on the delegated cgroup itself: %s\n", err(rc));
    rc = join("deleg/box");
    printf("G11   moves itself into its child: %s\n", err(rc));
    rc = join("other");
    printf("G11   moves itself outside the delegated cgroup: %s\n", err(rc));
    char text[64];
    get(at("deleg/box/cgroup.controllers"), text, sizeof text);
    printf("G11   its child's controllers: %s\n", text);
}

/* --- Devices -------------------------------------------------------------- */

/* A BPF_PROG_TYPE_CGROUP_DEVICE program allowing only /dev/null (char 1:3).
 * Its context is struct bpf_cgroup_dev_ctx { u32 access_type, major, minor }. */
static int load_device_program(void) {
    struct bpf_insn prog[] = {
        { .code = BPF_LDX | BPF_MEM | BPF_W, .dst_reg = 2, .src_reg = 1, .off = 4 },   /* r2 = major */
        { .code = BPF_LDX | BPF_MEM | BPF_W, .dst_reg = 3, .src_reg = 1, .off = 8 },   /* r3 = minor */
        { .code = BPF_ALU64 | BPF_MOV | BPF_K, .dst_reg = 0, .imm = 0 },               /* r0 = deny */
        { .code = BPF_JMP | BPF_JNE | BPF_K, .dst_reg = 2, .off = 2, .imm = 1 },       /* major != 1: exit */
        { .code = BPF_JMP | BPF_JNE | BPF_K, .dst_reg = 3, .off = 1, .imm = 3 },       /* minor != 3: exit */
        { .code = BPF_ALU64 | BPF_MOV | BPF_K, .dst_reg = 0, .imm = 1 },               /* r0 = allow */
        { .code = BPF_JMP | BPF_EXIT },
    };
    union bpf_attr attr;
    memset(&attr, 0, sizeof attr);
    attr.prog_type = BPF_PROG_TYPE_CGROUP_DEVICE;
    attr.insns = (uint64_t)(uintptr_t)prog;
    attr.insn_cnt = sizeof prog / sizeof *prog;
    attr.license = (uint64_t)(uintptr_t)"Apache-2.0";
    return syscall(SYS_bpf, BPF_PROG_LOAD, &attr, sizeof attr);
}

static int attach(int prog, const char *sub) {
    int cg = open(at(sub), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (cg < 0) return -1;
    union bpf_attr attr;
    memset(&attr, 0, sizeof attr);
    attr.target_fd = cg;
    attr.attach_bpf_fd = prog;
    attr.attach_type = BPF_CGROUP_DEVICE;
    return syscall(SYS_bpf, BPF_PROG_ATTACH, &attr, sizeof attr);
}

static void try_open(const char *label, const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    printf("%s opening %s: %s\n", label, path, fd >= 0 ? "ok" : strerror(errno));
    if (fd >= 0) close(fd);
}

static void check_devices(void) {
    mk("dev");
    int prog = load_device_program();
    printf("G12 loading a cgroup device program (root): %s\n", prog >= 0 ? "ok" : strerror(errno));
    if (prog < 0) return;
    int rc = attach(prog, "dev");
    printf("G12 attaching it to a cgroup: %s\n", err(rc));
    pid_t c = fork();
    if (c == 0) {
        if (join("dev")) _exit(2);
        try_open("G12 in that cgroup,", "/dev/null");
        try_open("G12 in that cgroup,", "/dev/zero");
        _exit(0);
    }
    waitpid(c, NULL, 0);
}

/* Rootless: a user with a delegated cgroup, as root of its own user namespace. */
static void check_devices_unprivileged(void) {
    mk("udev");
    delegate("udev", NOBODY);
    mk("udev/box");
    if (chown(at("udev/box"), NOBODY, NOBODY)) die("chown");
    if (join("udev")) die("join udev");
    if (become(NOBODY)) die("become");
    prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
    int prog = load_device_program();
    printf("G12 loading it as uid %d: %s\n", NOBODY, prog >= 0 ? "ok" : strerror(errno));
    if (unshare(CLONE_NEWUSER)) { printf("G12 user namespace: %s\n", strerror(errno)); return; }
    char map[32];
    snprintf(map, sizeof map, "0 %d 1", NOBODY);
    if (put("/proc/self/setgroups", "deny") || put("/proc/self/uid_map", map) || put("/proc/self/gid_map", map))
        printf("G12 maps: %s\n", strerror(errno));
    prog = load_device_program();
    printf("G12 loading it as root of a user namespace: %s\n", prog >= 0 ? "ok" : strerror(errno));
    if (prog < 0) return;
    int rc = attach(prog, "udev/box");
    printf("G12 attaching it there to a cgroup delegated to that user: %s\n", err(rc));
}

/* --- Setup and cleanup ---------------------------------------------------- */

/* Enables, below `dir`, each controller it has (one at a time: one missing
 * mustn't stop the others): 0, or -1 with errno from the first refusal. */
static int enable_all(const char *dir) {
    char path[256], available[256], *name, *save;
    int failed = 0, first_errno = 0;
    snprintf(path, sizeof path, "%s/cgroup.controllers", dir);
    get(path, available, sizeof available);
    snprintf(path, sizeof path, "%s/cgroup.subtree_control", dir);
    for (name = strtok_r(available, " ", &save); name; name = strtok_r(NULL, " ", &save)) {
        char plus[32];
        snprintf(plus, sizeof plus, "+%s", name);
        if (put(path, plus) && !failed) { failed = 1; first_errno = errno; }
    }
    errno = first_errno;
    return failed ? -1 : 0;
}

/* Makes the scratch cgroup usable: its parent (the root of the visible tree)
 * must hand controllers down. A host's root already does (systemd enables
 * them): the probe uses what it's given and never writes there. Only a root
 * that hands down nothing and refuses for holding processes (EBUSY) is a
 * container's cgroup namespace root: then, as docker-in-docker does, its
 * processes move into a leaf first. */
static void set_up(void) {
    char down[256], available[256], all[512] = "";
    get(ROOT "/cgroup.subtree_control", down, sizeof down);
    if (down[0]) return;
    /* All in one write: one at a time, an accepted threaded controller
     * (cpuset) would turn the next domain one's EBUSY into EOPNOTSUPP (G2). */
    get(ROOT "/cgroup.controllers", available, sizeof available);
    for (char *save, *name = strtok_r(available, " ", &save); name; name = strtok_r(NULL, " ", &save)) {
        strncat(all, "+", sizeof all - strlen(all) - 1);
        strncat(all, name, sizeof all - strlen(all) - 1);
        strncat(all, " ", sizeof all - strlen(all) - 1);
    }
    if (put(ROOT "/cgroup.subtree_control", all) == 0) return;
    if (errno != EBUSY) die("enabling controllers at the root");
    printf("setup: this cgroup namespace's root holds processes (EBUSY); moving them to %s/init\n", ROOT);
    if (mkdir(ROOT "/init", 0755) && errno != EEXIST) die("init");
    char text[8192], *line;
    for (int pass = 0; pass < 5; pass++) {
        get(ROOT "/cgroup.procs", text, sizeof text);
        if (!text[0]) break;
        for (line = strtok(text, "\n"); line; line = strtok(NULL, "\n")) put(ROOT "/init/cgroup.procs", line);
    }
    if (enable_all(ROOT)) die("enabling controllers at the root");
}

/* Removes the scratch tree, deepest first. */
static void remove_tree(const char *path) {
    DIR *d = opendir(path);
    if (!d) return;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (e->d_type != DT_DIR || e->d_name[0] == '.') continue;
        char sub[512];
        snprintf(sub, sizeof sub, "%s/%s", path, e->d_name);
        remove_tree(sub);
    }
    closedir(d);
    char kill_file[600];
    snprintf(kill_file, sizeof kill_file, "%s/cgroup.kill", path);
    put(kill_file, "1");
    for (int i = 0; i < 50 && rmdir(path) && errno == EBUSY; i++) sleep_ms(20);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (geteuid() != 0) { printf("run it as root (it drops to an ordinary user itself)\n"); return 2; }
    check_tree();
    set_up();
    snprintf(scratch, sizeof scratch, "%s/shipyard-probe.%d", ROOT, getpid());
    if (mkdir(scratch, 0755)) die("the scratch cgroup");
    if (enable_all(scratch)) printf("setup: some controllers aren't available here: %s\n", strerror(errno));
    char down[256];
    get(at("cgroup.subtree_control"), down, sizeof down);
    printf("setup: the scratch cgroup hands down: %s\n", down);
    in_child(check_internal_processes);
    in_child(check_clone_into_cgroup);
    in_child(check_memory);
    in_child(check_pids);
    in_child(check_cpu);
    in_child(check_io);
    in_child(check_freezer);
    in_child(check_kill);
    in_child(check_cgroup_namespace);
    in_child(check_cgroup_namespace_in_userns);
    in_child(check_delegation);
    in_child(check_devices);
    in_child(check_devices_unprivileged);
    remove_tree(scratch);
    struct stat st;
    printf("scratch cgroup removed: %s\n", stat(scratch, &st) ? "yes" : "NO");
    return 0;
}
