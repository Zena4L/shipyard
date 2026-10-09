/*
 * Kernel checks behind container networking (stage 7): what a network
 * namespace of the container's own allows (a sysfs mount, `net.*` sysctls,
 * even from a user namespace), how a veth pair reaches into a namespace, why
 * a netlink socket belongs to the namespace it was made in, which nf_tables
 * pieces the NAT and port rules use, conntrack's netlink interface, how long
 * a namespace lingers after its last process, and what the kernel's
 * extended ACK says. Each line of output answers one question;
 * docs/stages/07-networking.md records the results.
 *
 * Everything that changes networking happens in network namespaces the probe
 * makes and throws away: the host's (or the container's) interfaces, routes
 * and firewall are never touched. It needs root (CAP_NET_ADMIN, CAP_SYS_ADMIN)
 * for the veth, nf_tables and setns checks; run it in a privileged container:
 *
 *   docker run --rm --privileged -v "$PWD/scripts/probes":/p:ro rust:1-bookworm \
 *       sh -c 'gcc -Wall -Wextra -o /tmp/network /p/network.c && /tmp/network'
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/if_link.h>
#include <linux/netfilter.h>
#include <linux/netfilter/nf_tables.h>
#include <linux/netfilter/nfnetlink.h>
#include <linux/netfilter/nfnetlink_conntrack.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/veth.h>
#include <net/if.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef NETLINK_EXT_ACK
#define NETLINK_EXT_ACK 11
#endif
#ifndef NLM_F_ACK_TLVS
#define NLM_F_ACK_TLVS 0x200
#endif

static void die(const char *what) {
    printf("probe setup failed: %s: %s\n", what, strerror(errno));
    exit(2);
}

/* ---- A tiny netlink message builder: a header, a family header, and
 * attributes (nested ones with their length patched when they close). ---- */

struct msg {
    char buf[8192];
    size_t len;
    size_t nests[8];
    int depth;
};

static struct nlmsghdr *hdr(struct msg *m) { return (struct nlmsghdr *)m->buf; }

static void msg_init(struct msg *m, uint16_t type, uint16_t flags, const void *family,
                     size_t family_len) {
    memset(m, 0, sizeof *m);
    m->len = NLMSG_HDRLEN;
    hdr(m)->nlmsg_type = type;
    hdr(m)->nlmsg_flags = NLM_F_REQUEST | flags;
    memcpy(m->buf + m->len, family, family_len);
    m->len += NLMSG_ALIGN(family_len);
}

static void attr(struct msg *m, uint16_t type, const void *data, size_t len) {
    struct nlattr *a = (struct nlattr *)(m->buf + m->len);
    a->nla_type = type;
    a->nla_len = NLA_HDRLEN + len;
    memcpy(m->buf + m->len + NLA_HDRLEN, data, len);
    m->len += NLA_ALIGN(NLA_HDRLEN + len);
}

static void attr_str(struct msg *m, uint16_t type, const char *s) { attr(m, type, s, strlen(s) + 1); }
static void attr_u32(struct msg *m, uint16_t type, uint32_t v) { attr(m, type, &v, 4); }
static void attr_be32(struct msg *m, uint16_t type, uint32_t v) { v = htonl(v); attr(m, type, &v, 4); }

static void nest(struct msg *m, uint16_t type) {
    m->nests[m->depth++] = m->len;
    attr(m, type | NLA_F_NESTED, NULL, 0);
}

static void end(struct msg *m) {
    size_t start = m->nests[--m->depth];
    ((struct nlattr *)(m->buf + start))->nla_len = m->len - start;
}

static int nl_open(int protocol) {
    int fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, protocol);
    if (fd < 0) die("socket(AF_NETLINK)");
    int one = 1;
    if (setsockopt(fd, SOL_NETLINK, NETLINK_EXT_ACK, &one, sizeof one))
        printf("(this kernel has no extended ACK: the kernel's reasons below will be missing)\n");
    return fd;
}

static char ext_ack[256];

/* Sends `m` with an ACK requested; returns 0 or the kernel's errno, with the
 * extended ACK's message (if any) in `ext_ack`. */
