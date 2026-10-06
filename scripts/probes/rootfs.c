/*
 * Kernel checks behind a container's own root filesystem (stage 3): pivot_root
 * and the old root, mount propagation, /proc, devpts, mqueue, sysfs, read-only
 * and masked paths, device nodes, and symlinks planted at mount targets.
 * Each line of output answers one question; docs/stages/03-filesystem.md
 * records the results.
 *
 * Every check runs in a forked child with its own mount namespace, so the
 * mount table it runs in is left exactly as it was. Needs CAP_SYS_ADMIN, so
 * run it in a privileged container:
 *
 *   docker run --rm --privileged -v "$PWD/scripts/probes":/p:ro rust:1-bookworm \
 *       sh -c 'gcc -Wall -Wextra -o /tmp/rootfs /p/rootfs.c && /tmp/rootfs'
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

#define ROOT "/tmp/shipyard-probe-rootfs"
#define OUTSIDE "/tmp/shipyard-probe-outside"

static const char *err(int rc) { return rc == 0 ? "ok" : strerror(errno); }

static void die(const char *what) { printf("probe setup failed: %s: %s\n", what, strerror(errno)); exit(2); }

/* `system()` sets no errno: its failure is the command's. */
static void run(const char *command) {
    if (system(command)) { printf("probe setup failed: %s\n", command); exit(2); }
}

static int pivot_root(const char *new_root, const char *put_old) {
    return syscall(SYS_pivot_root, new_root, put_old);
}

/* A fresh scratch root filesystem: the directories the checks mount on, a
 * few files to read, write and mask, and the planted symlink (`evil`) that
 * points at a directory outside it. */
static void make_rootfs(void) {
    const char *dirs[] = { "", "/proc", "/dev", "/dev/pts", "/dev/mqueue", "/sys", "/tmp",
                           "/etc", "/etc/sub", "/etc/secretdir" };
    run("rm -rf " ROOT " " OUTSIDE);
    for (size_t i = 0; i < sizeof dirs / sizeof *dirs; i++) {
        char path[256]; snprintf(path, sizeof path, ROOT "%s", dirs[i]);
        if (mkdir(path, 0755)) die(path);
    }
    FILE *f = fopen(ROOT "/etc/file", "w"); if (!f) die("etc/file"); fputs("data\n", f); fclose(f);
    f = fopen(ROOT "/etc/secret", "w"); if (!f) die("etc/secret"); fputs("secret\n", f); fclose(f);
    f = fopen(ROOT "/etc/secretdir/inside", "w"); if (!f) die("secretdir"); fclose(f);
    if (mkdir(OUTSIDE, 0755)) die(OUTSIDE);
    if (symlink(OUTSIDE, ROOT "/evil")) die("symlink");
}

/* Runs `check` on a fresh scratch root, in a child with its own mount
 * namespace (plus `extra` namespaces). The tree is made private first, so
 * nothing the check mounts reaches the namespace the probe runs in, even when
 * that one's `/` is shared (a systemd host, or `scripts/test-linux.sh
 * --root-shell`); then `propagation` is applied (shipyard uses MS_SLAVE; with
 * MS_SHARED that makes a peer group of the check's own). */
static void in_namespace(int extra, unsigned long propagation, void (*check)(void)) {
    make_rootfs();
    pid_t c = fork();
    if (c < 0) die("fork");
    if (c == 0) {
        if (unshare(CLONE_NEWNS | extra)) { printf("  unshare: %s\n", strerror(errno)); _exit(1); }
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)
            || mount(NULL, "/", NULL, MS_REC | propagation, NULL)) {
            printf("  propagation: %s\n", strerror(errno)); _exit(1);
        }
        if (extra & CLONE_NEWPID) {
            /* Only children are born in the new PID namespace. */
            pid_t g = fork();
            if (g == 0) { check(); _exit(0); }
            waitpid(g, NULL, 0);
            _exit(0);
        }
        check();
        _exit(0);
    }
    waitpid(c, NULL, 0);
}

