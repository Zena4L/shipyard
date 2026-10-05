/*
 * Kernel checks behind joining a running container (stage 2, step 4): what
 * `setns` does with a PID namespace, threads, mount namespaces and pidfds.
 * Each line of output answers one question; docs/stages/02-namespaces.md
 * records the results.
 *
 * Needs CAP_SYS_ADMIN (namespaces), so run it in a privileged container:
 *
 *   docker run --rm --privileged -v "$PWD/scripts/probes":/p rust:1-bookworm \
 *       sh -c 'gcc -w -pthread -o /tmp/join /p/join.c && /tmp/join'
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/sched.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char *what) { printf("probe setup failed: %s: %s\n", what, strerror(errno)); exit(2); }

static pid_t raw_clone3(uint64_t flags) {
    struct clone_args a; memset(&a, 0, sizeof a);
    a.flags = flags; a.exit_signal = SIGCHLD;
    return syscall(SYS_clone3, &a, sizeof a);
}

static int pidfd_open(pid_t pid) { return syscall(SYS_pidfd_open, pid, 0); }

/* The namespace a /proc link names, e.g. "pid:[4026532603]". */
static void ns_of(const char *link, char *out, size_t len) {
    ssize_t n = readlink(link, out, len - 1);
    out[n < 0 ? 0 : n] = 0;
}

/* The target: a small container (new PID, mount, UTS, IPC namespaces), whose
 * init reaps everything and says so. */
static pid_t start_container(void) {
    int ready[2]; if (pipe(ready)) die("pipe");
    pid_t c = raw_clone3(CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC);
    if (c < 0) die("clone3 (the container)");
    if (c == 0) {
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) _exit(1);
        if (mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) _exit(1);
        if (sethostname("box", 3)) _exit(1);
        /* Blocked and waited for: pause() wouldn't wake for SIGCHLD,
         * whose default action is to ignore it. */
        sigset_t chld; sigemptyset(&chld); sigaddset(&chld, SIGCHLD);
        sigprocmask(SIG_BLOCK, &chld, NULL);
        if (write(ready[1], "r", 1) != 1) _exit(1);
        for (;;) {
            sigwaitinfo(&chld, NULL);
            int st; pid_t p;
            while ((p = waitpid(-1, &st, WNOHANG)) > 0)
                printf("  (container init reaped its pid %d)\n", p);
        }
    }
    char b; if (read(ready[0], &b, 1) != 1) die("container start");
    close(ready[0]); close(ready[1]);
    return c;
}

/* 1: another thread forks while this one has switched pid_for_children.
 * Started before the switch: a thread whose pid_for_children differs from
 * its own namespace can't create threads (clone(CLONE_THREAD): EINVAL). */
static int pidfd_g;
static pthread_barrier_t switched;
static void *fork_from_other_thread(void *arg) {
    char own[64], child[64];
    pthread_barrier_wait(&switched);
    ns_of("/proc/thread-self/ns/pid", own, sizeof own);
    int p[2]; if (pipe(p)) die("pipe");
    pid_t c = fork();
    if (c == 0) {
        ns_of("/proc/self/ns/pid", child, sizeof child);
        if (write(p[1], child, strlen(child) + 1) < 0) _exit(1);
        _exit(0);
    }
    waitpid(c, NULL, 0);
    if (read(p[0], child, sizeof child) <= 0) child[0] = 0;
    printf("1 other thread's child: %s (that thread's own: %s) -> %s\n", child, own,
           strcmp(child, own) == 0 ? "unaffected" : "AFFECTED");
    return NULL;
}

/* A process's state letter from /proc/PID/stat (R, S, D, Z...). */
static char state_of(pid_t pid) {
    char path[64], buf[512]; snprintf(path, sizeof path, "/proc/%d/stat", pid);
    FILE *f = fopen(path, "r"); if (!f) return '-';
    size_t n = fread(buf, 1, sizeof buf - 1, f); fclose(f); buf[n] = 0;
    char *close = strrchr(buf, ')');
    return close && close[1] && close[2] ? close[2] : '?';
}

/* 8: does a container's init finish exiting while a process we started in
 * its PID namespace is still our unreaped zombie? 9: a zombie init's ns files. */