static int talk_raw(int fd, const void *buf, size_t len) {
    ext_ack[0] = 0;
    if (send(fd, buf, len, 0) < 0) return errno;
    char reply[16384];
    for (;;) {
        ssize_t n = recv(fd, reply, sizeof reply, 0);
        if (n < 0) return errno;
        for (struct nlmsghdr *h = (struct nlmsghdr *)reply; NLMSG_OK(h, (size_t)n);
             h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type != NLMSG_ERROR) continue;
            struct nlmsgerr *e = NLMSG_DATA(h);
            if (h->nlmsg_flags & NLM_F_ACK_TLVS) {
                /* with NETLINK_CAP_ACK unset, the request follows the error */
                size_t off = NLMSG_HDRLEN + sizeof *e - sizeof e->msg + e->msg.nlmsg_len;
                if (e->error == 0) off = NLMSG_HDRLEN + sizeof *e;
                for (struct nlattr *a = (struct nlattr *)((char *)h + NLMSG_ALIGN(off));
                     (char *)a + NLA_HDRLEN <= (char *)h + h->nlmsg_len && a->nla_len >= NLA_HDRLEN;
                     a = (struct nlattr *)((char *)a + NLA_ALIGN(a->nla_len))) {
                    if ((a->nla_type & NLA_TYPE_MASK) == NLMSGERR_ATTR_MSG)
                        snprintf(ext_ack, sizeof ext_ack, "%s", (char *)a + NLA_HDRLEN);
                }
            }
            return -e->error;
        }
    }
}

static int talk(int fd, struct msg *m) {
    hdr(m)->nlmsg_flags |= NLM_F_ACK;
    hdr(m)->nlmsg_len = m->len;
    return talk_raw(fd, m->buf, m->len);
}

static const char *say(int rc) {
    static char out[512];
    if (rc == 0) return "ok";
    snprintf(out, sizeof out, "%s%s%s%s", strerror(rc), ext_ack[0] ? " (the kernel says: " : "",
             ext_ack, ext_ack[0] ? ")" : "");
    return out;
}

/* RTM_NEWLINK for a veth `name` whose peer `peer` is born in the namespace
 * `netns_fd` (or here, for -1). */
static int make_veth(int fd, const char *name, const char *peer, int netns_fd) {
    struct ifinfomsg ifi = {.ifi_family = AF_UNSPEC};
    struct msg m;
    msg_init(&m, RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &ifi, sizeof ifi);
    attr_str(&m, IFLA_IFNAME, name);
    nest(&m, IFLA_LINKINFO);
    attr_str(&m, IFLA_INFO_KIND, "veth");
    nest(&m, IFLA_INFO_DATA);
    nest(&m, VETH_INFO_PEER);
    /* the peer's own ifinfomsg comes first, as a raw prefix */
    memcpy(m.buf + m.len, &ifi, sizeof ifi);
    m.len += NLMSG_ALIGN(sizeof ifi);
    attr_str(&m, IFLA_IFNAME, peer);
    if (netns_fd >= 0) attr_u32(&m, IFLA_NET_NS_FD, (uint32_t)netns_fd);
    end(&m);
    end(&m);
    end(&m);
    return talk(fd, &m);
}

/* RTM_GETLINK by name on `fd`: 0 if the link exists in that socket's
 * namespace, else the errno (ENODEV). */
static int link_exists(int fd, const char *name) {
    struct ifinfomsg ifi = {.ifi_family = AF_UNSPEC};
    struct msg m;
    msg_init(&m, RTM_GETLINK, 0, &ifi, sizeof ifi);
    attr_str(&m, IFLA_IFNAME, name);
    return talk(fd, &m);
}

static int write_file(const char *path, const char *content) {
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return errno;
    ssize_t n = write(fd, content, strlen(content));
    int rc = n < 0 ? errno : 0;
    close(fd);
    return rc;
}

static void map_root_to(uid_t uid, gid_t gid) {
    char line[64];
    if (write_file("/proc/self/setgroups", "deny")) die("setgroups");
    snprintf(line, sizeof line, "0 %u 1", gid);
    if (write_file("/proc/self/gid_map", line)) die("gid_map");
    snprintf(line, sizeof line, "0 %u 1", uid);
    if (write_file("/proc/self/uid_map", line)) die("uid_map");
}