static int bind_root(void) { return mount(ROOT, ROOT, NULL, MS_BIND | MS_REC, NULL); }

static int mounts(void) {
    FILE *f = fopen("/proc/self/mountinfo", "r"); if (!f) return -1;
    int n = 0; char line[4096]; while (fgets(line, sizeof line, f)) n++; fclose(f); return n;
}

static int count_entries(const char *dir, int digits_only) {
    DIR *d = opendir(dir); if (!d) return -1;
    int n = 0; struct dirent *e;
    while ((e = readdir(d))) {
        if (e->d_name[0] == '.') continue;
        if (digits_only && (e->d_name[0] < '0' || e->d_name[0] > '9')) continue;
        n++;
    }
    closedir(d); return n;
}

static int is_mount_point(const char *path) {
    char parent[512]; struct stat a, b;
    snprintf(parent, sizeof parent, "%s/..", path);
    if (stat(path, &a) || stat(parent, &b)) return -1;
    return a.st_dev != b.st_dev;
}

/* 1: the new root must be a mount point. */
static void check_needs_mount_point(void) {
    if (chdir(ROOT)) die("chdir");
    int rc = pivot_root(".", ".");
    printf("1 pivot_root into a plain directory: %s\n", err(rc));
    if (bind_root() || chdir("/") || chdir(ROOT)) die("bind");
    rc = pivot_root(".", ".");
    printf("1 after bind-mounting it onto itself: %s\n", err(rc));
}

/* 2: pivot_root and shared propagation. */
static void check_pivot(void) {
    if (bind_root() || chdir(ROOT)) die("bind");
    printf("   pivot_root: %s\n", err(pivot_root(".", ".")));
}
static void check_shared(void) { printf("2 with / shared:\n"); check_pivot(); }
static void check_slave(void) { printf("2 with / slave (shipyard's choice):\n"); check_pivot(); }

