/*
 * Kernel checks behind a container's confinement (stage 4): capabilities,
 * no_new_privs, seccomp, user namespaces, and what root without capabilities
 * can still reach. Each line of output answers one question;
 * docs/stages/04-security.md records the results.
 *
 * Every check runs in a forked child, so the credentials, capabilities and
 * filters it changes die with it. Run it as root in a privileged container:
 * the capability checks need root, and the user-namespace checks drop to an
 * ordinary user (nobody) themselves.
 *
 *   docker run --rm --privileged -v "$PWD/scripts/probes":/p:ro rust:1-bookworm \
 *       sh -c 'gcc -Wall -Wextra -o /tmp/security /p/security.c && /tmp/security'
 *
 * It copies itself into a scratch directory (mkdtemp, mode 0711) as a
 * setuid-root program, and runs that copy (argv[1] = "show") to report what an
 * exec'd program got. That directory is a tmpfs in the probe's own mount
 * namespace, so the copy can't outlive the probe, even one killed with
 * SIGKILL. And run setuid, the copy does nothing but report.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <linux/audit.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sched.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#define NOBODY 65534

/* The scratch directory (from mkdtemp: no predictable name in /tmp), which
 * the probe works in, and the setuid-root copy in it. */
static char scratch[] = "/tmp/shipyard-probe-security.XXXXXX";
static char suid[sizeof scratch + 8];

static const char *err(int rc) { return rc == 0 ? "ok" : strerror(errno); }

static void die(const char *what) { printf("probe setup failed: %s: %s\n", what, strerror(errno)); exit(2); }

/* A line of /proc/self/status, like "CapEff:\t0000000000000000". */
static void status_field(const char *field, char *out, size_t size) {
    FILE *f = fopen("/proc/self/status", "r");
    char line[256];
    snprintf(out, size, "?");
    if (!f) return;
    size_t n = strlen(field);
    while (fgets(line, sizeof line, f))
        if (strncmp(line, field, n) == 0 && line[n] == ':') {
            char *v = line + n + 1;
            while (*v == '\t' || *v == ' ') v++;
            v[strcspn(v, "\n")] = 0;
            snprintf(out, size, "%s", v);
        }
    fclose(f);
}

/* The capability sets, through the raw capget/capset calls (no libcap). */
struct caps { uint64_t effective, permitted, inheritable; };

static int get_caps(struct caps *c) {
    struct __user_cap_header_struct h = { _LINUX_CAPABILITY_VERSION_3, 0 };
    struct __user_cap_data_struct d[2];
    if (syscall(SYS_capget, &h, d)) return -1;
    c->effective = d[0].effective | (uint64_t)d[1].effective << 32;
    c->permitted = d[0].permitted | (uint64_t)d[1].permitted << 32;
    c->inheritable = d[0].inheritable | (uint64_t)d[1].inheritable << 32;
    return 0;
}

static int set_caps(const struct caps *c) {
    struct __user_cap_header_struct h = { _LINUX_CAPABILITY_VERSION_3, 0 };
    struct __user_cap_data_struct d[2] = {
        { (uint32_t)c->effective, (uint32_t)c->permitted, (uint32_t)c->inheritable },
        { (uint32_t)(c->effective >> 32), (uint32_t)(c->permitted >> 32), (uint32_t)(c->inheritable >> 32) },
    };
    return syscall(SYS_capset, &h, d);
}

#define BIT(cap) ((uint64_t)1 << (cap))

/* Becomes `uid` fully: no supplementary groups, all three IDs. */
static int become(uid_t uid) {
    if (setgroups(0, NULL)) return -1;
    if (setresgid(uid, uid, uid)) return -1;
    return setresuid(uid, uid, uid);
}

/* Runs `check` in a forked child and waits for it. */
static void in_child(void (*check)(void)) {
    pid_t c = fork();
    if (c < 0) die("fork");
    if (c == 0) { check(); _exit(0); }
    waitpid(c, NULL, 0);
}

/* Runs the setuid-root copy (or /proc/self/exe) with "show" and lets it
 * print what it got. */
static void show(const char *program, const char *label) {
    pid_t c = fork();
    if (c < 0) die("fork");
    if (c == 0) {
        execl(program, program, "show", label, (char *)NULL);
        printf("   exec %s: %s\n", program, strerror(errno));
        _exit(127);
    }
    waitpid(c, NULL, 0);
}

