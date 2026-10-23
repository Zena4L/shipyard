/*
 * Kernel checks behind `--ipc container:NAME` sharing `/dev/shm` (stage 11,
 * step 11.4b2b2): can a process in one mount namespace bind a tmpfs that's
 * mounted only in another (container A's `/dev/shm`), reached through
 * `/proc/PID/root`? Each line of output answers one question.
 *
 * A is a child in a mount namespace of its own, with a tmpfs on a directory
 * holding a file. B is another child, in a mount namespace of its own too,
 * that tries: a plain bind (`mount(MS_BIND)`) of A's directory, then
 * `open_tree(OPEN_TREE_CLONE)` + `move_mount`. Root only (mount namespaces).
 *
 *   timeout 300 docker run --rm --privileged -v "$PWD/scripts/probes":/p:ro rust:1-bookworm \
 *       sh -c 'gcc -Wall -Wextra -o /tmp/foreign-shm /p/foreign-shm.c && /tmp/foreign-shm'
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef OPEN_TREE_CLONE
#define OPEN_TREE_CLONE 1
#endif
#ifndef MOVE_MOUNT_F_EMPTY_PATH
#define MOVE_MOUNT_F_EMPTY_PATH 0x00000004
#endif

static void die(const char *what) { printf("probe setup failed: %s: %s\n", what, strerror(errno)); exit(2); }

static const char *SHM = "/tmp/foreign-shm-a";

/* Without root, A has a user namespace of its own (its root is this user),
 * and B and the helper join it first, as a rootless container's do. */
static int rootless;

/* Writes `text` to /proc/self/`name`. */
static void write_self(const char *name, const char *text) {
    char path[64];
    snprintf(path, sizeof path, "/proc/self/%s", name);
    int fd = open(path, O_WRONLY);
    if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text)) die(name);
    close(fd);
}

/* Joins A's user namespace (rootless only). */
static void join_user(pid_t a) {
    if (!rootless) return;
    char ns[64];
    snprintf(ns, sizeof ns, "/proc/%d/ns/user", a);
    int fd = open(ns, O_RDONLY);
    if (fd < 0 || setns(fd, CLONE_NEWUSER)) die("joining A's user namespace");
    close(fd);
}

/* Whether `dir` holds A's file. */
static int sees_the_file(const char *dir) {
    char path[256], text[16] = {0};
    snprintf(path, sizeof path, "%s/marker", dir);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, text, sizeof text - 1);
    close(fd);
    return n > 0 && strncmp(text, "from-a", 6) == 0;
}