/* 3, 4: the old root detached; /proc from inside a new PID namespace. */
static void check_detach_and_proc(void) {
    if (bind_root()) die("bind");
    if (mount("proc", ROOT "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) die("proc");
    if (chdir(ROOT) || pivot_root(".", ".")) die("pivot_root");
    int rc = umount2(".", MNT_DETACH);
    if (chdir("/")) die("chdir /");
    printf("3 umount2(\".\", MNT_DETACH) of the old root: %s\n", err(rc));
    printf("3 a host path afterwards (/usr/bin): %s\n", access("/usr/bin", F_OK) == 0 ? "REACHABLE" : strerror(errno));
    printf("3 mounts left: %d (just the new root and /proc)\n", mounts());
    printf("4 processes in /proc: %d (only this namespace's)\n", count_entries("/proc", 1));
    printf("4 /proc/1/root resolves to: ");
    char buf[256]; ssize_t n = readlink("/proc/1/root", buf, sizeof buf - 1);
    printf("%s\n", n < 0 ? strerror(errno) : (buf[n] = 0, buf));
    /* What root inside could do: unmount /proc. Nothing must be under it. */
    int rc2 = umount2("/proc", MNT_DETACH);
    printf("4 after unmounting /proc inside: %s; entries left in /proc: %d (the root filesystem's own empty directory)\n",
           err(rc2), count_entries("/proc", 0));
}

/* 5: a devpts instance of the container's own. */
static void check_devpts(void) {
    int rc = mount("devpts", ROOT "/dev/pts", "devpts", MS_NOSUID | MS_NOEXEC,
                   "newinstance,ptmxmode=0666,mode=0620");
    printf("5 devpts newinstance: %s\n", err(rc));
    if (rc) return;
    if (symlink("pts/ptmx", ROOT "/dev/ptmx")) die("ptmx symlink");
    int fd = open(ROOT "/dev/ptmx", O_RDWR | O_NOCTTY);
    unsigned n = 0;
    if (fd < 0 || ioctl(fd, TIOCGPTN, &n)) { printf("5 opening /dev/ptmx: %s\n", strerror(errno)); return; }
    struct stat inside, host;
    char path[256]; snprintf(path, sizeof path, ROOT "/dev/pts/%u", n);
    if (stat(ROOT "/dev/pts", &inside) || stat("/dev/pts", &host)) die("stat /dev/pts");
    printf("5 pty %u created inside: %s; its own instance (st_dev differs from the host's /dev/pts): %s\n",
           n, access(path, F_OK) == 0 ? "yes" : "NO", inside.st_dev != host.st_dev ? "yes" : "NO");
}

/* 6: mqueue shows the IPC namespace of whoever mounted it. */
static void check_mqueue(void) {
    int rc = mount("mqueue", ROOT "/dev/mqueue", "mqueue", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    printf("   mqueue mount: %s; the probe's queue visible: %s\n", err(rc),
           access(ROOT "/dev/mqueue/shipyard-probe", F_OK) == 0 ? "yes (the host's)" : "no");
}
static void check_mqueue_same_ipc(void) { printf("6 without a new IPC namespace:\n"); check_mqueue(); }
static void check_mqueue_new_ipc(void) { printf("6 with a new IPC namespace:\n"); check_mqueue(); }

/* 7: sysfs follows the network namespace. */
static void check_sysfs(void) {
    int rc = mount("sysfs", ROOT "/sys", "sysfs", MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    printf("   sysfs mount: %s; network devices listed: %d\n", err(rc),
           rc ? -1 : count_entries(ROOT "/sys/class/net", 0));
}
static void check_sysfs_host_net(void) { printf("7 without a new network namespace:\n"); check_sysfs(); }
static void check_sysfs_new_net(void) { printf("7 with a new network namespace:\n"); check_sysfs(); }

/* 8: read-only binds, and their submounts. */
static void check_readonly(void) {
    if (mount("tmpfs", ROOT "/etc/sub", "tmpfs", 0, NULL)) die("submount");
    int rc = mount(ROOT "/etc", ROOT "/etc", NULL, MS_BIND | MS_REC, NULL);
    rc = rc ? rc : mount(NULL, ROOT "/etc", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL);
    printf("8 read-only bind remount: %s\n", err(rc));
    int fd = open(ROOT "/etc/file", O_WRONLY);
    printf("8 writing below it: %s\n", fd >= 0 ? "ALLOWED" : strerror(errno));
    fd = open(ROOT "/etc/sub/x", O_WRONLY | O_CREAT, 0644);
    printf("8 writing in a submount of the recursive bind: %s\n", fd >= 0 ? "allowed (submounts stay writable)" : strerror(errno));
}

/* 8b: a read-only bind remount replaces the mount's flags. */
static int has_flag(const char *path, unsigned long flag) {
    struct statvfs v; return statvfs(path, &v) == 0 && (v.f_flag & flag);
}
static void check_remount_flags(void) {
    if (mount("tmpfs", ROOT "/etc/sub", "tmpfs", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) die("flagged tmpfs");
    if (mount(ROOT "/etc/sub", ROOT "/tmp", NULL, MS_BIND, NULL)) die("bind");
    int rc = mount(NULL, ROOT "/tmp", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL);
    printf("8b remount ro with only MS_RDONLY: %s; nosuid kept: %s, nodev kept: %s, noexec kept: %s\n", err(rc),
           has_flag(ROOT "/tmp", ST_NOSUID) ? "yes" : "NO", has_flag(ROOT "/tmp", ST_NODEV) ? "yes" : "NO",
           has_flag(ROOT "/tmp", ST_NOEXEC) ? "yes" : "NO");
    rc = mount(NULL, ROOT "/tmp", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    printf("8b remount ro with the existing flags repeated: %s; nosuid %s, nodev %s, noexec %s, read-only %s\n", err(rc),
           has_flag(ROOT "/tmp", ST_NOSUID) ? "yes" : "NO", has_flag(ROOT "/tmp", ST_NODEV) ? "yes" : "NO",
           has_flag(ROOT "/tmp", ST_NOEXEC) ? "yes" : "NO", has_flag(ROOT "/tmp", ST_RDONLY) ? "yes" : "NO");
}

/* 14: a root filesystem on a noexec mount. */
static void check_noexec_root(void) {
    if (mount("tmpfs", ROOT "/tmp", "tmpfs", MS_NOEXEC, NULL)) die("noexec tmpfs");
    run("cp /bin/true " ROOT "/tmp/true");
    /* The container's root is a recursive bind; the noexec submount comes along. */
    if (bind_root()) die("bind");
    printf("14 a program on a noexec mount under the root: executable: %s\n",
           access(ROOT "/tmp/true", X_OK) == 0 ? "yes" : strerror(errno));
    int rc = mount(NULL, ROOT "/tmp", NULL, MS_REMOUNT | MS_BIND, NULL);
    printf("14 root can remount it without noexec: %s; executable now: %s\n", err(rc),
           access(ROOT "/tmp/true", X_OK) == 0 ? "yes" : strerror(errno));
}

/* 9: masking a file and a directory. */
static void check_masking(void) {
    int rc = mount("/dev/null", ROOT "/etc/secret", NULL, MS_BIND, NULL);
    char buf[16]; int fd = open(ROOT "/etc/secret", O_RDONLY);
    ssize_t n = fd < 0 ? -1 : read(fd, buf, sizeof buf);
    printf("9 /dev/null bound over a file: %s; reading it gives %zd bytes\n", err(rc), n);
    rc = mount("tmpfs", ROOT "/etc/secretdir", "tmpfs", MS_RDONLY, "size=0");
    printf("9 read-only tmpfs over a directory: %s; entries visible: %d\n", err(rc),
           count_entries(ROOT "/etc/secretdir", 0));
}

/* 10: device nodes in a tmpfs, with and without nodev. */
static void check_devices(void) {
    if (mount("tmpfs", ROOT "/dev", "tmpfs", MS_NOSUID, "mode=755")) die("dev tmpfs");
    int rc = mknod(ROOT "/dev/null", S_IFCHR | 0666, makedev(1, 3));
    int fd = rc ? -1 : open(ROOT "/dev/null", O_WRONLY);
    printf("10 mknod /dev/null in a tmpfs: %s; opening it: %s\n", err(rc), fd >= 0 ? "ok" : strerror(errno));
    if (mount("tmpfs", ROOT "/tmp", "tmpfs", MS_NOSUID | MS_NODEV, NULL)) die("nodev tmpfs");
    rc = mknod(ROOT "/tmp/null", S_IFCHR | 0666, makedev(1, 3));
    fd = rc ? -1 : open(ROOT "/tmp/null", O_WRONLY);
    printf("10 the same in a nodev tmpfs: mknod %s; opening it: %s\n", err(rc), fd >= 0 ? "ok" : strerror(errno));
}

/* 11, 12, 13: symlinks and missing directories at mount targets. */
struct open_how { uint64_t flags, mode, resolve; };
#define RESOLVE_NO_MAGICLINKS 0x02
#define RESOLVE_IN_ROOT 0x10
#ifndef SYS_openat2
#define SYS_openat2 437
#endif

static void check_targets(void) {
    /* 11: the target is a symlink, planted in the root filesystem, that
     * points outside it. */
    int rc = mount("tmpfs", ROOT "/evil", "tmpfs", 0, NULL);
    printf("11 mounting at a planted symlink (" ROOT "/evil -> " OUTSIDE "): %s; "
           "the mount landed outside the root filesystem: %s\n",
           err(rc), is_mount_point(OUTSIDE) == 1 ? "YES" : "no");
    if (rc == 0) umount2(OUTSIDE, MNT_DETACH);

    /* 12: resolving inside the root, then mounting through the descriptor. */
    int root = open(ROOT, O_PATH | O_DIRECTORY | O_CLOEXEC);
    struct open_how how = { .flags = O_PATH | O_CLOEXEC,
                            .resolve = RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS };
    int planted = syscall(SYS_openat2, root, "evil", &how, sizeof how);
    int have_openat2 = !(planted < 0 && errno == ENOSYS);
    if (!have_openat2) {
        printf("12 openat2: not available here (ENOSYS): resolution needs a fallback\n");
    } else {
        /* ENOENT: the symlink's /tmp/... was looked up under the root, where
         * it doesn't exist, instead of on the host. */
        printf("12 openat2(RESOLVE_IN_ROOT) of the planted symlink: %s\n",
               planted >= 0 ? "opened (inside the root)" :
               errno == ENOENT ? "ENOENT: resolved inside the root, not on the host" : strerror(errno));
    }
    if (have_openat2) {
        struct stat inside, etc;
        int up = syscall(SYS_openat2, root, "../../../etc", &how, sizeof how);
        printf("12 openat2(RESOLVE_IN_ROOT) of ../../../etc: %s\n",
               up >= 0 && fstat(up, &inside) == 0 && stat(ROOT "/etc", &etc) == 0 &&
               inside.st_ino == etc.st_ino ? "the root's /etc (.. stops at the root)" :
               up < 0 ? strerror(errno) : "SOMETHING ELSE (escaped?)");
    }
    /* A real target: through openat2 if there is one, otherwise a plain
     * O_NOFOLLOW open of a directory known to be one. */
    int target_fd = have_openat2 ? syscall(SYS_openat2, root, "proc", &how, sizeof how)
                                 : openat(root, "proc", O_PATH | O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC);
    if (target_fd < 0) { printf("12 opening the real target: %s\n", strerror(errno)); return; }
    char target[64]; snprintf(target, sizeof target, "/proc/self/fd/%d", target_fd);
    rc = mount("tmpfs", target, "tmpfs", 0, NULL);
    printf("12 mounting through %s (opened with %s): %s; it is " ROOT "/proc: %s\n", target,
           have_openat2 ? "openat2" : "O_NOFOLLOW", err(rc),
           is_mount_point(ROOT "/proc") == 1 ? "yes" : "NO");

    /* 13: a target that doesn't exist. */
    rc = mount("tmpfs", ROOT "/missing", "tmpfs", 0, NULL);
    printf("13 mounting at a missing directory: %s\n", err(rc));
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int before = mounts();
    in_namespace(0, MS_SLAVE, check_needs_mount_point);
    in_namespace(0, MS_SHARED, check_shared);
    in_namespace(0, MS_SLAVE, check_slave);
    in_namespace(CLONE_NEWPID, MS_SLAVE, check_detach_and_proc);
    in_namespace(0, MS_SLAVE, check_devpts);
    struct mq_attr attr = { .mq_maxmsg = 1, .mq_msgsize = 8 };
    mqd_t q = mq_open("/shipyard-probe", O_CREAT | O_RDWR, 0600, &attr);
    if (q == (mqd_t)-1) printf("   (mq_open: %s)\n", strerror(errno));
    in_namespace(0, MS_SLAVE, check_mqueue_same_ipc);
    in_namespace(CLONE_NEWIPC, MS_SLAVE, check_mqueue_new_ipc);
    mq_unlink("/shipyard-probe");
    in_namespace(0, MS_SLAVE, check_sysfs_host_net);
    in_namespace(CLONE_NEWNET, MS_SLAVE, check_sysfs_new_net);
    in_namespace(0, MS_SLAVE, check_readonly);
    in_namespace(0, MS_SLAVE, check_remount_flags);
    in_namespace(0, MS_SLAVE, check_masking);
    in_namespace(0, MS_SLAVE, check_devices);
    in_namespace(0, MS_SLAVE, check_targets);
    in_namespace(0, MS_SLAVE, check_noexec_root);
    run("rm -rf " ROOT " " OUTSIDE);
    printf("mount table unchanged: %s (%d mounts before, %d after); scratch files removed: %s\n",
           before == mounts() ? "yes" : "NO", before, mounts(),
           access(ROOT, F_OK) && access(OUTSIDE, F_OK) ? "yes" : "NO");
    return 0;
}