/* ---- nf_tables: one batch (BEGIN, the messages, END) on a NETLINK_NETFILTER
 * socket. Every message is an nfgenmsg plus attributes. ---- */

struct batch {
    char buf[16384];
    size_t len;
};

static void batch_add(struct batch *b, uint16_t type, uint16_t flags, uint8_t family, struct msg *body) {
    struct nfgenmsg nfg = {.nfgen_family = family, .version = NFNETLINK_V0, .res_id = 0};
    struct msg m;
    msg_init(&m, type, flags, &nfg, sizeof nfg);
    if (body) {
        memcpy(m.buf + m.len, body->buf, body->len);
        m.len += body->len;
    }
    hdr(&m)->nlmsg_len = m.len;
    /* any number unique in the batch will do: its offset in the batch, +1 */
    hdr(&m)->nlmsg_seq = b->len + 1;
    memcpy(b->buf + b->len, m.buf, m.len);
    b->len += m.len;
}

static int batch_send(int fd, struct batch *b) {
    /* BEGIN/END carry res_id NFNL_SUBSYS_NFTABLES; the last message asks for an ACK */
    struct nfgenmsg nfg = {.nfgen_family = AF_UNSPEC, .version = NFNETLINK_V0,
                           .res_id = htons(NFNL_SUBSYS_NFTABLES)};
    struct batch out = {0};
    struct msg m;
    msg_init(&m, NFNL_MSG_BATCH_BEGIN, 0, &nfg, sizeof nfg);
    hdr(&m)->nlmsg_len = m.len;
    memcpy(out.buf, m.buf, m.len);
    out.len = m.len;
    memcpy(out.buf + out.len, b->buf, b->len);
    out.len += b->len;
    msg_init(&m, NFNL_MSG_BATCH_END, 0, &nfg, sizeof nfg);
    hdr(&m)->nlmsg_len = m.len;
    memcpy(out.buf + out.len, m.buf, m.len);
    out.len += m.len;
    /* errors always come back; only the batch's last message asks for an ACK,
     * so the first NLMSG_ERROR is either the first failure or that ACK */
    return talk_raw(fd, out.buf, out.len);
}

#define NFT(type) ((NFNL_SUBSYS_NFTABLES << 8) | (type))

/* An expression: { NFTA_LIST_ELEM { NFTA_EXPR_NAME name, NFTA_EXPR_DATA {...} } } */
static void expr_begin(struct msg *m, const char *name) {
    nest(m, NFTA_LIST_ELEM);
    attr_str(m, NFTA_EXPR_NAME, name);
    nest(m, NFTA_EXPR_DATA);
}
static void expr_end(struct msg *m) { end(m); end(m); }

/* A table `probe` in family `family`, with a nat chain at `hook` and one rule
 * built by `rule`; returns the batch's result. */
static int nft_try(int fd, uint8_t family, uint32_t hook, int32_t priority, const char *chain_type,
                   void (*rule)(struct msg *)) {
    struct batch b = {0};
    struct msg body;
    memset(&body, 0, sizeof body);
    attr_str(&body, NFTA_TABLE_NAME, "probe");
    batch_add(&b, NFT(NFT_MSG_NEWTABLE), NLM_F_CREATE, family, &body);

    memset(&body, 0, sizeof body);
    attr_str(&body, NFTA_CHAIN_TABLE, "probe");
    attr_str(&body, NFTA_CHAIN_NAME, "c");
    nest(&body, NFTA_CHAIN_HOOK);
    attr_be32(&body, NFTA_HOOK_HOOKNUM, hook);
    attr_be32(&body, NFTA_HOOK_PRIORITY, (uint32_t)priority);
    end(&body);
    attr_str(&body, NFTA_CHAIN_TYPE, chain_type);
    batch_add(&b, NFT(NFT_MSG_NEWCHAIN), NLM_F_CREATE, family, &body);

    if (rule) {
        memset(&body, 0, sizeof body);
        attr_str(&body, NFTA_RULE_TABLE, "probe");
        attr_str(&body, NFTA_RULE_CHAIN, "c");
        nest(&body, NFTA_RULE_EXPRESSIONS);
        rule(&body);
        end(&body);
        batch_add(&b, NFT(NFT_MSG_NEWRULE), NLM_F_CREATE | NLM_F_APPEND, family, &body);
    }

    memset(&body, 0, sizeof body);
    attr_str(&body, NFTA_TABLE_NAME, "probe");
    batch_add(&b, NFT(NFT_MSG_DELTABLE), NLM_F_ACK, family, &body);
    return batch_send(fd, &b);
}

