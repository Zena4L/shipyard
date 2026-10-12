/*
 * Kernel checks behind terminals, I/O and joining (stage 8): devpts
 * instances in user namespaces, TIOCGPTPEER, controlling terminals in a PID
 * namespace, job control, SCM_RIGHTS across namespaces, window sizes, what
 * the master sees when the slave closes (and the reverse), TIOCSTI,
 * dumpability across setns, openat2 confined to a root, CLONE_INTO_CGROUP,
 * and pidfds in poll. Each line of output answers one question (T1…T14);
 * the stage plan's step 8.1a, then docs/stages/08-terminals.md, record the
 * results.
 *
 * Every check runs in a forked child, and every mount in a private mount
 * namespace, so nothing outlives the probe. It only ever uses terminals it
 * made itself: TIOCSTI is never tried on its own stdio or /dev/tty, and the
 * TIOCSTI checks run with stdin from /dev/null. It writes no sysctl. The
 * cgroup check works in a scratch cgroup it removes. Run it as root in a
 * privileged container; the rootless checks drop to nobody (65534)
 * themselves:
 *
 *   docker run --rm --privileged -v "$PWD/scripts/probes":/p:ro rust:1-bookworm \
 *       sh -c 'gcc -Wall -Wextra -Werror -o /tmp/terminal /p/terminal.c && /tmp/terminal'
 *
 * As the cgroup probe does, in that container only: a container's cgroup
 * namespace root is an ordinary cgroup, which can't hand controllers down
 * while it holds processes, so they're moved into a leaf (/init) first. A
 * host's real root cgroup is exempt, and there nothing is moved. Any
 * controller it enables at the root is disabled again at the end.
 *
 * Interrupted (Ctrl-C), it can leave /tmp/shipyard-probe-tty.* and, during
 * T13, a scratch cgroup behind: remove them by hand.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <linux/openat2.h>
#include <linux/sched.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#ifndef TIOCGPTPEER
#define TIOCGPTPEER _IO('T', 0x41)
#endif

#define NOBODY 65534
#define CGROOT "/sys/fs/cgroup"

/* The scratch directory every check mounts over (in its own mount
 * namespace), a directory in it T12's containers mount over, and a file
 * only the host's view has. */
static char base[64], inner[128], host_only[128];

static const char *say(int rc) { return rc == 0 ? "ok" : strerror(errno); }

static void die(const char *what) { printf("probe setup failed: %s: %s\n", what, strerror(errno)); exit(2); }

static void sleep_ms(long ms) {
    struct timespec t = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&t, NULL);
}

static long now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

/* "exited N", "killed by SIGNAME", "stopped by SIGNAME", "continued". */
static const char *how(int status) {
    static char text[64];
    if (WIFEXITED(status)) snprintf(text, sizeof text, "exited %d", WEXITSTATUS(status));
    else if (WIFSIGNALED(status)) snprintf(text, sizeof text, "killed by SIG%s", sigabbrev_np(WTERMSIG(status)));
    else if (WIFSTOPPED(status)) snprintf(text, sizeof text, "stopped by SIG%s", sigabbrev_np(WSTOPSIG(status)));
    else if (WIFCONTINUED(status)) snprintf(text, sizeof text, "continued");
    else snprintf(text, sizeof text, "status %#x", status);
    return text;
}

static pid_t spawn(void);

static void in_child(void (*check)(void)) {
    pid_t c = spawn();
    if (c == 0) { check(); _exit(0); }
    int status;
    waitpid(c, &status, 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) printf("   (that check's process %s)\n", how(status));
}

static int put(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    ssize_t n = write(fd, text, strlen(text));
    int saved = errno;
    close(fd);
    errno = saved;
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

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

/* Creates `path` holding `text`: 0, or -1 with errno. */
static int make_file(const char *path, const char *text) {
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0644);
    if (fd < 0) return -1;
    ssize_t n = write(fd, text, strlen(text));
    close(fd);
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

/* One byte, to wake whoever waits on the other end. */
static void poke(int fd) { (void)!write(fd, "x", 1); }

/* Waits up to `ms` for a byte: 1 if it came, 0 if not. */
static int await(int fd, int ms) {
    struct pollfd p = { .fd = fd, .events = POLLIN };
    if (poll(&p, 1, ms) != 1) return 0;
    char c;
    return read(fd, &c, 1) == 1;
}

static void to_nobody(void) {
    if (setgroups(0, NULL) || setresgid(NOBODY, NOBODY, NOBODY) || setresuid(NOBODY, NOBODY, NOBODY)) die("drop to nobody");
    /* changing credentials made it non-dumpable, which makes its /proc
     * files root's: it couldn't write its own maps */
    if (prctl(PR_SET_DUMPABLE, 1)) die("PR_SET_DUMPABLE");
}

static void private_mounts(void) {
    if (unshare(CLONE_NEWNS)) die("unshare(CLONE_NEWNS)");
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("make-rprivate");
}

/* The signals the checks rely on back to their defaults, none blocked: the
 * probe may have been started with some ignored (`&` from a script ignores
 * SIGINT and SIGQUIT; nohup, SIGHUP) or blocked. */
static void default_signals(void) {
    const int sigs[] = { SIGINT, SIGQUIT, SIGTSTP, SIGHUP, SIGTTIN, SIGTTOU, SIGWINCH, SIGALRM };
    for (size_t i = 0; i < sizeof sigs / sizeof sigs[0]; i++) signal(sigs[i], SIG_DFL);
    sigset_t none;
    sigemptyset(&none);
    sigprocmask(SIG_SETMASK, &none, NULL);
}

/* fork(), or the check ends: a failed fork must never leave a -1 that
 * reaches kill(), which signals every process on the machine. The child
 * starts with default signals. */
static pid_t spawn(void) {
    pid_t c = fork();
    if (c < 0) die("fork");
    if (c == 0) default_signals();
    return c;
}

/* Kills and reaps a child (a real PID only). */
static void end(pid_t pid) {
    if (pid <= 0) return;
    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
}

/* Reads exactly `len` bytes within `ms`: 0, or -1 (timed out, or the other
 * end failed). A namespace's PID 1 drops alarm()'s SIGALRM (SIG_DFL), so
 * its waits are bounded this way. */
static int read_within(int fd, void *buf, size_t len, int ms) {
    struct pollfd p = { .fd = fd, .events = POLLIN };
    if (poll(&p, 1, ms) != 1) { errno = ETIMEDOUT; return -1; }
    return read(fd, buf, len) == (ssize_t)len ? 0 : -1;
}

/* As rootless: nobody, root of a user namespace of its own (mapped to
 * nobody alone), with a private mount namespace and `extra` namespaces. */
static int rootless(int extra, const char *label) {
    to_nobody();
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS | extra)) {
        printf("%s user namespace as nobody: %s\n", label, strerror(errno));
        return -1;
    }
    char line[32];
    if (put("/proc/self/setgroups", "deny")) die("setgroups");
    snprintf(line, sizeof line, "0 %d 1", NOBODY);
    if (put("/proc/self/gid_map", line) || put("/proc/self/uid_map", line)) die("id maps");
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("make-rprivate");
    return 0;
}