int main(void) {
    /* A child's lines must reach the output before its `_exit`. */
    setvbuf(stdout, NULL, _IONBF, 0);
    rootless = geteuid() != 0;
    printf("as: %s\n", rootless ? "a normal user (A has a user namespace)" : "root");
    int ready[2], done[2];
    if (pipe(ready) || pipe(done)) die("pipe");
    mkdir(SHM, 0755);
    pid_t a = fork();
    if (a < 0) die("fork");
    if (a == 0) {
        /* A: its own mount namespace, a tmpfs there only, a file in it. */
        if (rootless) {
            uid_t uid = geteuid();
            gid_t gid = getegid();
            if (unshare(CLONE_NEWUSER | CLONE_NEWNS)) die("A: unshare");
            char map[64];
            write_self("setgroups", "deny");
            snprintf(map, sizeof map, "0 %d 1", uid);
            write_self("uid_map", map);
            snprintf(map, sizeof map, "0 %d 1", gid);
            write_self("gid_map", map);
        } else if (unshare(CLONE_NEWNS)) die("A: unshare");
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("A: private");
        if (mount("shm", SHM, "tmpfs", 0, "size=1m")) die("A: tmpfs");
        char path[256];
        snprintf(path, sizeof path, "%s/marker", SHM);
        int fd = open(path, O_WRONLY | O_CREAT, 0644);
        if (fd < 0 || write(fd, "from-a", 6) != 6) die("A: marker");
        close(fd);
        char c = 1;
        if (write(ready[1], &c, 1) != 1) die("A: ready");
        if (read(done[0], &c, 1) < 0) die("A: done");
        _exit(0);
    }
    /* Only A writes `ready`: if it fails, the read ends (no hang). */
    close(ready[1]);
    char c;
    if (read(ready[0], &c, 1) != 1) die("A didn't get ready");
    printf("the host's directory doesn't show A's tmpfs (the control): %s\n",
           sees_the_file(SHM) ? "NO (it does)" : "yes");
    pid_t b = fork();
    if (b < 0) die("fork");
    if (b == 0) {
        /* B: a mount namespace of its own, as a container's setup is. */
        join_user(a);
        if (unshare(CLONE_NEWNS)) die("B: unshare");
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("B: private");
        char source[256];
        snprintf(source, sizeof source, "/proc/%d/root%s", a, SHM);
        printf("B reads A's file through /proc/PID/root: %s\n", sees_the_file(source) ? "yes" : "no");
        const char *target1 = "/tmp/foreign-shm-b1", *target2 = "/tmp/foreign-shm-b2";
        mkdir(target1, 0755);
        mkdir(target2, 0755);
        int fd = open(source, O_PATH | O_DIRECTORY);
        if (fd < 0) die("B: open A's directory");
        char by_fd[64];
        snprintf(by_fd, sizeof by_fd, "/proc/self/fd/%d", fd);
        if (mount(by_fd, target1, NULL, MS_BIND, NULL) == 0)
            printf("mount(MS_BIND) of A's tmpfs: works, B sees A's file: %s\n",
                   sees_the_file(target1) ? "yes" : "no");
        else
            printf("mount(MS_BIND) of A's tmpfs: refused (%s)\n", strerror(errno));
        int tree = (int)syscall(SYS_open_tree, fd, "", OPEN_TREE_CLONE | AT_EMPTY_PATH);
        if (tree < 0) {
            printf("open_tree(OPEN_TREE_CLONE) of A's tmpfs: refused (%s)\n", strerror(errno));
        } else if (syscall(SYS_move_mount, tree, "", AT_FDCWD, target2, MOVE_MOUNT_F_EMPTY_PATH)) {
            printf("open_tree: cloned, move_mount refused (%s)\n", strerror(errno));
        } else {
            printf("open_tree + move_mount of A's tmpfs: works, B sees A's file: %s\n",
                   sees_the_file(target2) ? "yes" : "no");
            /* Shared, not copied: a file B writes, A sees. */
            char path[256];
            snprintf(path, sizeof path, "%s/from-b", target2);
            int out = open(path, O_WRONLY | O_CREAT, 0644);
            if (out >= 0) close(out);
            char back[256];
            snprintf(back, sizeof back, "/proc/%d/root%s/from-b", a, SHM);
            printf("a file B makes there is A's too: %s\n", access(back, F_OK) == 0 ? "yes" : "no");
        }
        /* A helper in A's mount namespace clones it there (a detached
         * mount: no namespace's), and hands it to B over a socket. */
        int pair[2];
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair)) die("B: socketpair");
        pid_t h = fork();
        if (h < 0) die("B: fork");
        if (h == 0) {
            /* In A's user namespace already: B's child. */
            char ns[64];
            snprintf(ns, sizeof ns, "/proc/%d/ns/mnt", a);
            int mnt = open(ns, O_RDONLY);
            if (mnt < 0 || setns(mnt, CLONE_NEWNS)) {
                printf("the helper enters A's mount namespace: refused (%s)\n", strerror(errno));
                _exit(1);
            }
            int clone = (int)syscall(SYS_open_tree, AT_FDCWD, SHM, OPEN_TREE_CLONE);
            if (clone < 0) {
                printf("open_tree(OPEN_TREE_CLONE) in A's namespace: refused (%s)\n", strerror(errno));
                _exit(1);
            }
            char control[CMSG_SPACE(sizeof(int))] = {0};
            char byte = 0;
            struct iovec iov = {.iov_base = &byte, .iov_len = 1};
            struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control,
                                 .msg_controllen = sizeof control};
            struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
            cmsg->cmsg_level = SOL_SOCKET;
            cmsg->cmsg_type = SCM_RIGHTS;
            cmsg->cmsg_len = CMSG_LEN(sizeof(int));
            memcpy(CMSG_DATA(cmsg), &clone, sizeof(int));
            if (sendmsg(pair[1], &msg, 0) != 1) die("helper: sendmsg");
            _exit(0);
        }
        char control[CMSG_SPACE(sizeof(int))] = {0};
        char byte;
        struct iovec iov = {.iov_base = &byte, .iov_len = 1};
        struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control,
                             .msg_controllen = sizeof control};
        int got = -1;
        if (recvmsg(pair[0], &msg, 0) == 1 && CMSG_FIRSTHDR(&msg))
            memcpy(&got, CMSG_DATA(CMSG_FIRSTHDR(&msg)), sizeof(int));
        waitpid(h, NULL, 0);
        if (got >= 0) {
            const char *target3 = "/tmp/foreign-shm-b3";
            mkdir(target3, 0755);
            if (syscall(SYS_move_mount, got, "", AT_FDCWD, target3, MOVE_MOUNT_F_EMPTY_PATH))
                printf("a clone made in A's namespace, moved into B's: refused (%s)\n", strerror(errno));
            else {
                printf("a clone made in A's namespace, moved into B's: works, B sees A's file: %s\n",
                       sees_the_file(target3) ? "yes" : "no");
                char path[256];
                snprintf(path, sizeof path, "%s/from-b3", target3);
                int out = open(path, O_WRONLY | O_CREAT, 0644);
                if (out >= 0) close(out);
                char back[256];
                snprintf(back, sizeof back, "/proc/%d/root%s/from-b3", a, SHM);
                printf("a file B makes there is A's too: %s\n", access(back, F_OK) == 0 ? "yes" : "no");
            }
        }
        _exit(0);
    }
    int status;
    waitpid(b, &status, 0);
    if (write(done[1], &c, 1) != 1) die("done");
    waitpid(a, &status, 0);
    FILE *release = fopen("/proc/sys/kernel/osrelease", "r");
    char kernel[65] = "?";
    if (release && fgets(kernel, sizeof kernel, release)) kernel[strcspn(kernel, "\n")] = 0;
    printf("kernel: %s\n", kernel);
    return 0;
}