/* masquerade */
static void rule_masq(struct msg *m) {
    expr_begin(m, "masq");
    expr_end(m);
}

/* fib daddr type local → reg 1; cmp reg 1 == RTN_LOCAL; accept */
static void rule_fib(struct msg *m) {
    expr_begin(m, "fib");
    attr_be32(m, NFTA_FIB_DREG, NFT_REG_1);
    attr_be32(m, NFTA_FIB_RESULT, NFT_FIB_RESULT_ADDRTYPE);
    attr_be32(m, NFTA_FIB_FLAGS, NFTA_FIB_F_DADDR);
    expr_end(m);
    expr_begin(m, "cmp");
    attr_be32(m, NFTA_CMP_SREG, NFT_REG_1);
    attr_be32(m, NFTA_CMP_OP, NFT_CMP_EQ);
    nest(m, NFTA_CMP_DATA);
    uint32_t local = RTN_LOCAL; /* host byte order: fib writes it that way */
    attr(m, NFTA_DATA_VALUE, &local, 4);
    end(m);
    expr_end(m);
    expr_begin(m, "immediate");
    attr_be32(m, NFTA_IMMEDIATE_DREG, NFT_REG_VERDICT);
    nest(m, NFTA_IMMEDIATE_DATA);
    nest(m, NFTA_DATA_VERDICT);
    attr_be32(m, NFTA_VERDICT_CODE, NF_ACCEPT);
    end(m);
    end(m);
    expr_end(m);
}

/* ct status & dnat != 0 (bitwise + cmp): the hairpin and 127/8 guard rules */
static void rule_ct_status(struct msg *m) {
    expr_begin(m, "ct");
    attr_be32(m, NFTA_CT_KEY, NFT_CT_STATUS);
    attr_be32(m, NFTA_CT_DREG, NFT_REG_1);
    expr_end(m);
    uint32_t mask = 0x20 /* IPS_DST_NAT */, zero = 0;
    expr_begin(m, "bitwise");
    attr_be32(m, NFTA_BITWISE_SREG, NFT_REG_1);
    attr_be32(m, NFTA_BITWISE_DREG, NFT_REG_1);
    attr_be32(m, NFTA_BITWISE_LEN, 4);
    nest(m, NFTA_BITWISE_MASK);
    attr(m, NFTA_DATA_VALUE, &mask, 4);
    end(m);
    nest(m, NFTA_BITWISE_XOR);
    attr(m, NFTA_DATA_VALUE, &zero, 4);
    end(m);
    expr_end(m);
    expr_begin(m, "cmp");
    attr_be32(m, NFTA_CMP_SREG, NFT_REG_1);
    attr_be32(m, NFTA_CMP_OP, NFT_CMP_NEQ);
    nest(m, NFTA_CMP_DATA);
    attr(m, NFTA_DATA_VALUE, &zero, 4);
    end(m);
    expr_end(m);
}

static double ms_since(struct timespec *t0) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (t.tv_sec - t0->tv_sec) * 1e3 + (t.tv_nsec - t0->tv_nsec) / 1e6;
}

/* Runs `fn` in a forked child; returns its exit status. */
static int in_child(void (*fn)(void)) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) die("fork");
    if (pid == 0) {
        fn();
        fflush(stdout);
        _exit(0);
    }
    int status;
    waitpid(pid, &status, 0);
    return status;
}

/* N1, N2: a user namespace (root mapped to nobody, as rootless) with a
 * network and mount namespace of its own. */