/* A devpts instance of its own at base/pts, ptmx reached through a
 * base/ptmx -> pts/ptmx link (as a container's /dev/ptmx), and one pty from
 * it: the slave through TIOCGPTPEER. The mount namespace must be private. */
static int open_pty(int *master, int *slave) {
    char pts[128], ptmx[128];
    snprintf(pts, sizeof pts, "%s/pts", base);
    snprintf(ptmx, sizeof ptmx, "%s/ptmx", base);
    if (mount("tmpfs", base, "tmpfs", MS_NOSUID, "mode=0755")) return -1;
    if (mkdir(pts, 0755) || symlink("pts/ptmx", ptmx)) return -1;
    if (mount("devpts", pts, "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620")) return -1;
    *master = open(ptmx, O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (*master < 0) return -1;
    int unlock = 0;
    if (ioctl(*master, TIOCSPTLCK, &unlock)) return -1;
    *slave = ioctl(*master, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    return *slave < 0 ? -1 : 0;
}

/* Another pty from the same instance (open_pty first). */
static int another_pty(int *master, int *slave) {
    char ptmx[128];
    snprintf(ptmx, sizeof ptmx, "%s/ptmx", base);
    *master = open(ptmx, O_RDWR | O_NOCTTY | O_CLOEXEC);
    int unlock = 0;
    if (*master < 0 || ioctl(*master, TIOCSPTLCK, &unlock)) return -1;
    *slave = ioctl(*master, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    return *slave < 0 ? -1 : 0;
}

/* The calling process takes `slave` as its controlling terminal, in a new
 * session. */
static int take_terminal(int slave) {
    if (setsid() < 0) return -1;
    return ioctl(slave, TIOCSCTTY, 0);
}

/* ---- T1–T3: devpts in a user namespace, TIOCGPTPEER, and why not a path ---- */

static void check_devpts(void) {
    if (rootless(0, "T1")) return;
    char pts[128], ptmx[128];
    snprintf(pts, sizeof pts, "%s/pts", base);
    snprintf(ptmx, sizeof ptmx, "%s/ptmx", base);
    if (mount("tmpfs", base, "tmpfs", MS_NOSUID, "mode=0755")) { printf("T1 tmpfs in the user namespace: %s\n", strerror(errno)); return; }
    if (mkdir(pts, 0755) || symlink("pts/ptmx", ptmx)) die("pts directory");

    int rc = mount("devpts", pts, "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620,gid=5");
    printf("T1 devpts newinstance with gid=5 (unmapped there), as rootless: %s\n", say(rc));
    if (rc == 0) umount(pts);
    rc = mount("devpts", pts, "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620");
    printf("T1 devpts newinstance without gid, as rootless: %s\n", say(rc));
    if (rc) return;
    int master = open(ptmx, O_RDWR | O_NOCTTY | O_CLOEXEC);
    printf("T1 ptmx through the ptmx -> pts/ptmx link: %s\n", master < 0 ? strerror(errno) : "ok");
    if (master < 0) return;
    unsigned index = 99;
    ioctl(master, TIOCGPTN, &index);

    int slave = ioctl(master, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    printf("T2 TIOCGPTPEER before unlockpt: %s\n", slave < 0 ? strerror(errno) : "ok");
    if (slave >= 0) close(slave);
    int unlock = 0;
    if (ioctl(master, TIOCSPTLCK, &unlock)) die("unlockpt");
    slave = ioctl(master, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (slave < 0) { printf("T2 TIOCGPTPEER after unlockpt: %s\n", strerror(errno)); return; }
    struct stat on, st;
    stat(pts, &on);
    fstat(slave, &st);
    printf("T2 TIOCGPTPEER after unlockpt: ok, index %u, rdev %u:%u, on this devpts: %s\n", index,
           major(st.st_rdev), minor(st.st_rdev), st.st_dev == on.st_dev ? "yes" : "NO");

    /* Without O_NOCTTY, does opening the slave make it the controlling
     * terminal of a session leader that has none? */
    for (int noctty = 1; noctty >= 0; noctty--) {
        pid_t c = spawn();
        if (c == 0) {
            const char *how_opened = noctty ? " | O_NOCTTY" : " (no O_NOCTTY)";
            if (setsid() < 0) { printf("T2 setsid: %s\n", strerror(errno)); _exit(0); }
            int s = ioctl(master, TIOCGPTPEER, O_RDWR | (noctty ? O_NOCTTY : 0));
            if (s < 0) { printf("T2 TIOCGPTPEER%s as a session leader: %s\n", how_opened, strerror(errno)); _exit(0); }
            int tty = open("/dev/tty", O_RDWR);
            printf("T2 a session leader opens it with TIOCGPTPEER%s: %s it as controlling terminal\n",
                   how_opened, tty >= 0 ? "gets" : "doesn't get");
            _exit(0);
        }
        waitpid(c, NULL, 0);
    }

    /* A second instance mounted over the first: a path now reaches B. */
    rc = mount("devpts", pts, "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620");
    if (rc) { printf("T3 a second devpts over the first: %s\n", strerror(errno)); return; }
    int again = ioctl(master, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    struct stat st2;
    fstat(again, &st2);
    printf("T3 TIOCGPTPEER with another devpts mounted over: %s, the first instance's slave: %s\n",
           again < 0 ? strerror(errno) : "ok", again >= 0 && st2.st_dev == on.st_dev ? "yes" : "NO");
    char by_name[160];
    snprintf(by_name, sizeof by_name, "%s/%u", pts, index);
    int named = open(by_name, O_RDWR | O_NOCTTY);
    struct stat st3;
    if (named < 0) printf("T3 opening pts/%u by path then: %s (the path no longer reaches the first instance)\n", index, strerror(errno));
    else if (fstat(named, &st3) == 0)
        printf("T3 opening pts/%u by path then: opens %s\n", index, st3.st_dev == on.st_dev ? "the first instance's pty" : "a pty of the instance on top");
}

/* ---- T4: the controlling terminal in a PID namespace ---- */

static void check_pid_namespace_ctty(void) {
    if (rootless(CLONE_NEWPID, "T4")) return;
    pid_t init = spawn();
    if (init > 0) { waitpid(init, NULL, 0); return; }
    /* PID 1: a session of its own, as Shipyard's init. */
    int master, slave;
    if (setsid() < 0 || open_pty(&master, &slave)) { printf("T4 PID 1's pty: %s\n", strerror(errno)); _exit(0); }
    int ready[2], done[2];
    if (pipe(ready) || pipe(done)) die("pipe");
    pid_t command = spawn();
    if (command == 0) {
        int rc = take_terminal(slave);
        int tty = open("/dev/tty", O_RDWR);
        printf("T4 PID %d (the command): setsid + TIOCSCTTY: %s; /dev/tty opens: %s; tcgetsid = itself: %s\n",
               getpid(), say(rc), tty >= 0 ? "yes" : strerror(errno), tcgetsid(slave) == getpid() ? "yes" : "no");
        poke(ready[1]);
        await(done[0], 5000);
        _exit(0);
    }
    await(ready[0], 5000);
    int rc = ioctl(slave, TIOCSCTTY, 0);
    printf("T4 PID %d (the init, its own session) TIOCSCTTY on the command's terminal: %s\n", getpid(), say(rc));
    rc = ioctl(slave, TIOCSCTTY, 1);
    printf("T4 the same, stealing (arg 1, needs CAP_SYS_ADMIN in the initial user namespace): %s\n", say(rc));
    poke(done[1]);
    waitpid(command, NULL, 0);
    _exit(0);
}

/* ---- T5: job control under the pty ---- */

static void check_job_control(void) {
    private_mounts();
    int master, slave;
    if (open_pty(&master, &slave)) { printf("T5 pty: %s\n", strerror(errno)); return; }
    int ready[2];
    if (pipe(ready)) die("pipe");
    pid_t leader = spawn();
    if (leader == 0) {
        close(master);
        alarm(10);
        if (take_terminal(slave)) { printf("T5 controlling terminal: %s\n", strerror(errno)); _exit(1); }
        signal(SIGTTOU, SIG_IGN); /* as a shell: it moves the foreground */
        const char *keys[] = { "^C", "^Z", "^\\" };
        for (int i = 0; i < 3; i++) {
            pid_t job = spawn();
            if (job == 0) { setpgid(0, 0); for (;;) pause(); }
            setpgid(job, job);
            tcsetpgrp(slave, job);
            poke(ready[1]);
            int status;
            waitpid(job, &status, WUNTRACED);
            printf("T5 %s typed: the foreground job %s\n", keys[i], how(status));
            end(job);
            tcsetpgrp(slave, getpgrp());
        }
        pid_t reader = spawn();
        if (reader == 0) { setpgid(0, 0); char c; (void)!read(slave, &c, 1); _exit(0); }
        setpgid(reader, reader);
        int status;
        waitpid(reader, &status, WUNTRACED);
        printf("T5 a background job reads the terminal: %s\n", how(status));
        end(reader);
        for (int blocked = 0; blocked <= 1; blocked++) {
            pid_t setter = spawn();
            if (setter == 0) {
                setpgid(0, 0);
                if (blocked) {
                    sigset_t ttou;
                    sigemptyset(&ttou);
                    sigaddset(&ttou, SIGTTOU);
                    sigprocmask(SIG_BLOCK, &ttou, NULL);
                }
                struct termios t;
                tcgetattr(slave, &t);
                cfmakeraw(&t);
                _exit(tcsetattr(slave, TCSANOW, &t) == 0 ? 0 : 1);
            }
            setpgid(setter, setter);
            waitpid(setter, &status, WUNTRACED);
            printf("T5 a background process sets raw mode%s: %s\n", blocked ? " with SIGTTOU blocked" : "", how(status));
            end(setter);
        }
        _exit(0);
    }
    close(slave);
    const char keys[] = { 0x03, 0x1a, 0x1c };
    for (int i = 0; i < 3; i++) {
        if (!await(ready[0], 5000)) break;
        sleep_ms(50); /* the job is in pause() */
        (void)!write(master, &keys[i], 1);
    }
    /* keep the master open (and drained) until the leader is done */
    int status;
    while (waitpid(leader, &status, WNOHANG) == 0) {
        char sink[256];
        struct pollfd p = { .fd = master, .events = POLLIN };
        if (poll(&p, 1, 50) == 1) (void)!read(master, sink, sizeof sink);
    }
}

/* ---- T6: SCM_RIGHTS across namespaces ---- */

static int send_fd(int sock, int fd) {
    char data = 'm';
    struct iovec io = { .iov_base = &data, .iov_len = 1 };
    union { char buf[CMSG_SPACE(sizeof(int))]; struct cmsghdr align; } u;
    struct msghdr msg = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = u.buf, .msg_controllen = sizeof u.buf };
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof fd);
    return sendmsg(sock, &msg, 0) == 1 ? 0 : -1;
}

static int receive_fd(int sock) {
    char data;
    struct iovec io = { .iov_base = &data, .iov_len = 1 };
    union { char buf[CMSG_SPACE(sizeof(int))]; struct cmsghdr align; } u;
    struct msghdr msg = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = u.buf, .msg_controllen = sizeof u.buf };
    ssize_t n = recvmsg(sock, &msg, MSG_CMSG_CLOEXEC);
    if (n == 0) errno = ENODATA; /* the container side ended without sending */
    if (n != 1) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    if (!c || c->cmsg_type != SCM_RIGHTS) { errno = EBADMSG; return -1; }
    int fd;
    memcpy(&fd, CMSG_DATA(c), sizeof fd);
    return fd;
}

static void check_scm_rights(void) {
    int sock[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sock)) die("socketpair");
    pid_t container = spawn();
    if (container == 0) {
        close(sock[0]);
        if (rootless(CLONE_NEWPID, "T6")) _exit(0);
        pid_t init = spawn();
        if (init == 0) {
            int master, slave;
            if (open_pty(&master, &slave)) { printf("T6 the container's pty: %s\n", strerror(errno)); _exit(0); }
            if (send_fd(sock[1], master)) printf("T6 sendmsg(SCM_RIGHTS): %s\n", strerror(errno));
            close(master);
            /* PID 1 drops alarm()'s SIGALRM: the wait is a poll */
            char line[64] = "";
            ssize_t n = -1;
            struct pollfd p = { .fd = slave, .events = POLLIN };
            if (poll(&p, 1, 5000) == 1) n = read(slave, line, sizeof line - 1);
            else errno = ETIMEDOUT;
            if (n > 0) {
                line[n] = 0;
                line[strcspn(line, "\n")] = 0;
                printf("T6 the slave inside (PID %d, user+mount+PID namespaces) reads: \"%s\"\n", getpid(), line);
            } else printf("T6 the slave inside: %s\n", n == 0 ? "end-of-file" : strerror(errno));
            _exit(0);
        }
        int status;
        waitpid(init, &status, 0);
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) printf("   (the container's init %s)\n", how(status));
        _exit(0);
    }
    close(sock[1]);
    int master = receive_fd(sock[0]);
    if (master < 0) printf("T6 receiving the master: %s\n", errno == ENODATA ? "nothing came (the container side failed)" : strerror(errno));
    else {
        unsigned index = 99;
        int rc = ioctl(master, TIOCGPTN, &index);
        printf("T6 the host receives the master: ok; TIOCGPTN: %s, index %u\n", say(rc), index);
        (void)!write(master, "hello\n", 6);
    }
    waitpid(container, NULL, 0);
}

/* ---- T7: window size and SIGWINCH ---- */

static void check_winsize(void) {
    private_mounts();
    int master, slave;
    if (open_pty(&master, &slave)) { printf("T7 pty: %s\n", strerror(errno)); return; }
    int ready[2];
    if (pipe(ready)) die("pipe");
    struct winsize ws;
    ioctl(slave, TIOCGWINSZ, &ws);
    printf("T7 a new pty's size: %ux%u\n", ws.ws_row, ws.ws_col);
    pid_t leader = spawn();
    if (leader == 0) {
        sigset_t winch;
        sigemptyset(&winch);
        sigaddset(&winch, SIGWINCH);
        sigprocmask(SIG_BLOCK, &winch, NULL);
        if (take_terminal(slave)) { printf("T7 controlling terminal: %s\n", strerror(errno)); _exit(1); }
        poke(ready[1]);
        struct timespec one = { 1, 0 }, short_wait = { 0, 300000000 };
        int got = sigtimedwait(&winch, NULL, &one);
        ioctl(slave, TIOCGWINSZ, &ws);
        printf("T7 TIOCSWINSZ 40x100 on the master: the foreground %s SIGWINCH; the slave reads %ux%u\n",
               got == SIGWINCH ? "gets" : "doesn't get", ws.ws_row, ws.ws_col);
        poke(ready[1]);
        got = sigtimedwait(&winch, NULL, &short_wait);
        printf("T7 the same size set again: %s\n", got == SIGWINCH ? "SIGWINCH again" : "no SIGWINCH");
        _exit(0);
    }
    struct winsize big = { .ws_row = 40, .ws_col = 100 };
    if (await(ready[0], 5000)) ioctl(master, TIOCSWINSZ, &big);
    if (await(ready[0], 5000)) ioctl(master, TIOCSWINSZ, &big);
    waitpid(leader, NULL, 0);
}

/* ---- T8: the master after the slave's last close ---- */

static void check_slave_close(void) {
    private_mounts();
    int master, slave;
    if (open_pty(&master, &slave)) { printf("T8 pty: %s\n", strerror(errno)); return; }
    for (int held = 0; held <= 1; held++) {
        if (held && another_pty(&master, &slave)) { printf("T8 second pty: %s\n", strerror(errno)); return; }
        pid_t holder = -1;
        if (held) {
            holder = spawn();
            if (holder == 0) { alarm(10); for (;;) pause(); }
        }
        pid_t writer = spawn();
        if (writer == 0) { (void)!write(slave, "last words\n", 11); _exit(0); }
        close(slave);
        waitpid(writer, NULL, 0);
        sleep_ms(100);
        struct pollfd p = { .fd = master, .events = POLLIN };
        int ready = poll(&p, 1, 300);
        char buf[64] = "";
        ssize_t n = read(master, buf, sizeof buf - 1);
        int first = errno;
        if (n > 0) buf[n] = 0;
        char events[16] = "";
        if (p.revents & POLLIN) strcat(events, " IN");
        if (p.revents & POLLHUP) strcat(events, " HUP");
        printf("T8 %s: poll %d (revents%s), read %zd bytes%s%s\n",
               held ? "the writer exits, another process still holds the slave" : "the slave's last holder writes and exits",
               ready, events[0] ? events : " none", n,
               n < 0 ? ": " : "", n < 0 ? strerror(first) : "");
        fcntl(master, F_SETFL, O_NONBLOCK);
        n = read(master, buf, sizeof buf);
        printf("T8   then a second read: %s\n", n < 0 ? strerror(errno) : "data");
        if (held) {
            end(holder);
            sleep_ms(50);
            n = read(master, buf, sizeof buf);
            printf("T8   once the holder is gone: %s\n", n < 0 ? strerror(errno) : "data");
        }
        close(master);
    }
}

/* ---- T9: closing the master ---- */

static volatile sig_atomic_t hangups;
static void on_hup(int sig) { (void)sig; hangups++; }

static void check_master_close(void) {
    private_mounts();
    int master, slave;
    if (open_pty(&master, &slave)) { printf("T9 pty: %s\n", strerror(errno)); return; }
    int ready[2];
    if (pipe(ready)) die("pipe");
    pid_t leader = spawn();
    if (leader == 0) {
        close(master);
        alarm(10);
        signal(SIGHUP, on_hup);
        if (take_terminal(slave)) { printf("T9 controlling terminal: %s\n", strerror(errno)); _exit(1); }
        signal(SIGTTOU, SIG_IGN);
        pid_t job = spawn();
        if (job == 0) { setpgid(0, 0); for (;;) pause(); }
        setpgid(job, job);
        tcsetpgrp(slave, job);
        kill(job, SIGSTOP);
        int status;
        waitpid(job, &status, WUNTRACED);
        poke(ready[1]);
        sleep_ms(300);
        printf("T9 the master closed: the session leader got SIGHUP: %s\n", hangups ? "yes" : "no");
        char seen[128] = "";
        while (waitpid(job, &status, WUNTRACED | WCONTINUED | WNOHANG) > 0) {
            if (seen[0]) strncat(seen, ", ", sizeof seen - strlen(seen) - 1);
            strncat(seen, how(status), sizeof seen - strlen(seen) - 1);
            if (WIFEXITED(status) || WIFSIGNALED(status)) break;
        }
        printf("T9   the stopped foreground job: %s\n", seen[0] ? seen : "nothing (still stopped)");
        char c;
        ssize_t n = read(slave, &c, 1);
        printf("T9   reading the slave afterwards: %s\n", n < 0 ? strerror(errno) : n == 0 ? "end-of-file" : "data");
        end(job);
        _exit(0);
    }
    close(slave);
    if (await(ready[0], 5000)) close(master);
    waitpid(leader, NULL, 0);
}

/* ---- T10: TIOCSTI ---- */

static void check_tiocsti(void) {
    int null = open("/dev/null", O_RDONLY);
    if (null < 0 || dup2(null, 0) < 0) die("stdin from /dev/null");
    char legacy[16];
    get("/proc/sys/dev/tty/legacy_tiocsti", legacy, sizeof legacy);
    printf("T10 dev.tty.legacy_tiocsti: %s\n", strcmp(legacy, "?") ? legacy : "absent (before 6.2: TIOCSTI allowed on one's own terminal, or anywhere with CAP_SYS_ADMIN)");
    char c = 'x';
    int p[2];
    if (pipe(p)) die("pipe");
    printf("T10 TIOCSTI on a pipe: %s\n", say(ioctl(p[0], TIOCSTI, &c)));

    pid_t root = spawn();
    if (root == 0) {
        private_mounts();
        int master, slave;
        if (open_pty(&master, &slave)) { printf("T10 pty: %s\n", strerror(errno)); _exit(0); }
        printf("T10 TIOCSTI as root of the initial user namespace, on a pty that isn't its terminal: %s\n",
               say(ioctl(slave, TIOCSTI, &c)));
        _exit(0);
    }
    waitpid(root, NULL, 0);

    pid_t contained = spawn();
    if (contained == 0) {
        if (rootless(0, "T10")) _exit(0);
        int master, slave, master2, slave2;
        if (open_pty(&master, &slave) || another_pty(&master2, &slave2)) { printf("T10 pty: %s\n", strerror(errno)); _exit(0); }
        if (take_terminal(slave)) { printf("T10 controlling terminal: %s\n", strerror(errno)); _exit(0); }
        printf("T10 TIOCSTI as root of a user namespace, on its own controlling terminal: %s\n", say(ioctl(slave, TIOCSTI, &c)));
        printf("T10 TIOCSTI as root of a user namespace, on another terminal: %s\n", say(ioctl(slave2, TIOCSTI, &c)));
        _exit(0);
    }
    waitpid(contained, NULL, 0);
}

/* ---- T11: dumpability across setns ---- */

/* A container (a user namespace mapping 0 -> 100000, PID and mount
 * namespaces, its own /proc), and a process joining it as exec does:
 * setns(pid) -> fork -> setns(user, mount) -> credentials. From root in the
 * container: can it ptrace the joiner, or open its exe and fd directory? */
static void joiner(int hidden) {
    int up[2], go[2], found[2], done[2];
    if (pipe(up) || pipe(go) || pipe(found) || pipe(done)) die("pipe");
    pid_t outer = spawn();
    if (outer == 0) {
        if (unshare(CLONE_NEWUSER)) { printf("T11 user namespace: %s\n", strerror(errno)); _exit(0); }
        poke(up[1]);
        if (!await(go[0], 5000) || setresgid(0, 0, 0) || setresuid(0, 0, 0)) die("the container's credentials");
        if (unshare(CLONE_NEWPID | CLONE_NEWNS)) die("PID and mount namespaces");
        pid_t init = spawn();
        if (init == 0) {
            if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) || mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) die("the container's /proc");
            pid_t target = 0;
            if (read_within(found[0], &target, sizeof target, 10000) || target <= 0) {
                printf("T11 the joiner never arrived: %s\n", strerror(errno));
                poke(done[1]);
                _exit(0);
            }
            char path[64];
            snprintf(path, sizeof path, "/proc/%d/exe", target);
            int exe = open(path, O_RDONLY);
            int exe_errno = errno;
            snprintf(path, sizeof path, "/proc/%d/fd", target);
            DIR *fds = opendir(path);
            int fd_errno = errno;
            int traced = ptrace(PTRACE_ATTACH, target, 0, 0);
            int trace_errno = errno;
            printf("T11 %s joiner, from root in the container: open exe %s, list fd %s, PTRACE_ATTACH %s\n",
                   hidden ? "a non-dumpable" : "a dumpable (the control)",
                   exe >= 0 ? "OK (the host binary)" : strerror(exe_errno), fds ? "OK" : strerror(fd_errno),
                   traced == 0 ? "OK" : strerror(trace_errno));
            kill(target, SIGKILL);
            poke(done[1]);
            _exit(0);
        }
        (void)!write(up[1], &init, sizeof init);
        waitpid(init, NULL, 0);
        _exit(0);
    }
    if (!await(up[0], 5000)) die("the container's user namespace");
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/uid_map", outer);
    if (put(path, "0 100000 65536")) die("uid_map");
    snprintf(path, sizeof path, "/proc/%d/gid_map", outer);
    if (put(path, "0 100000 65536")) die("gid_map");
    poke(go[1]);
    pid_t init = 0;
    if (read_within(up[0], &init, sizeof init, 5000)) die("the container's init");

    pid_t join = spawn();
    if (join == 0) {
        if (hidden && prctl(PR_SET_DUMPABLE, 0)) die("PR_SET_DUMPABLE 0");
        char ns[64];
        snprintf(ns, sizeof ns, "/proc/%d/ns/pid", init);
        int pid_ns = open(ns, O_RDONLY | O_CLOEXEC);
        snprintf(ns, sizeof ns, "/proc/%d/ns/user", init);
        int user_ns = open(ns, O_RDONLY | O_CLOEXEC);
        snprintf(ns, sizeof ns, "/proc/%d/ns/mnt", init);
        int mnt_ns = open(ns, O_RDONLY | O_CLOEXEC);
        if (pid_ns < 0 || user_ns < 0 || mnt_ns < 0 || setns(pid_ns, CLONE_NEWPID)) die("the container's namespaces");
        pid_t joined = spawn();
        if (joined == 0) {
            if (setns(user_ns, CLONE_NEWUSER) || setns(mnt_ns, CLONE_NEWNS) || chdir("/")) die("setns(user, mount)");
            int after_setns = prctl(PR_GET_DUMPABLE);
            if (setresgid(0, 0, 0) || setresuid(0, 0, 0)) die("the joiner's credentials");
            int after_ids = prctl(PR_GET_DUMPABLE);
            if (!hidden && prctl(PR_SET_DUMPABLE, 1)) die("PR_SET_DUMPABLE 1");
            printf("T11 %s: dumpable after setns %d, after switching to the container's root %d\n",
                   hidden ? "PR_SET_DUMPABLE 0 before the first setns" : "never set, made dumpable again after the switch (the control)",
                   after_setns, after_ids);
            pid_t me = getpid();
            (void)!write(found[1], &me, sizeof me);
            alarm(20); /* not PID 1 here: it ends this if the container's init never does */
            for (;;) pause();
        }
        waitpid(joined, NULL, 0);
        _exit(0);
    }
    await(done[0], 10000);
    waitpid(join, NULL, 0);
    waitpid(outer, NULL, 0);
}

