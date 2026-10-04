/*

 * Run it in a privileged Linux container (needs CAP_SYS_ADMIN for the
 * namespace checks), and once unprivileged (to see seccomp's effect):
 *
 *   docker run --rm --privileged -v "$PWD/scripts/probes":/p rust:1-bookworm \
 *       sh -c 'gcc -w -o /tmp/probe /p/namespaces.c && /tmp/probe full'
 *   docker run --rm -v "$PWD/scripts/probes":/p rust:1-bookworm \
 *       sh -c 'gcc -w -o /tmp/probe /p/namespaces.c && /tmp/probe'
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t raw_clone3(uint64_t flags) {
    struct clone_args a; memset(&a, 0, sizeof a);
    a.flags = flags; a.exit_signal = SIGCHLD;
    return syscall(SYS_clone3, &a, sizeof a);
}

/* 1: does a namespace init get signals it blocks (no handler)? and unblocked ones? */
static void die(const char *what) { printf("probe setup failed: %s: %s\n", what, strerror(errno)); exit(2); }

static void spike_signals(void) {
    int p[2]; if (pipe(p)) die("pipe");
    pid_t c = raw_clone3(CLONE_NEWPID);
    if (c < 0) { printf("signals: clone3 failed: %s\n", strerror(errno)); return; }
    if (c == 0) {
        sigset_t all; sigfillset(&all);
        sigdelset(&all, SIGUSR2);              /* USR2 left unblocked, default action */
        sigprocmask(SIG_SETMASK, &all, NULL);
        if (write(p[1], "r", 1) != 1) _exit(3);
        siginfo_t si;
        /* A timeout, not alarm(): PID 1 would drop an unblocked SIGALRM too. */
        struct timespec limit = { .tv_sec = 5 };
        for (int i = 0; i < 3; i++) {
            if (sigtimedwait(&all, &si, &limit) < 0) { printf("  init: no signal within 5 s\n"); _exit(4); }
            printf("  init(pid %d) got signal %d from pid %d\n", getpid(), si.si_signo, si.si_pid);
        }
        _exit(0);
    }
    char b; if (read(p[0], &b, 1) != 1) die("waiting for the init");
    kill(c, SIGUSR2);                  /* expect: dropped (pid 1, no handler, unblocked) */
    usleep(100000);
    kill(c, SIGTERM); kill(c, SIGINT); kill(c, SIGUSR1);  /* expect: received (blocked) */
    int st; waitpid(c, &st, 0);
    if (WIFSIGNALED(st)) printf("signals: init was killed by signal %d (USR2 was NOT dropped!)\n", WTERMSIG(st));
    else if (WEXITSTATUS(st) == 0) printf("signals: init got all three blocked signals; USR2 was dropped\n");
    else printf("signals: init failed (exit %d): a blocked signal didn't arrive\n", WEXITSTATUS(st));
}

/* 2: private propagation + fresh /proc in a new mount+pid namespace */
static void spike_proc(void) {
    pid_t c = raw_clone3(CLONE_NEWPID | CLONE_NEWNS);
    if (c < 0) { printf("proc: clone3 failed: %s\n", strerror(errno)); return; }
    if (c == 0) {
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) { printf("proc: rprivate: %s\n", strerror(errno)); _exit(1); }
        if (mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) { printf("proc: mount: %s\n", strerror(errno)); _exit(1); }
        system("echo '  inside: pids in /proc:' $(ls /proc | grep -E '^[0-9]+$' | tr '\\n' ' ')");
        _exit(0);
    }
    int st; waitpid(c, &st, 0);
    system("echo '  host after: proc mounts:' $(grep -c ' /proc ' /proc/self/mountinfo)");
}

int main(int argc, char **argv) {
    /* Unbuffered: a buffer copied by fork would print its lines twice. */
    setvbuf(stdout, NULL, _IONBF, 0);
    system("echo 'propagation of /:' $(awk '$5==\"/\"{print $7}' /proc/self/mountinfo)");
    pid_t c = raw_clone3(0);
    if (c == 0) _exit(0);
    printf("clone3 plain: %s\n", c < 0 ? strerror(errno) : "ok");
    if (c > 0) waitpid(c, NULL, 0);
    c = raw_clone3(CLONE_NEWPID);
    if (c == 0) _exit(0);
    printf("clone3 NEWPID: %s\n", c < 0 ? strerror(errno) : "ok");
    if (c > 0) waitpid(c, NULL, 0);
    c = syscall(SYS_clone, CLONE_NEWPID | SIGCHLD, 0, 0, 0, 0);
    if (c == 0) _exit(0);
    printf("clone NEWPID: %s\n", c < 0 ? strerror(errno) : "ok");
    if (c > 0) waitpid(c, NULL, 0);
    if (argc > 1) { spike_signals(); spike_proc(); }
    return 0;
}