static void rootless_netns(void) {
    if (setgid(65534) || setuid(65534)) die("drop to nobody");
    /* changing credentials made the process non-dumpable, which makes its
     * /proc/self files root's: it couldn't write its own maps */
    if (prctl(PR_SET_DUMPABLE, 1)) die("PR_SET_DUMPABLE");
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET | CLONE_NEWNS)) {
        printf("N1 user+network namespace (as nobody): %s\n", strerror(errno));
        return;
    }
    map_root_to(65534, 65534);
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) die("make-rprivate");
    mkdir("/tmp/shipyard-probe-sys", 0755);
    int rc = mount("sysfs", "/tmp/shipyard-probe-sys", "sysfs", MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    printf("N1 sysfs mount in a user namespace owning its netns: %s\n", rc ? strerror(errno) : "ok");
    if (rc == 0) {
        char line[512] = "";
        FILE *ls = popen("ls /tmp/shipyard-probe-sys/class/net | tr '\\n' ' '", "r");
        if (ls && fgets(line, sizeof line, ls)) printf("   /sys/class/net lists: %s\n", line);
        if (ls) pclose(ls);
    }
    printf("N2 net.ipv4.ip_unprivileged_port_start=0 there: %s\n",
           say(write_file("/proc/sys/net/ipv4/ip_unprivileged_port_start", "0")));
    printf("N2 net.ipv4.ping_group_range=\"0 2147483647\" there: %s\n",
           say(write_file("/proc/sys/net/ipv4/ping_group_range", "0 2147483647")));
    /* the range must map into the namespace's owner: only root (0) is mapped here */
    printf("N2 net.ipv4.ping_group_range=\"0 0\" (only the mapped group) there: %s\n",
           say(write_file("/proc/sys/net/ipv4/ping_group_range", "0 0")));
    int fd = nl_open(NETLINK_ROUTE);
    struct ifinfomsg ifi = {.ifi_family = AF_UNSPEC, .ifi_index = 1, .ifi_flags = IFF_UP, .ifi_change = IFF_UP};
    struct msg m;
    msg_init(&m, RTM_NEWLINK, 0, &ifi, sizeof ifi);
    printf("N2 bringing lo up as the namespace's root: %s\n", say(talk(fd, &m)));
}

/* Unshares a new network namespace and returns a descriptor for it. */
static int new_netns(void) {
    if (unshare(CLONE_NEWNET)) die("unshare(NEWNET)");
    int fd = open("/proc/thread-self/ns/net", O_RDONLY | O_CLOEXEC);
    if (fd < 0) die("open ns/net");
    return fd;
}

/* N3, N4, N8, N9: as real root, in namespaces of the probe's own. */
static void veth_checks(void) {
    int a = new_netns(); /* "the host" for this check */
    int b = new_netns(); /* "the container" */
    if (setns(a, CLONE_NEWNET)) die("setns(a)");
    int host = nl_open(NETLINK_ROUTE);

    printf("N3 veth whose peer is born in another netns as eth0: %s\n",
           say(make_veth(host, "shy-probe0", "eth0", b)));
    printf("   eth0 seen from the host side: %s\n", say(link_exists(host, "eth0")));

    if (setns(b, CLONE_NEWNET)) die("setns(b)");
    int inside = nl_open(NETLINK_ROUTE);
    if (setns(a, CLONE_NEWNET)) die("setns(a) back");
    printf("N4 a socket made inside, used after setns back, sees eth0: %s\n",
           say(link_exists(inside, "eth0")));

    struct ifinfomsg ifi = {.ifi_family = AF_UNSPEC};
    struct msg m;
    msg_init(&m, RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &ifi, sizeof ifi);
    attr_str(&m, IFLA_IFNAME, "a-name-longer-than-15");
    nest(&m, IFLA_LINKINFO);
    attr_str(&m, IFLA_INFO_KIND, "bridge");
    end(&m);
    printf("N9 extended ACK for a 21-byte interface name: %s\n", say(talk(host, &m)));

    msg_init(&m, RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &ifi, sizeof ifi);
    attr_str(&m, IFLA_IFNAME, "shy-probe1");
    nest(&m, IFLA_LINKINFO);
    attr_str(&m, IFLA_INFO_KIND, "no-such-kind");
    end(&m);
    printf("N9 extended ACK for an unknown link kind: %s\n", say(talk(host, &m)));

    /* N8: a namespace whose last process exits, and its veth's host end */
    printf("N8 creating a veth into a namespace held only by a child: ");
    fflush(stdout);
    int sync[2];
    if (pipe(sync)) die("pipe");
    pid_t child = fork();
    if (child == 0) {
        close(sync[0]);
        if (unshare(CLONE_NEWNET)) die("child unshare");
        if (write(sync[1], "x", 1) != 1) die("child write");
        pause();
        _exit(0);
    }
    close(sync[1]);
    char x;
    if (read(sync[0], &x, 1) != 1) die("sync");
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/ns/net", child);
    int cns = open(path, O_RDONLY | O_CLOEXEC);
    if (cns < 0) die("child ns");
    int rc = make_veth(host, "shy-probe2", "eth0", cns);
    close(cns);
    printf("%s\n", say(rc));
    if (rc == 0) {
        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        kill(child, SIGKILL);
        waitpid(child, NULL, 0);
        double gone = -1;
        while (ms_since(&t0) < 5000) {
            if (link_exists(host, "shy-probe2") == ENODEV) {
                gone = ms_since(&t0);
                break;
            }
            usleep(1000);
        }
        if (gone < 0)
            printf("   the host end is still there 5 s after the namespace's last process died\n");
        else
            printf("   the host end was gone %.1f ms after the last process died\n", gone);
    }
}