static void check_dumpable(void) {
    char scope[16];
    get("/proc/sys/kernel/yama/ptrace_scope", scope, sizeof scope);
    printf("T11 kernel.yama.ptrace_scope: %s (above 0, PTRACE_ATTACH needs more than dumpability)\n", scope);
    joiner(1);
    joiner(0);
    pid_t c = spawn();
    if (c == 0) {
        if (prctl(PR_SET_DUMPABLE, 0)) die("PR_SET_DUMPABLE 0");
        char *argv[] = { "terminal", "dumpable", NULL };
        execv("/proc/self/exe", argv);
        die("execv");
    }
    waitpid(c, NULL, 0);
}

/* ---- T12: openat2 confined to a container's root ---- */

static int open2(int dir, const char *path, uint64_t resolve) {
    struct open_how how = { .flags = O_RDONLY | O_CLOEXEC, .resolve = resolve };
    return syscall(SYS_openat2, dir, path, &how, sizeof how);
}

/* "reads \"…\"" for an opened file, or the error. */
static const char *contents(int fd) {
    static char text[48];
    if (fd < 0) return strerror(errno);
    char data[32];
    ssize_t n = read(fd, data, sizeof data - 1);
    close(fd);
    data[n < 0 ? 0 : n] = 0;
    snprintf(text, sizeof text, "reads \"%s\"", data);
    return text;
}