/* --- Capabilities (TLPI ch. 39, capabilities(7)) ---------------------------- */

static void check_bounding_needs_setpcap(void) {
    struct caps c;
    if (get_caps(&c)) die("capget");
    c.effective &= ~BIT(CAP_SETPCAP);
    if (set_caps(&c)) die("capset");
    int rc = prctl(PR_CAPBSET_DROP, CAP_SYS_ADMIN, 0, 0, 0);
    printf("C1 PR_CAPBSET_DROP without CAP_SETPCAP in effective: %s\n", err(rc));
    c.effective |= BIT(CAP_SETPCAP);
    if (set_caps(&c)) die("capset");
    rc = prctl(PR_CAPBSET_DROP, CAP_SYS_ADMIN, 0, 0, 0);
    printf("C1 with it: %s; CAP_SYS_ADMIN still bounding: %d\n", err(rc),
           prctl(PR_CAPBSET_READ, CAP_SYS_ADMIN, 0, 0, 0));
}

static void check_setuid_clears(void) {
    char prm[64], eff[64];
    if (become(NOBODY)) die("become");
    status_field("CapPrm", prm, sizeof prm);
    status_field("CapEff", eff, sizeof eff);
    printf("C2 root -> uid %d without PR_SET_KEEPCAPS: permitted %s, effective %s\n", NOBODY, prm, eff);
}

static void check_keepcaps(void) {
    char prm[64], eff[64];
    if (prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0)) die("keepcaps");
    if (become(NOBODY)) die("become");
    status_field("CapPrm", prm, sizeof prm);
    status_field("CapEff", eff, sizeof eff);
    printf("C3 the same with PR_SET_KEEPCAPS: permitted %s, effective %s\n", prm, eff);
}

static void check_ambient(void) {
    struct caps c = { BIT(CAP_NET_BIND_SERVICE), BIT(CAP_NET_BIND_SERVICE), BIT(CAP_NET_BIND_SERVICE) };
    if (prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0)) die("keepcaps");
    if (become(NOBODY)) die("become");
    int rc = set_caps(&c);
    int amb = prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, CAP_NET_BIND_SERVICE, 0, 0);
    printf("C4 uid %d keeps CAP_NET_BIND_SERVICE (capset %s), raises it ambient: %s; after exec:\n",
           NOBODY, err(rc), err(amb));
    show("/proc/self/exe", "C4");
}

static void check_bounding_vs_setuid(void) {
    if (prctl(PR_CAPBSET_DROP, CAP_SYS_ADMIN, 0, 0, 0)) die("bounding");
    if (become(NOBODY)) die("become");
    printf("C5 CAP_SYS_ADMIN dropped from the bounding set, then a setuid-root program:\n");
    show(suid, "C5");
}

/* --- no_new_privs ------------------------------------------------------------ */

static void check_nnp_setuid(void) {
    if (become(NOBODY)) die("become");
    printf("N1 uid %d runs a setuid-root program without no_new_privs:\n", NOBODY);
    show(suid, "N1a");
    int rc = prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    printf("N1 PR_SET_NO_NEW_PRIVS: %s; the same program:\n", err(rc));
    show(suid, "N1b");
    rc = prctl(PR_SET_NO_NEW_PRIVS, 0, 0, 0, 0);
    printf("N3 unsetting it: %s; still set: %d\n", err(rc), prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0));
}

static void check_nnp_inherited(void) {
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) die("nnp");
    pid_t c = fork();
    if (c == 0) {
        printf("N2 after fork, set: %d; after exec:\n", prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0));
        show("/proc/self/exe", "N2");
        _exit(0);
    }
    waitpid(c, NULL, 0);
}

/* --- seccomp (seccomp(2)) ----------------------------------------------------- */

/* A filter: `uname` returns ERRNO(the low 16 bits of seccomp_data.arch), and
 * `getppid` returns ERRNO(the syscall number the filter saw for it, which
 * should be __NR_getppid); everything else is allowed. */
