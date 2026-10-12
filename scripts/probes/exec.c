/*
 * Kernel checks behind `exec` (stage 8, step 8.4a): joining a container that
 * has a user namespace, from root and from an unprivileged user, the way
 * `shipyard-runtime exec` does it. Each line of output answers one question;
 * the stage plan records the results.
 *
 *   E1  After setns(user) and setns(pid), does clone3(CLONE_PARENT |
 *       CLONE_INTO_CGROUP) give a child that is the *caller's parent's*
 *       child, inside the PID namespace, in the cgroup?
 *   E2  NS_GET_USERNS: which of the container's namespaces does its user
 *       namespace own (a namespace it shares with the host is owned by the
 *       host's)?
 *   E3  Dumpability across setns(user): set to 0 before, what is it after?
 *   E4  Can root inside the container (all capabilities in its own user
 *       namespace, CAP_SYS_PTRACE included) reach the exec'd-to-be process
 *       before its execve: readlink /proc/PID/exe, open /proc/PID/fd,
 *       read /proc/PID/environ? Non-dumpable versus a dumpable control.
 *
 * Usage: exec CGROUP_DIR   (a cgroup v2 directory this user may move
 * processes into; run as root, then as a user with a delegated subtree).
 *
 *   gcc -Wall -Wextra -o /tmp/exec scripts/probes/exec.c
 *   /tmp/exec /sys/fs/cgroup/probe-exec
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/nsfs.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char *what) {
    printf("probe setup failed: %s: %s\n", what, strerror(errno));
    exit(2);
}

static pid_t raw_clone3(uint64_t flags, int cgroup) {
    struct clone_args a;
    memset(&a, 0, sizeof a);
    a.flags = flags;
    /* With CLONE_PARENT the child takes its parent's exit signal: clone3
     * refuses one of its own (EINVAL). */
    a.exit_signal = (flags & CLONE_PARENT) ? 0 : SIGCHLD;
    if (cgroup >= 0) {
        a.flags |= CLONE_INTO_CGROUP;
        a.cgroup = (uint64_t)cgroup;
    }
    return syscall(SYS_clone3, &a, sizeof a);
}

static void write_file(const char *path, const char *text) {
    int fd = open(path, O_WRONLY);
    if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text)) die(path);
    close(fd);
}

/* The container: user, PID and mount namespaces, its own /proc. Its init
 * waits for a PID (as it sees it) on `probe`, tries to reach that process,
 * reports on `answer`, and reaps whatever comes to it. */
static pid_t start_container(int probe, int answer) {
    int go[2];
    if (pipe(go)) die("pipe");
    pid_t c = raw_clone3(CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNS, -1);
    if (c < 0) die("clone3 (the container)");
    if (c == 0) {
        char b;
        close(go[1]);
        if (read(go[0], &b, 1) != 1) _exit(1);
        if (setresgid(0, 0, 0) || setresuid(0, 0, 0)) _exit(1);
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) _exit(1);
        if (mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) _exit(1);
        signal(SIGCHLD, SIG_DFL);
        for (;;) {
            int pid;
            ssize_t n = read(probe, &pid, sizeof pid);
            if (n == 0) break;
            if (n != sizeof pid) _exit(1);
            char path[64], out[256], link[256];
            int len = 0;
            snprintf(path, sizeof path, "/proc/%d/exe", pid);
            ssize_t r = readlink(path, link, sizeof link - 1);
            len += snprintf(out + len, sizeof out - len, "exe:%s ",
                            r < 0 ? strerror(errno) : "readable");
            snprintf(path, sizeof path, "/proc/%d/fd", pid);
            int fd = open(path, O_RDONLY | O_DIRECTORY);
            len += snprintf(out + len, sizeof out - len, "fd:%s ",
                            fd < 0 ? strerror(errno) : "listable");
            if (fd >= 0) close(fd);
            snprintf(path, sizeof path, "/proc/%d/environ", pid);
            fd = open(path, O_RDONLY);
            snprintf(out + len, sizeof out - len, "environ:%s",
                     fd < 0 ? strerror(errno) : "readable");
            if (fd >= 0) close(fd);
            if (write(answer, out, strlen(out) + 1) < 0) _exit(1);
        }
        while (waitpid(-1, NULL, 0) > 0) {}
        _exit(0);
    }
    close(go[0]);
    /* Map container root to whoever we are (root: also root; a user: the
     * single mapping an unprivileged process may write). */
    char path[64], map[64];
    snprintf(path, sizeof path, "/proc/%d/setgroups", c);
    write_file(path, "deny");
    snprintf(map, sizeof map, "0 %d 1", getuid());
    snprintf(path, sizeof path, "/proc/%d/uid_map", c);
    write_file(path, map);
    snprintf(map, sizeof map, "0 %d 1", getgid());
    snprintf(path, sizeof path, "/proc/%d/gid_map", c);
    write_file(path, map);
    if (write(go[1], "g", 1) != 1) die("go");
    close(go[1]);
    return c;
}

static int ns_open(pid_t pid, const char *kind) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/ns/%s", pid, kind);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) die(path);
    return fd;
}