static void check_openat2(void) {
    int ready[2];
    if (pipe(ready)) die("pipe");
    pid_t container = spawn();
    if (container == 0) {
        private_mounts();
        if (mount("tmpfs", inner, "tmpfs", 0, "mode=0755")) die("the container's tmpfs");
        if (chdir(inner) || mkdir("root", 0755) || mkdir("root/etc", 0755) || mkdir("root/proc", 0755)) die("the container's root");
        if (make_file("root/etc/marker", "inside")) die("marker");
        if (symlink("/etc/marker", "root/abs") || symlink("../../host-only", "root/up")) die("links");
        if (mount("proc", "root/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) die("proc");
        if (chroot("root") || chdir("/")) die("chroot");
        poke(ready[1]);
        for (;;) pause();
    }
    if (!await(ready[0], 5000)) { end(container); return; }
    char path[160];
    snprintf(path, sizeof path, "/proc/%d/root", container);
    int root = open(path, O_PATH | O_DIRECTORY | O_CLOEXEC);
    printf("T12 an absolute link, RESOLVE_IN_ROOT: %s\n", contents(open2(root, "abs", RESOLVE_IN_ROOT)));
    printf("T12 a link out by \"..\", RESOLVE_IN_ROOT: %s\n", contents(open2(root, "up", RESOLVE_IN_ROOT)));
    printf("T12   the control, plain openat: %s\n", contents(openat(root, "up", O_RDONLY)));
    snprintf(path, sizeof path, "proc/self/root%s", host_only);
    printf("T12 through /proc/self/root, plain openat (the control): %s\n", contents(openat(root, path, O_RDONLY)));
    printf("T12   RESOLVE_IN_ROOT alone: %s\n", contents(open2(root, path, RESOLVE_IN_ROOT)));
    printf("T12   with RESOLVE_NO_MAGICLINKS: %s\n", contents(open2(root, path, RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)));
    close(root);
    end(container);

    /* Rootless: nobody's container, with subordinate IDs mapped by root. */
    int up[2], go[2];
    if (pipe(up) || pipe(go)) die("pipe");
    container = spawn();
    if (container == 0) {
        to_nobody();
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS)) { printf("T12 user namespace as nobody: %s\n", strerror(errno)); _exit(0); }
        poke(up[1]);
        if (!await(go[0], 5000) || setresgid(0, 0, 0) || setresuid(0, 0, 0)) die("the container's credentials");
        /* as an exec'd command would be again */
        if (prctl(PR_SET_DUMPABLE, 1)) die("PR_SET_DUMPABLE");
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) || mount("tmpfs", inner, "tmpfs", 0, "mode=0755")) die("tmpfs");
        if (chdir(inner) || mkdir("root", 0755)) die("root");
        int own = open("root/own", O_CREAT | O_WRONLY, 0600);
        int sub = open("root/sub", O_CREAT | O_WRONLY, 0600);
        if (own < 0 || sub < 0 || fchown(sub, 1, 1)) die("files");
        if (chroot("root") || chdir("/")) die("chroot");
        poke(ready[1]);
        for (;;) pause();
    }
    if (!await(up[0], 5000)) { waitpid(container, NULL, 0); return; }
    char map[64];
    snprintf(path, sizeof path, "/proc/%d/uid_map", container);
    snprintf(map, sizeof map, "0 %d 1\n1 100000 65536", NOBODY);
    if (put(path, map)) die("uid_map");
    snprintf(path, sizeof path, "/proc/%d/gid_map", container);
    if (put(path, map)) die("gid_map");
    poke(go[1]);
    if (await(ready[0], 5000)) {
        pid_t reader = spawn();
        if (reader == 0) {
            to_nobody();
            snprintf(path, sizeof path, "/proc/%d/root", container);
            int r = open(path, O_PATH | O_DIRECTORY | O_CLOEXEC);
            printf("T12 nobody opens its container's /proc/<pid>/root: %s\n", r < 0 ? strerror(errno) : "ok");
            int fd = open2(r, "own", RESOLVE_IN_ROOT);
            printf("T12   a 0600 file of the container's root (nobody itself): %s\n", fd < 0 ? strerror(errno) : "readable");
            fd = open2(r, "sub", RESOLVE_IN_ROOT);
            printf("T12   a 0600 file of the container's uid 1 (subordinate 100000): %s\n", fd < 0 ? strerror(errno) : "readable");
            _exit(0);
        }
        waitpid(reader, NULL, 0);
    }
    end(container);
}