static int install_report_filter(void) {
    /* Jump offsets count the instructions skipped: "0, 2" goes on to the
     * next if equal, else skips two. */
    struct sock_filter f[] = {
        /* 0 */ BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        /* 1 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_uname, 0, 2),  /* uname: 2, else 4 */
        /* 2 */ BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        /* 3 */ BPF_JUMP(BPF_JMP | BPF_JA, 2, 0, 0),                    /* to 6, with the arch */
        /* 4 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getppid, 1, 0), /* getppid: 6, with nr */
        /* 5 */ BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        /* 6 */ BPF_STMT(BPF_ALU | BPF_AND | BPF_K, SECCOMP_RET_DATA),   /* the value as errno */
        /* 7 */ BPF_STMT(BPF_ALU | BPF_OR | BPF_K, SECCOMP_RET_ERRNO),
        /* 8 */ BPF_STMT(BPF_RET | BPF_A, 0),
    };
    struct sock_fprog prog = { sizeof f / sizeof *f, f };
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog);
}

static void check_seccomp_needs_nnp(void) {
    if (become(NOBODY)) die("become");
    int rc = install_report_filter();
    printf("S1 uid %d installs a filter without no_new_privs: %s\n", NOBODY, err(rc));
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) die("nnp");
    rc = install_report_filter();
    printf("S1 with no_new_privs: %s\n", err(rc));
}

static void check_seccomp_report(void) {
    if (install_report_filter()) { printf("S2 installing as root: %s\n", strerror(errno)); return; }
    struct utsname u;
    int rc = uname(&u);
    int arch = errno;
    printf("S2 the filter's view: uname -> %s (arch low 16 bits: 0x%04x; AUDIT_ARCH_AARCH64 0x%04x, X86_64 0x%04x)\n",
           rc ? "ERRNO" : "allowed?!", rc ? arch : 0, AUDIT_ARCH_AARCH64 & 0xffff, AUDIT_ARCH_X86_64 & 0xffff);
    errno = 0;
    long got = syscall(SYS_getppid);
    printf("S3 getppid -> %s %d (this build's __NR_getppid is %d)\n",
           got < 0 ? "ERRNO" : "allowed?!", got < 0 ? errno : 0, __NR_getppid);
    printf("S4 after exec, the filter still applies:\n");
    show("/proc/self/exe", "S4");
}

/* --- User namespaces (user_namespaces(7)) --------------------------------------- */