static ino_t inode_of(int fd) {
    struct stat st;
    if (fstat(fd, &st)) die("fstat");
    return st.st_ino;
}

/* E2: for each kind, whether the container's user namespace owns it. */
static void owners(pid_t c) {
    int user = ns_open(c, "user");
    ino_t own = inode_of(user);
    const char *kinds[] = {"pid", "mnt", "uts", "ipc", "net", "cgroup"};
    printf("E2 owned by the container's user namespace:");
    for (size_t i = 0; i < sizeof kinds / sizeof kinds[0]; i++) {
        int ns = ns_open(c, kinds[i]);
        int owner = ioctl(ns, NS_GET_USERNS);
        if (owner < 0) {
            printf(" %s:%s", kinds[i], strerror(errno));
        } else {
            printf(" %s:%s", kinds[i], inode_of(owner) == own ? "yes" : "no(host's)");
            close(owner);
        }
        close(ns);
    }
    printf("\n");
    close(user);
}

/* E1, E3, E4: the joiner. `dumpable` 0 for the real thing, 1 for the
 * control. The exec'd-to-be process sends its PID (as the container sees
 * it) on `probe`, waits for the container's answer, and exits 7. */
static void join(pid_t c, int cgroup, int dumpable, int probe, int answer) {
    int user = ns_open(c, "user"), pidns = ns_open(c, "pid");
    int cloned[2];
    if (pipe(cloned)) die("pipe");
    pid_t joiner = fork();
    if (joiner < 0) die("fork");
    if (joiner == 0) {
        if (!dumpable && prctl(PR_SET_DUMPABLE, 0)) _exit(10);
        int before = prctl(PR_GET_DUMPABLE);
        if (setns(user, CLONE_NEWUSER)) { printf("E1 setns(user): %s\n", strerror(errno)); _exit(11); }
        int after = prctl(PR_GET_DUMPABLE);
        if (!dumpable) prctl(PR_SET_DUMPABLE, 0);
        if (setns(pidns, CLONE_NEWPID)) { printf("E1 setns(pid): %s\n", strerror(errno)); _exit(12); }
        printf("E3 dumpable %d before setns(user), %d after (fs.suid_dumpable applies on a credential change)\n",
               before, after);
        fflush(stdout);
        pid_t child = raw_clone3(CLONE_PARENT, cgroup);
        if (child < 0) { printf("E1 clone3(CLONE_PARENT|CLONE_INTO_CGROUP): %s\n", strerror(errno)); _exit(13); }
        if (child == 0) {
            int me = getpid();
            char cg[256] = {0};
            int fd = open("/proc/self/cgroup", O_RDONLY);
            ssize_t n = fd < 0 ? -1 : read(fd, cg, sizeof cg - 1);
            if (n > 0 && cg[n - 1] == '\n') cg[n - 1] = 0;
            printf("E1 the child: pid %d inside, parent %d (0: outside the namespace), cgroup %s\n",
                   me, getppid(), cg);
            fflush(stdout);
            if (write(probe, &me, sizeof me) != sizeof me) _exit(1);
            char out[256];
            if (read(answer, out, sizeof out) <= 0) _exit(1);
            printf("E4 %s, from the container's root: %s\n",
                   dumpable ? "dumpable (the control)" : "non-dumpable", out);
            fflush(stdout);
            _exit(7);
        }
        printf("E1 the joiner (pid %d) cloned %d, and exits\n", getpid(), child);
        fflush(stdout);
        if (write(cloned[1], &child, sizeof child) != sizeof child) _exit(14);
        _exit(0);
    }
    close(user);
    close(pidns);
    close(cloned[1]);
    int st;
    if (waitpid(joiner, &st, 0) != joiner) die("waitpid (joiner)");
    printf("E1 joiner ended: %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -WTERMSIG(st));
    pid_t child;
    if (read(cloned[0], &child, sizeof child) != sizeof child) {
        printf("E1 no clone\n");
        close(cloned[0]);
        return;
    }
    close(cloned[0]);
    /* CLONE_PARENT: the clone is ours. */
    pid_t got = waitpid(child, &st, 0);
    if (got < 0) {
        printf("E1 waitpid(-1): %s (the clone isn't our child)\n", strerror(errno));
    } else {
        printf("E1 reaped our child %d: exit %d\n", got,
               WIFEXITED(st) ? WEXITSTATUS(st) : -WTERMSIG(st));
    }
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc != 2) {
        fprintf(stderr, "usage: %s CGROUP_DIR\n", argv[0]);
        return 2;
    }
    printf("== as uid %d\n", getuid());
    int cgroup = open(argv[1], O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (cgroup < 0) die(argv[1]);
    int probe[2], answer[2];
    if (pipe(probe) || pipe(answer)) die("pipe");
    pid_t c = start_container(probe[0], answer[1]);
    owners(c);
    join(c, cgroup, 0, probe[1], answer[0]);
    join(c, cgroup, 1, probe[1], answer[0]);
    close(probe[1]);
    kill(c, SIGKILL);
    waitpid(c, NULL, 0);
    return 0;
}