/* ---- T13: CLONE_INTO_CGROUP ---- */

static char scratch[96];

static const char *at(const char *sub) {
    static char path[4][192];
    static int next;
    char *p = path[next++ % 4];
    snprintf(p, sizeof path[0], "%s/%s", scratch, sub);
    return p;
}

/* Starts a child in the cgroup at `dir`: it writes one byte to `out` and
 * exits. Returns its PID, or -1 with errno. */
static pid_t start_in(const char *dir, int out) {
    int fd = open(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) return -1;
    struct clone_args args = { .flags = CLONE_INTO_CGROUP, .exit_signal = SIGCHLD, .cgroup = (uint64_t)fd };
    pid_t c = syscall(SYS_clone3, &args, sizeof args);
    if (c == 0) { poke(out); _exit(0); }
    int saved = errno;
    close(fd);
    errno = saved;
    return c;
}

/* What T13 enabled at the root ("-memory -pids" to undo it), and the
 * process whose exit cleans up (not a child's). */
static char cgroup_undo[32];
static pid_t cgroup_owner;

/* Removes the scratch cgroups (killing what's left in them) and disables
 * what the setup enabled. Runs on any exit of T13's process, die()s too. */
static void cgroup_clean_up(void) {
    if (getpid() != cgroup_owner) return;
    cgroup_owner = 0;
    const char *leaves[] = { "leaf", "frozen" };
    for (int l = 0; l < 2; l++) {
        char kill_file[64];
        snprintf(kill_file, sizeof kill_file, "%s/cgroup.kill", leaves[l]);
        put(at(kill_file), "1");
        for (int i = 0; i < 50 && rmdir(at(leaves[l])) && errno == EBUSY; i++) sleep_ms(20);
    }
    for (int i = 0; i < 50 && rmdir(scratch) && errno == EBUSY; i++) sleep_ms(20);
    if (cgroup_undo[0] && put(CGROOT "/cgroup.subtree_control", cgroup_undo))
        printf("T13 cleanup: undoing \"%s\" at the root: %s\n", cgroup_undo, strerror(errno));
}