static int write_file(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    ssize_t n = write(fd, text, strlen(text));
    int saved = errno;
    close(fd);
    errno = saved;
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

static void print_file(const char *label, const char *path) {
    char text[128] = "(absent)";
    FILE *f = fopen(path, "r");
    if (f) { if (!fgets(text, sizeof text, f)) snprintf(text, sizeof text, "(empty)"); fclose(f); }
    text[strcspn(text, "\n")] = 0;
    printf("U0 %s: %s\n", label, text);
}

/* Inside the new user, mount, PID, IPC and UTS namespaces, as root of the
 * user namespace (PID 1 of the new PID namespace). */
static void inside_userns(void) {
    char where[64];
    printf("U4 inside: uid %d, CapEff ", getuid());
    status_field("CapEff", where, sizeof where);
    printf("%s\n", where);
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) printf("U4 / private: %s\n", strerror(errno));
    int rc = mount("tmpfs", "mnt", "tmpfs", 0, "size=1m");
    printf("U4 tmpfs: %s\n", err(rc));
    mkdir("mnt/proc", 0755); mkdir("mnt/sys", 0755); mkdir("mnt/pts", 0755);
    mkdir("mnt/mq", 0755); mkdir("mnt/root", 0755); mkdir("mnt/usr", 0755);
    rc = mount("proc", "mnt/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    printf("U5 proc (our own PID namespace): %s\n", err(rc));
    rc = mount("sysfs", "mnt/sys", "sysfs", MS_RDONLY, NULL);
    printf("U6 sysfs (the host's network namespace): %s\n", err(rc));
    rc = mount("/sys", "mnt/sys", NULL, MS_BIND | MS_REC, NULL);
    printf("U6 a recursive bind of the host's /sys instead: %s\n", err(rc));
    rc = mknod("mnt/null", S_IFCHR | 0666, makedev(1, 3));
    printf("U7 mknod /dev/null: %s\n", err(rc));
    int fd = open("mnt/null", O_CREAT | O_WRONLY, 0666); if (fd >= 0) close(fd);
    rc = mount("/dev/null", "mnt/null", NULL, MS_BIND, NULL);
    fd = open("mnt/null", O_WRONLY);
    printf("U7 a bind of the host's /dev/null instead: %s; writable: %s\n", err(rc), fd >= 0 ? "yes" : strerror(errno));
    if (fd >= 0) close(fd);
    rc = mount("devpts", "mnt/pts", "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=620");
    printf("U8 devpts newinstance: %s\n", err(rc));
    rc = mount("mqueue", "mnt/mq", "mqueue", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    printf("U8 mqueue (our own IPC namespace): %s\n", err(rc));
    rc = sethostname("probe", 5);
    printf("U8 sethostname: %s\n", err(rc));
    char path[128];
    snprintf(path, sizeof path, "mnt/proc/sys/kernel/shmmni");
    rc = write_file(path, "4000");
    printf("U8 an IPC sysctl (kernel.shmmni): %s\n", err(rc));
    /* Mounts carried over from the host are locked: their flags can be added
     * to, never taken away (mount_setattr only sets). */
    rc = mount("/usr", "mnt/usr", NULL, MS_BIND | MS_REC, NULL);
    printf("U9 binding the host's /usr: %s\n", err(rc));
    struct { uint64_t set, clr, propagation, userns; } attr = { 1 /* RDONLY */, 0, 0, 0 };
    int ufd = open("mnt/usr", O_PATH | O_CLOEXEC);
    rc = syscall(442, ufd, "", 0x1000 /* AT_EMPTY_PATH */ | 0x8000 /* AT_RECURSIVE */, &attr, sizeof attr);
    printf("U9 mount_setattr adding read-only, recursively: %s\n", err(rc));
    attr.set = 0; attr.clr = 1;
    rc = syscall(442, ufd, "", 0x1000, &attr, sizeof attr);
    printf("U9 mount_setattr clearing it again (we set it, so it isn't locked): %s\n", err(rc));
    rc = mount(NULL, "/proc", NULL, MS_REMOUNT | MS_BIND, NULL);
    printf("U9 remounting the host's /proc without its nosuid,nodev,noexec (locked): %s\n", err(rc));
    if (ufd >= 0) close(ufd);
    /* pivot_root into a tmpfs root. */
    rc = mount("tmpfs", "mnt/root", "tmpfs", 0, "size=1m");
    mkdir("mnt/root/old", 0755);
    rc = rc ? rc : syscall(SYS_pivot_root, "mnt/root", "mnt/root/old");
    printf("U10 pivot_root: %s\n", err(rc));
}

static void check_userns(void) {
    print_file("kernel.unprivileged_userns_clone", "/proc/sys/kernel/unprivileged_userns_clone");
    print_file("kernel.apparmor_restrict_unprivileged_userns", "/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    print_file("user.max_user_namespaces", "/proc/sys/user/max_user_namespaces");
    printf("U0 newuidmap: %s; newgidmap: %s\n",
           access("/usr/bin/newuidmap", X_OK) ? "missing" : "present",
           access("/usr/bin/newgidmap", X_OK) ? "missing" : "present");
    if (become(NOBODY)) die("become");
    int rc = unshare(CLONE_NEWUSER);
    printf("U1 uid %d: unshare(CLONE_NEWUSER): %s\n", NOBODY, err(rc));
    if (rc) return;
    char map[64];
    snprintf(map, sizeof map, "0 %d 1\n", NOBODY);
    /* Changing credentials (root -> nobody) made this process non-dumpable,
     * so its /proc/self files belong to root. */
    rc = write_file("/proc/self/uid_map", map);
    printf("U2 writing its own uid_map (one ID), non-dumpable after the credential change: %s\n", err(rc));
    if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0)) die("dumpable");
    rc = write_file("/proc/self/uid_map", map);
    printf("U2 the same once dumpable again: %s\n", err(rc));
    rc = write_file("/proc/self/uid_map", map);
    printf("U2 writing uid_map a second time: %s\n", err(rc));
    rc = write_file("/proc/self/gid_map", map);
    printf("U3 gid_map before denying setgroups: %s\n", err(rc));
    rc = write_file("/proc/self/setgroups", "deny");
    int rc2 = write_file("/proc/self/gid_map", map);
    printf("U3 setgroups deny: %s; then gid_map: %s\n", err(rc), err(rc2));
    if (unshare(CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWIPC | CLONE_NEWUTS)) {
        printf("U4 the other namespaces: %s\n", strerror(errno)); return;
    }
    pid_t c = fork();
    if (c == 0) { inside_userns(); _exit(0); }
    waitpid(c, NULL, 0);
}

/* --- What root without CAP_SYS_PTRACE can reach --------------------------------- */

static void check_proc1(void) {
    if (unshare(CLONE_NEWPID | CLONE_NEWNS)) { printf("P1 unshare: %s\n", strerror(errno)); return; }
    pid_t init = fork();
    if (init == 0) {
        /* PID 1: not dumpable, holding a descriptor, waiting for its child. */
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) ||
            mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) die("proc");
        prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
        pid_t c = fork();
        if (c == 0) {
            int fd = open("/proc/1/fd/0", O_RDONLY);
            printf("P1 root with every capability opens /proc/1/fd/0 of a non-dumpable PID 1: %s\n",
                   fd >= 0 ? "ok" : strerror(errno));
            if (fd >= 0) close(fd);
            struct caps cur;
            if (get_caps(&cur)) die("capget");
            cur.effective &= ~BIT(CAP_SYS_PTRACE); cur.permitted &= ~BIT(CAP_SYS_PTRACE);
            if (set_caps(&cur)) die("capset");
            fd = open("/proc/1/fd/0", O_RDONLY);
            printf("P1 without CAP_SYS_PTRACE: %s\n", fd >= 0 ? "ok" : strerror(errno));
            _exit(0);
        }
        waitpid(c, NULL, 0);
        _exit(0);
    }
    waitpid(init, NULL, 0);
}