/* N5, N6, N7: nf_tables and conntrack, in a namespace of the probe's own. */
static void netfilter_checks(void) {
    new_netns();
    int fd = nl_open(NETLINK_NETFILTER);
    printf("N5 inet nat chain (postrouting, srcnat) with masquerade: %s\n",
           say(nft_try(fd, NFPROTO_INET, NF_INET_POST_ROUTING, 100, "nat", rule_masq)));
    printf("N5 inet nat chain (prerouting, dstnat) with fib daddr type local: %s\n",
           say(nft_try(fd, NFPROTO_INET, NF_INET_PRE_ROUTING, -100, "nat", rule_fib)));
    printf("N5 inet filter chain with ct status dnat: %s\n",
           say(nft_try(fd, NFPROTO_INET, NF_INET_PRE_ROUTING, 0, "filter", rule_ct_status)));
    printf("N5 ip nat chain (for comparison): %s\n",
           say(nft_try(fd, NFPROTO_IPV4, NF_INET_POST_ROUTING, 100, "nat", rule_masq)));

    struct stat st;
    printf("N6 route_localnet per interface (conf/lo): %s\n",
           stat("/proc/sys/net/ipv4/conf/lo/route_localnet", &st) == 0 ? "present" : strerror(errno));

    int ct = nl_open(NETLINK_NETFILTER);
    struct nfgenmsg nfg = {.nfgen_family = AF_INET, .version = NFNETLINK_V0};
    struct msg m;
    msg_init(&m, (NFNL_SUBSYS_CTNETLINK << 8) | IPCTNL_MSG_CT_GET, NLM_F_DUMP, &nfg, sizeof nfg);
    hdr(&m)->nlmsg_len = m.len;
    int rc = 0;
    ext_ack[0] = 0;
    if (send(ct, m.buf, m.len, 0) < 0) rc = errno;
    else {
        char reply[16384];
        ssize_t n = recv(ct, reply, sizeof reply, 0);
        if (n < 0) rc = errno;
        else {
            struct nlmsghdr *h = (struct nlmsghdr *)reply;
            if (h->nlmsg_type == NLMSG_ERROR) rc = -((struct nlmsgerr *)NLMSG_DATA(h))->error;
        }
    }
    printf("N7 conntrack dump over ctnetlink: %s\n", say(rc));
}

int main(void) {
    struct utsname uts;
    uname(&uts);
    printf("kernel: %s\n", uts.release);
    if (getuid() != 0) {
        printf("run as root (in a privileged container): the checks make namespaces and links\n");
        return 2;
    }
    in_child(rootless_netns);
    in_child(veth_checks);
    in_child(netfilter_checks);
    return 0;
}