/* Memory (a domain controller) and pids handed down from the root, if they
 * aren't already (on a systemd host they are, and nothing changes). A
 * container's cgroup namespace root holds processes, so the write is EBUSY
 * there, and only then are its processes moved into a leaf (/init) first, as
 * the cgroup probe does. (Enabling pids alone would succeed there, but leave
 * every new child "domain invalid": EOPNOTSUPP to join.) What's enabled is
 * disabled again at the end. Returns -1, saying why, to skip T13. */
static int cgroup_set_up(void) {
    char available[256], down[256], add[32] = "";
    get(CGROOT "/cgroup.controllers", available, sizeof available);
    if (!strstr(available, "memory") || !strstr(available, "pids")) {
        printf("T13 skipped: the root cgroup has no memory or pids controller (it has: %s)\n", available);
        return -1;
    }
    get(CGROOT "/cgroup.subtree_control", down, sizeof down);
    if (!strstr(down, "memory")) strcat(add, "+memory ");
    if (!strstr(down, "pids")) strcat(add, "+pids");
    if (!add[0]) return 0;
    if (put(CGROOT "/cgroup.subtree_control", add)) {
        if (errno != EBUSY) { printf("T13 skipped: enabling %s at the root: %s\n", add, strerror(errno)); return -1; }
        printf("setup: this cgroup namespace's root holds processes (EBUSY); moving them to %s/init\n", CGROOT);
        if (mkdir(CGROOT "/init", 0755) && errno != EEXIST) die("init");
        char text[8192];
        for (int pass = 0; pass < 5; pass++) {
            get(CGROOT "/cgroup.procs", text, sizeof text);
            if (!text[0]) break;
            for (char *save, *line = strtok_r(text, "\n", &save); line; line = strtok_r(NULL, "\n", &save))
                put(CGROOT "/init/cgroup.procs", line);
        }
        if (put(CGROOT "/cgroup.subtree_control", add)) { printf("T13 skipped: enabling %s at the root: %s\n", add, strerror(errno)); return -1; }
    }
    snprintf(cgroup_undo, sizeof cgroup_undo, "%s", add);
    for (char *sign = cgroup_undo; (sign = strchr(sign, '+')); sign++) *sign = '-';
    return 0;
}