static void exit_waits_for_outside_zombies(void) {
    pid_t target = start_container();
    char path[64]; snprintf(path, sizeof path, "/proc/%d", target);
    int dir = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    int pid_ns = openat(dir, "ns/pid", O_RDONLY | O_CLOEXEC);
    int own = open("/proc/self/ns/pid", O_RDONLY | O_CLOEXEC);
    if (setns(pid_ns, CLONE_NEWPID)) die("setns");
    pid_t joined = fork();
    if (joined == 0) { for (;;) pause(); }
    setns(own, CLONE_NEWPID);
    kill(target, SIGKILL);
    usleep(500000);
    printf("8 init killed, joined process unreaped: init state %c, joined state %c\n",
           state_of(target), state_of(joined));
    errno = 0;
    int mnt = openat(dir, "ns/mnt", O_RDONLY | O_CLOEXEC);
    printf("9 openat ns/mnt of that init: %s\n", mnt >= 0 ? "opened" : strerror(errno));
    waitpid(joined, NULL, 0);
    usleep(200000);
    printf("8 after we reaped it: init state %c\n", state_of(target));
    /* 10: fork into the namespace of a zombie (exited, unreaped) init. */
    if (setns(pid_ns, CLONE_NEWPID)) printf("10 setns(ns/pid of the zombie init): %s\n", strerror(errno));
    errno = 0;
    pid_t late = fork();
    if (late == 0) _exit(0);
    printf("10 fork into a zombie init's namespace: %s\n", late < 0 ? strerror(errno) : "SUCCEEDED");
    if (late > 0) waitpid(late, NULL, 0);
    /* 11: a switched thread creating a PID namespace. */
    errno = 0;
    pid_t nested = raw_clone3(CLONE_NEWPID);
    if (nested == 0) _exit(0);
    printf("11 clone3(NEWPID) from a switched thread: %s\n", nested < 0 ? strerror(errno) : "SUCCEEDED");
    if (nested > 0) waitpid(nested, NULL, 0);
    setns(own, CLONE_NEWPID);
    errno = 0;
    mnt = openat(dir, "ns/mnt", O_RDONLY | O_CLOEXEC);
    printf("9 openat ns/mnt of the zombie init: %s\n", mnt >= 0 ? "opened" : strerror(errno));
    int pidns = openat(dir, "ns/pid", O_RDONLY | O_CLOEXEC);
    printf("9 openat ns/pid of the zombie init: %s\n", pidns >= 0 ? "opened" : strerror(errno));
    waitpid(target, NULL, 0);
    /* 12: the /proc/PID directory descriptor after the PID is reused. */
    FILE *last = fopen("/proc/sys/kernel/ns_last_pid", "w");
    if (last) { fprintf(last, "%d", target - 1); fclose(last); }
    pid_t again = fork();
    if (again == 0) { pause(); _exit(0); }
    errno = 0;
    int reused = openat(dir, "ns/pid", O_RDONLY | O_CLOEXEC);
    printf("12 PID %d %s; openat through the old descriptor: %s\n", target,
           again == target ? "reused" : "not reused (check skipped)",
           reused >= 0 ? "OPENED (reached the new process)" : strerror(errno));
    kill(again, SIGKILL); waitpid(again, NULL, 0);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (prctl(PR_SET_CHILD_SUBREAPER, 1)) die("subreaper");
    pid_t target = start_container();
    int pidfd = pidfd_open(target);
    if (pidfd < 0) die("pidfd_open");
    pidfd_g = pidfd;
    char own_pid_ns[64], target_pid_ns[64], path[64];
    ns_of("/proc/self/ns/pid", own_pid_ns, sizeof own_pid_ns);
    snprintf(path, sizeof path, "/proc/%d/ns/pid", target);
    ns_of(path, target_pid_ns, sizeof target_pid_ns);
    int own_pid_fd = open("/proc/thread-self/ns/pid", O_RDONLY | O_CLOEXEC);
    int target_pid_fd = open(path, O_RDONLY | O_CLOEXEC);
    printf("own %s, container %s\n", own_pid_ns, target_pid_ns);

    pthread_barrier_init(&switched, NULL, 2);
    pthread_t t; pthread_create(&t, NULL, fork_from_other_thread, NULL);
    /* Switch this thread's pid_for_children. */
    if (setns(pidfd, CLONE_NEWPID)) printf("setns(pidfd, NEWPID): %s\n", strerror(errno));
    pthread_t t2;
    errno = pthread_create(&t2, NULL, (void *(*)(void *))pause, NULL);
    printf("1b creating a thread while switched: %s\n", errno ? strerror(errno) : "allowed");
    pthread_barrier_wait(&switched);
    pthread_join(t, NULL);

    /* 2: our own child lands inside, is ours, and we get its status. */
    pid_t c = fork();
    if (c == 0) {
        char ns[64]; ns_of("/proc/self/ns/pid", ns, sizeof ns);
        /* getpid() is the PID inside; the parent is outside, so getppid() is 0. */
        printf("2 child: ns %s, getpid %ld, getppid %ld\n", ns,
               (long)syscall(SYS_getpid), (long)getppid());
        _exit(7);
    }
    int st; pid_t r = waitpid(c, &st, 0);
    printf("2 parent reaped %d (host pid %d): exit %d\n", r, c, WIFEXITED(st) ? WEXITSTATUS(st) : -1);

    /* 4: the child's orphan goes to the container's init, not to our subreaper. */
    c = fork();
    if (c == 0) {
        if (fork() == 0) { usleep(300000); _exit(0); }
        _exit(0);
    }
    waitpid(c, NULL, 0);
    usleep(600000);
    r = waitpid(-1, &st, WNOHANG);
    printf("4 after the orphan exited, our waitpid(-1): %d (%s) -> %s\n", r,
           r < 0 ? strerror(errno) : "a child",
           r < 0 || r == 0 ? "the container's init got it" : "WE got it");

    /* 3: back to our own namespace. */
    if (setns(own_pid_fd, CLONE_NEWPID)) printf("3 restore: %s\n", strerror(errno));
    char now[64]; ns_of("/proc/thread-self/ns/pid_for_children", now, sizeof now);
    printf("3 restored pid_for_children: %s -> %s\n", now,
           strcmp(now, own_pid_ns) == 0 ? "ours again" : "STILL SWITCHED");

    /* 7: a mount-namespace join from a process with another thread running. */
    pthread_t idle; pthread_create(&idle, NULL, (void *(*)(void *))pause, NULL);
    usleep(50000);
    errno = 0;
    int rc = setns(pidfd, CLONE_NEWNS);
    printf("7 setns(NEWNS) with two threads: %s\n", rc == 0 ? "allowed" : strerror(errno));

    /* 5: in a (single-threaded) child: join the rest at once; cwd and root. */
    c = fork();
    if (c == 0) {
        if (chdir("/tmp")) _exit(1);
        if (setns(pidfd, CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC)) {
            printf("5 setns(pidfd, NS|UTS|IPC): %s\n", strerror(errno)); _exit(1);
        }
        char host[64] = "", cwd[256] = "";
        gethostname(host, sizeof host);
        if (!getcwd(cwd, sizeof cwd)) snprintf(cwd, sizeof cwd, "(getcwd: %s)", strerror(errno));
        printf("5 joined: hostname %s, cwd %s\n", host, cwd);
        _exit(0);
    }
    waitpid(c, NULL, 0);

    /* 6: a PID namespace whose init is dead. */
    kill(target, SIGKILL);
    usleep(100000);
    errno = 0;
    rc = setns(pidfd, CLONE_NEWPID);
    printf("6a setns(pidfd) to a dead, unreaped init: %s\n", rc == 0 ? "allowed" : strerror(errno));
    if (rc == 0) setns(own_pid_fd, CLONE_NEWPID);
    rc = syscall(SYS_pidfd_send_signal, pidfd, 0, NULL, 0);
    printf("6b pidfd_send_signal(0) to it: %s\n", rc == 0 ? "ok (still a zombie)" : strerror(errno));
    waitpid(target, NULL, 0);
    rc = syscall(SYS_pidfd_send_signal, pidfd, 0, NULL, 0);
    printf("6c after reaping, pidfd_send_signal(0): %s\n", rc == 0 ? "ok" : strerror(errno));
    if (setns(target_pid_fd, CLONE_NEWPID)) printf("6d setns(ns file): %s\n", strerror(errno));
    errno = 0;
    c = fork();
    if (c == 0) _exit(0);
    printf("6d fork into the dead namespace: %s\n", c < 0 ? strerror(errno) : "SUCCEEDED");
    if (c > 0) waitpid(c, NULL, 0);
    /* Back first: a switched thread can't create a PID namespace either. */
    setns(own_pid_fd, CLONE_NEWPID);
    exit_waits_for_outside_zombies();
    return 0;
}