/* Copies this program to `to` (in C: `system()`'s shell would copy itself). */
static void copy_self(const char *to) {
    int in = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    int out = open(to, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0755);
    if (in < 0 || out < 0) die("copying the probe");
    char buf[65536];
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0)
        if (write(out, buf, n) != n) die("copying the probe");
    close(in);
    close(out);
}

/* --- The copy started with "show" --------------------------------------------- */

static int show_main(const char *label) {
    char eff[64], amb[64];
    status_field("CapEff", eff, sizeof eff);
    status_field("CapAmb", amb, sizeof amb);
    struct utsname u;
    int urc = uname(&u);
    int uerr = errno;
    printf("   [%s] euid %d, CapEff %s, CapAmb %s, no_new_privs %d, CAP_SYS_ADMIN effective: %s, uname: %s",
           label, geteuid(), eff, amb, prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0),
           strtoull(eff, NULL, 16) & BIT(CAP_SYS_ADMIN) ? "yes" : "no",
           urc == 0 ? "allowed" : "refused");
    if (urc) printf(" (errno %d)", uerr);
    printf("\n");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc >= 3 && strcmp(argv[1], "show") == 0) return show_main(argv[2]);
    /* Run setuid (the copy, by anyone), it only ever reports: otherwise any
     * user could run the root-only checks through a leftover copy. */
    if (getuid() != geteuid() || getgid() != getegid()) {
        printf("this is the probe's setuid copy: it only reports (\"show\")\n");
        return 2;
    }
    if (geteuid() != 0) { printf("run it as root (it drops to an ordinary user itself)\n"); return 2; }
    /* The scratch files live on a tmpfs in a mount namespace of the probe's
     * own: when its last process dies, even by SIGKILL, the namespace and
     * the tmpfs (with the setuid copy) go with it. At worst an empty
     * directory is left. 0711: nobody can reach the copy to run it, but not
     * list the directory. */
    if (!mkdtemp(scratch)) die("the scratch directory");
    if (unshare(CLONE_NEWNS) || mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) ||
        mount("tmpfs", scratch, "tmpfs", 0, "mode=0711,size=64m") || chdir(scratch))
        die("the scratch tmpfs");
    snprintf(suid, sizeof suid, "%s/suid", scratch);
    if (mkdir("mnt", 0755)) die("mnt");
    copy_self(suid);
    if (chown(suid, 0, 0) || chmod(suid, 04755)) die("setuid copy");
    in_child(check_bounding_needs_setpcap);
    in_child(check_setuid_clears);
    in_child(check_keepcaps);
    in_child(check_ambient);
    in_child(check_bounding_vs_setuid);
    in_child(check_nnp_setuid);
    in_child(check_nnp_inherited);
    /* The last check that runs it: the setuid copy goes at once. */
    if (unlink(suid)) die("removing the setuid copy");
    in_child(check_seccomp_needs_nnp);
    in_child(check_seccomp_report);
    in_child(check_userns);
    in_child(check_proc1);
    if (chdir("/") || umount2(scratch, MNT_DETACH) || rmdir(scratch)) die("removing the scratch directory");
    printf("scratch files removed: %s\n", access(scratch, F_OK) ? "yes" : "NO");
    return 0;
}