/* A fresh pipe for each start, so one child's byte can't answer for another. */
static void fresh_pipe(int out[2]) {
    if (out[0] >= 0) { close(out[0]); close(out[1]); }
    if (pipe(out)) die("pipe");
}

static void check_clone_into_cgroup(void) {
    if (cgroup_set_up()) return;
    snprintf(scratch, sizeof scratch, "%s/shipyard-probe-tty.%d", CGROOT, getpid());
    cgroup_owner = getpid();
    atexit(cgroup_clean_up);
    if (mkdir(scratch, 0755) || mkdir(at("leaf"), 0755) || mkdir(at("frozen"), 0755)) die("the scratch cgroups");
    int out[2] = { -1, -1 };
    printf("T13 handing memory and pids down from the parent: %s\n", say(put(at("cgroup.subtree_control"), "+memory +pids")));
    fresh_pipe(out);
    pid_t c = start_in(scratch, out[1]);
    printf("T13 CLONE_INTO_CGROUP into a cgroup with controllers in subtree_control: %s\n", c < 0 ? strerror(errno) : "ok");
    if (c > 0) waitpid(c, NULL, 0);
    fresh_pipe(out);
    c = start_in(at("leaf"), out[1]);
    printf("T13 into a leaf: %s, it ran: %s\n", c < 0 ? strerror(errno) : "ok", c > 0 && await(out[0], 2000) ? "yes" : "no");
    if (c > 0) waitpid(c, NULL, 0);
    fresh_pipe(out);
    if (put(at("frozen/cgroup.freeze"), "1")) die("freeze");
    c = start_in(at("frozen"), out[1]);
    if (c < 0) printf("T13 into a frozen leaf: %s\n", strerror(errno));
    else {
        int ran = await(out[0], 300);
        int thawed = put(at("frozen/cgroup.freeze"), "0") == 0;
        int after = thawed && await(out[0], 2000);
        printf("T13 into a frozen leaf: ok; it ran while frozen: %s, after thawing: %s\n", ran ? "YES" : "no",
               thawed ? (after ? "yes" : "no") : "the thaw failed");
        put(at("frozen/cgroup.kill"), "1"); /* never wait on a child still frozen */
        waitpid(c, NULL, 0);
    }
    cgroup_clean_up();
    struct stat st;
    printf("T13 scratch cgroup removed: %s\n", stat(scratch, &st) ? "yes" : "NO");
}

