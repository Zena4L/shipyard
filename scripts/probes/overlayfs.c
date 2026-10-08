/*
 * Kernel checks behind the overlay layer driver (stage 6): whether an
 * unprivileged user namespace can mount overlayfs at all (Linux 5.11+),
 * whether it needs the `userxattr` option, how whiteouts and opaque
 * directories behave for an unprivileged mounter, and what happens where the
 * kernel refuses (so the refusal message is built against real behaviour),
 * and how many layers each way of mounting can stack (one `mount(2)` takes
 * at most a page of options; the new mount API's `lowerdir+`, Linux 6.8,
 * takes one layer per call). Each line of output answers one question;
 * docs/stages/06-images.md records the results.
 *
 * The rootless checks run in a forked child that first enters a new user and
 * mount namespace (unshare -Um, root mapped to the caller), so they mirror
 * exactly what Shipyard's rootless overlay does, and leave the host's mount
 * table untouched. The privileged check runs in its own mount namespace too.
 *
 * Rootless overlayfs needs an unprivileged user namespace to be allowed by
 * the host; run it where that is the case. CAP_SYS_ADMIN in the initial
 * namespace is not required for the rootless checks (that is the point), but
 * the privileged baseline check needs it, so run the probe in a privileged
 * container to get both:
 *
 *   docker run --rm --privileged -v "$PWD/scripts/probes":/p:ro rust:1-bookworm \
 *       sh -c 'gcc -Wall -Wextra -o /tmp/overlayfs /p/overlayfs.c && /tmp/overlayfs'
 *
 * On an old kernel, or where unprivileged user namespaces are restricted
 * (AppArmor, sysctl), the rootless mount lines print the kernel's error
 * instead of "ok"; that is what the refusal message explains.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#define BASE "/tmp/shipyard-probe-overlay"
#define LAYERS 100

#ifndef FSOPEN_CLOEXEC
#define FSOPEN_CLOEXEC 0x1
#define FSCONFIG_SET_STRING 1
#define FSCONFIG_CMD_CREATE 6
#define FSMOUNT_CLOEXEC 0x1
#define MOVE_MOUNT_F_EMPTY_PATH 0x4
#endif

static const char *err(int rc) { return rc == 0 ? "ok" : strerror(errno); }

static void die(const char *what) {
    printf("probe setup failed: %s: %s\n", what, strerror(errno));
    exit(2);
}

static void run(const char *command) {
    if (system(command)) {
        printf("probe setup failed: %s\n", command);
        exit(2);
    }
}

/* Writes `content` to `path`, or dies. */
static void write_file(const char *path, const char *content) {
    FILE *f = fopen(path, "w");
    if (!f) die(path);
    if (fputs(content, f) == EOF) die(path);
    if (fclose(f)) die(path);
}

/* Maps root in a new user namespace to the given host id, as a rootless
 * container does: deny setgroups, then write gid_map and uid_map. */
static void map_root_to(uid_t uid, gid_t gid) {
    char line[64];
    write_file("/proc/self/setgroups", "deny");
    snprintf(line, sizeof line, "0 %u 1", gid);
    write_file("/proc/self/gid_map", line);
    snprintf(line, sizeof line, "0 %u 1", uid);
    write_file("/proc/self/uid_map", line);
}

/* A fresh layout: two lower dirs, an upper, a work dir, and a merged mount
 * point, under BASE. BASE is a tmpfs the caller mounts in its own mount
 * namespace, so the overlay dirs are on a plain filesystem — overlayfs
 * refuses an upperdir that is itself on overlay (EINVAL), which a container's
 * own /tmp often is. lower1 holds `base` and `gone`; lower2 (the upper of the
 * two lowers) gets a whiteout of `gone`. */
static void make_layout(void) {
    if (mkdir(BASE, 0755) && errno != EEXIST) die("mkdir base");
    if (mount("tmpfs", BASE, "tmpfs", 0, NULL)) die("mount tmpfs on base");
    run("mkdir -p " BASE "/lower1/d " BASE "/lower2 " BASE "/upper " BASE "/work " BASE "/merged");
    write_file(BASE "/lower1/base", "from lower1\n");
    write_file(BASE "/lower1/gone", "should be hidden\n");
    write_file(BASE "/lower1/d/keep", "under d\n");
}

/* Tries one overlay mount with the given options string; returns 0 on
 * success. Does not unmount: the caller is in its own mount namespace. */
static int try_overlay(const char *opts) {
    return mount("overlay", BASE "/merged", "overlay", 0, opts);
}

static struct utsname uts;