/* ---- T14: a pidfd in poll ---- */

static void check_pidfd(void) {
    int idle[2];
    if (pipe(idle)) die("pipe");
    pid_t c = spawn();
    if (c == 0) { sleep_ms(200); _exit(7); }
    int pidfd = syscall(SYS_pidfd_open, c, 0);
    if (pidfd < 0) { printf("T14 pidfd_open: %s\n", strerror(errno)); waitpid(c, NULL, 0); return; }
    struct pollfd p[2] = { { .fd = pidfd, .events = POLLIN }, { .fd = idle[0], .events = POLLIN } };
    long start = now_ms();
    int n = poll(p, 2, 2000);
    long took = now_ms() - start;
    int status;
    waitpid(c, &status, 0);
    printf("T14 poll on a pidfd and an idle pipe: %d ready after %ld ms; the pidfd %s, the pipe %s; the child %s\n", n, took,
           p[0].revents & POLLIN ? "readable" : "not readable", p[1].revents ? "ready" : "not ready", how(status));
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "dumpable") == 0) {
        printf("T11 PR_SET_DUMPABLE 0, then execve: dumpable %d\n", prctl(PR_GET_DUMPABLE));
        return 0;
    }
    setvbuf(stdout, NULL, _IONBF, 0);
    struct utsname uts;
    uname(&uts);
    printf("kernel: %s %s\n", uts.release, uts.machine);
    if (geteuid() != 0) { printf("run it as root (it drops to an ordinary user itself)\n"); return 2; }
    char restrict_userns[16];
    get("/proc/sys/kernel/apparmor_restrict_unprivileged_userns", restrict_userns, sizeof restrict_userns);
    printf("kernel.apparmor_restrict_unprivileged_userns: %s\n", strcmp(restrict_userns, "?") ? restrict_userns : "absent");

    snprintf(base, sizeof base, "/tmp/shipyard-probe-tty.XXXXXX");
    if (!mkdtemp(base) || chmod(base, 0755)) die("the scratch directory");
    snprintf(inner, sizeof inner, "%s/inner", base);
    snprintf(host_only, sizeof host_only, "%s/host-only", base);
    if (mkdir(inner, 0755)) die("inner");
    if (make_file(host_only, "HOST")) die("host-only");

    in_child(check_devpts);
    in_child(check_pid_namespace_ctty);
    in_child(check_job_control);
    in_child(check_scm_rights);
    in_child(check_winsize);
    in_child(check_slave_close);
    in_child(check_master_close);
    in_child(check_tiocsti);
    in_child(check_dumpable);
    in_child(check_openat2);
    in_child(check_clone_into_cgroup);
    in_child(check_pidfd);

    unlink(host_only);
    rmdir(inner);
    rmdir(base);
    struct stat st;
    printf("scratch directory removed: %s\n", stat(base, &st) ? "yes" : "NO");
    return 0;
}