int main(void) {
    uname(&uts);
    printf("kernel: %s\n", uts.release);

    uid_t uid = getuid();
    gid_t gid = getgid();

    /* 1. Privileged baseline: overlayfs in a plain mount namespace, as real
     * root. Confirms the layering and whiteout semantics the driver relies
     * on, independent of the rootless question. */
    fflush(stdout); /* so the forked child does not inherit a buffered copy */
    pid_t child = fork();
    if (child == 0) {
        if (unshare(CLONE_NEWNS)) die("unshare(NEWNS)");
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("make-rprivate");
        make_layout();
        int rc = try_overlay("lowerdir=" BASE "/lower2:" BASE "/lower1,"
                             "upperdir=" BASE "/upper,workdir=" BASE "/work");
        printf("privileged overlay mount: %s\n", err(rc));
        if (rc == 0) {
            run("mknod " BASE "/lower2/gone c 0 0 2>/dev/null");
            printf("  whiteout device node as root: %s\n",
                   access(BASE "/lower2/gone", F_OK) == 0 ? "made" : "could not make");
            printf("  merged hides lower1/gone: %s\n",
                   access(BASE "/merged/gone", F_OK) == 0 ? "no (still visible)" : "yes");
        }
        exit(0);
    }
    waitpid(child, NULL, 0);

    /* 2. Rootless: the real question. Enter a new user + mount namespace,
     * map root to the caller, and try the same mount, first without and then
     * with `userxattr`. On < 5.11 or where unprivileged userns overlay is
     * blocked, these print an error — the signal for the fuse fallback. */
    fflush(stdout);
    child = fork();
    if (child == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS)) {
            printf("rootless userns+mountns: %s\n", strerror(errno));
            printf("rootless overlay mount: skipped (no user namespace)\n");
            exit(0);
        }
        map_root_to(uid, gid);
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("make-rprivate");

        /* Without userxattr: stricter kernels reject this for an
         * unprivileged mounter (overlay would use trusted.overlay.*). */
        make_layout();
        int plain = try_overlay("lowerdir=" BASE "/lower2:" BASE "/lower1,"
                                "upperdir=" BASE "/upper,workdir=" BASE "/work");
        printf("rootless overlay mount (no userxattr): %s\n", err(plain));
        (void)umount(BASE); /* drop the attempt's tmpfs for a clean retry */

        /* With userxattr: overlay stores its state in user.overlay.*, which
         * an unprivileged mounter can set. The expected rootless path on
         * 5.11+. */
        make_layout();
        int withxattr = try_overlay("lowerdir=" BASE "/lower2:" BASE "/lower1,"
                                    "upperdir=" BASE "/upper,workdir=" BASE "/work,userxattr");
        printf("rootless overlay mount (userxattr): %s\n", err(withxattr));
        if (withxattr == 0 || plain == 0) {
            printf("  merged sees lower1/base: %s\n",
                   access(BASE "/merged/base", F_OK) == 0 ? "yes" : "no");
            write_file(BASE "/merged/base", "touched\n");
            printf("  copy-up on write: ok\n");
        }
        exit(0);
    }
    waitpid(child, NULL, 0);

    /* 3. metacopy / redirect_dir availability (module params), which the
     * driver must not depend on if the test kernel lacks them. */
    printf("metacopy param: %s\n",
           access("/sys/module/overlay/parameters/metacopy", F_OK) == 0 ? "present" : "absent");
    printf("redirect_dir param: %s\n",
           access("/sys/module/overlay/parameters/redirect_dir", F_OK) == 0 ? "present" : "absent");

    /* 4. Is fuse-overlayfs there? (Not used yet: a fallback through it
     * needs a helper outside the container's PID namespace.) */
    printf("fuse-overlayfs on PATH: %s\n",
           system("command -v fuse-overlayfs >/dev/null 2>&1") == 0 ? "yes" : "no");

    /* 5. How many layers: LAYERS layers with long names (as the store's
     * are), one legacy mount with all their absolute paths, then the new
     * mount API with one `lowerdir+` each. As root, in a mount namespace of
     * its own. */
    fflush(stdout);
    child = fork();
    if (child == 0) {
        if (unshare(CLONE_NEWNS)) die("unshare(NEWNS)");
        if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("make-rprivate");
        make_layout();
        static char paths[LAYERS][160];
        static char options[LAYERS * 170];
        size_t used = (size_t)snprintf(options, sizeof options, "lowerdir=");
        for (int i = 0; i < LAYERS; i++) {
            snprintf(paths[i], sizeof paths[i],
                     BASE "/layers/%064x/diff", i + 1);
            char command[256];
            snprintf(command, sizeof command, "mkdir -p %s", paths[i]);
            run(command);
            used += (size_t)snprintf(options + used, sizeof options - used, "%s%s",
                                     i ? ":" : "", paths[i]);
        }
        snprintf(options + used, sizeof options - used,
                 ",upperdir=" BASE "/upper,workdir=" BASE "/work");
        printf("legacy mount of %d layers (%zu bytes of options): %s\n", LAYERS,
               strlen(options), err(try_overlay(options)));
        (void)umount(BASE "/merged");

        int fs = (int)syscall(SYS_fsopen, "overlay", FSOPEN_CLOEXEC);
        printf("fsopen(overlay): %s\n", fs >= 0 ? "ok" : strerror(errno));
        if (fs < 0) exit(0);
        int first = (int)syscall(SYS_fsconfig, fs, FSCONFIG_SET_STRING, "lowerdir+", paths[0], 0);
        printf("fsconfig lowerdir+ (Linux 6.8): %s\n", first == 0 ? "ok" : strerror(errno));
        if (first != 0) exit(0);
        for (int i = 1; i < LAYERS; i++)
            if (syscall(SYS_fsconfig, fs, FSCONFIG_SET_STRING, "lowerdir+", paths[i], 0)) die("lowerdir+");
        if (syscall(SYS_fsconfig, fs, FSCONFIG_SET_STRING, "upperdir", BASE "/upper", 0)) die("upperdir");
        if (syscall(SYS_fsconfig, fs, FSCONFIG_SET_STRING, "workdir", BASE "/work", 0)) die("workdir");
        int created = (int)syscall(SYS_fsconfig, fs, FSCONFIG_CMD_CREATE, NULL, NULL, 0);
        int mounted = created == 0 ? (int)syscall(SYS_fsmount, fs, FSMOUNT_CLOEXEC, 0) : -1;
        int moved = mounted >= 0 ? (int)syscall(SYS_move_mount, mounted, "", AT_FDCWD,
                                                BASE "/merged", MOVE_MOUNT_F_EMPTY_PATH)
                                 : -1;
        printf("new mount API with %d layers: %s\n", LAYERS, moved == 0 ? "ok" : strerror(errno));
        exit(0);
    }
    waitpid(child, NULL, 0);
    return 0;
}
