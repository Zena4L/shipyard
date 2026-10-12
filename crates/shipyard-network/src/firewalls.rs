//! Other programs' firewalls that would drop what Shipyard's rules let
//! through (stage 7, completed in stage 8, step 8.6: finding F14).
//!
//! **Why another firewall matters.** `nf_tables` runs every table's base
//! chains at a hook (TLPI's network chapters stop before `nf_tables`; see
//! `nft(8)`). An `accept` in Shipyard's table ends only its own chain: the
//! packet still meets every other table's chain at that hook, and a drop or
//! reject in any of them is final. So a firewall installed beside Shipyard
//! can drop a container's traffic after Shipyard accepted it.
//!
//! **Shipyard never changes another program's firewall.** It reads them,
//! and says what would let its containers through, in that firewall's own
//! terms:
//!
//! - **firewalld** (its `inet firewalld` table) sends what comes from an
//!   interface in no zone to its default zone, which rejects what it doesn't
//!   allow: the DNS server's queries always, the way out where the zone
//!   doesn't forward to the host's way out. The fix: the bridge in its
//!   `trusted` zone.
//! - **Docker** sets its `FORWARD` chain's policy to drop and keeps the
//!   `DOCKER-USER` chain for the admin's own rules: three rules there, for
//!   traffic between the containers on the bridge (Docker loads
//!   `br_netfilter`, so bridged frames meet the IP hooks too), their way
//!   out, and replies and published ports (DNAT'd) coming in.
//! - **ufw** drops forwarded traffic by default and keeps its rules in
//!   chains of its own: the fix is `ufw route allow` (and `ufw allow` for
//!   the DNS server).
//! - **Any other chain dropping by policy, or ending in a rule that drops
//!   or rejects everything** (`iptables-services`' last `-j REJECT`) gets
//!   the same rules inserted there, as `iptables` or `nft` lines.
//!
//! A fix already applied is recognised: each rule it prints must be there,
//! in the right direction (a rule for a former way out, or one narrowed to a
//! port, doesn't count).

use std::net::IpAddr;

use crate::nft::consts::{
    NF_INET_FORWARD, NF_INET_LOCAL_IN, NFPROTO_INET, NFPROTO_IPV4, NFPROTO_IPV6,
};
use crate::nft::{Chain, FIREWALLD, Rule};

/// A network, as the assessment needs it.
#[derive(Debug, Clone, Copy)]
pub struct NetworkView<'a> {
    /// Its bridge.
    pub bridge: &'a str,
    /// The host's way out for IPv4 (its default route's interface), if it
    /// has one.
    pub uplink: Option<&'a str>,
    /// The host's way out for IPv6, if it has one.
    pub uplink6: Option<&'a str>,
    /// Whether it has a way out at all (not `--internal`).
    pub outward: bool,
    /// Whether it has IPv6 (its traffic meets the `ip6` tables too).
    pub ipv6: bool,
    /// Whether bridged traffic meets the IP hooks here (`br_netfilter`'s
    /// `bridge-nf-call-iptables`): then a chain dropping what's forwarded
    /// keeps the containers on the bridge from each other too.
    pub bridged: bool,
    /// Its DNS server's address (the gateway), if it has one.
    pub dns: Option<IpAddr>,
}

/// Whose firewall it is: that says how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Firewall {
    /// firewalld's zones.
    Firewalld,
    /// Docker's `FORWARD` chain; the fix goes in `DOCKER-USER`.
    Docker {
        /// Whether it's the IPv6 one (`ip6tables`', the `ip6 filter` table).
        ipv6: bool,
    },
    /// ufw's; the fix is ufw's own rules (they cover IPv4 and IPv6).
    Ufw,
    /// Another chain dropping or rejecting by default.
    Chain {
        /// The chain, as `nft` names it: its family, table and name.
        at: String,
    },
}

/// What a firewall would keep from a network's containers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four independent facts, each shown on its own"
)]
pub struct Blocks {
    /// Their way out to the world.
    pub way_out: bool,
    /// Their published ports, from elsewhere.
    pub ports: bool,
    /// Each other, on the bridge.
    pub each_other: bool,
    /// Their DNS server, on the gateway.
    pub dns: bool,
}

/// Another firewall that would drop a network's traffic, and how to let it
/// through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Whose.
    pub firewall: Firewall,
    /// The network's bridge.
    pub bridge: String,
    /// What it would keep from the containers.
    pub blocks: Blocks,
    /// The network's DNS server, when it keeps the containers from it.
    pub dns: Option<IpAddr>,
    /// Whether its fix is applied already: nothing to say then.
    pub fixed: bool,
    /// The commands that let the traffic through, in that firewall's terms,
    /// shell-quoted, to run in order.
    pub fix: Vec<String>,
}

/// Docker's chain for the admin's own rules.
const DOCKER_USER: &str = "DOCKER-USER";
/// ufw's chains for its user's rules: what's forwarded, what comes in.
const UFW_FORWARD: &str = "ufw-user-forward";
const UFW_INPUT: &str = "ufw-user-input";
/// ufw's own chains before them: empty while ufw is disabled (it leaves its
/// chains, flushed, behind).
const UFW_BEFORE_FORWARD: &str = "ufw-before-forward";
const UFW_BEFORE_INPUT: &str = "ufw-before-input";

/// What the kernel's firewall holds now, assessed for `view` (see
/// [`assess`]): every chain, and the rules of those that matter.
///
/// # Errors
///
/// The firewall couldn't be listed.
#[cfg(target_os = "linux")]
pub fn conflicts(
    nft: &mut crate::nft::Nftables,
    view: &NetworkView<'_>,
) -> Result<Vec<Conflict>, crate::netlink::NetlinkError> {
    let chains = nft.chains()?;
    let mut rules = Vec::new();
    for (family, table, chain) in to_read(&chains, view) {
        rules.extend(nft.rules(family, &table, chain.as_deref())?);
    }
    Ok(assess(&chains, &rules, view))
}

/// The rules [`assess`] reads, from `chains`: `(family, table, chain)`, a
/// whole table where `chain` is `None` (firewalld's). Only these: a host
/// running Kubernetes has tens of thousands of rules elsewhere.
#[must_use]
pub fn to_read(chains: &[Chain], view: &NetworkView<'_>) -> Vec<(u8, String, Option<String>)> {
    let mut read = Vec::new();
    if chains.iter().any(is_firewalld_hook) {
        read.push((NFPROTO_INET, FIREWALLD.to_owned(), None));
    }
    for chain in chains.iter().filter(|chain| considered(chain, view)) {
        read.push((chain.family, chain.table.clone(), Some(chain.name.clone())));
        for theirs in [
            DOCKER_USER,
            UFW_FORWARD,
            UFW_INPUT,
            UFW_BEFORE_FORWARD,
            UFW_BEFORE_INPUT,
        ] {
            if has_chain(chains, chain, theirs) {
                read.push((chain.family, chain.table.clone(), Some(theirs.to_owned())));
            }
        }
    }
    read.sort();
    read.dedup();
    read
}

/// The firewalls in `chains` (every chain listed) and `rules` (those
/// [`to_read`] names) that would drop `view`'s traffic, each with its fix
/// and whether it's applied.
#[must_use]
pub fn assess(chains: &[Chain], rules: &[Rule], view: &NetworkView<'_>) -> Vec<Conflict> {
    let mut found = Vec::new();
    if chains.iter().any(is_firewalld_hook) {
        found.extend(firewalld(rules, view));
    }
    for chain in chains.iter().filter(|chain| considered(chain, view)) {
        let last = rules
            .iter()
            .rfind(|rule| in_chain(rule, chain, &chain.name));
        let closed =
            chain.drops_forwarded() || chain.drops_input() || last.is_some_and(Rule::ends_all);
        if !closed {
            continue;
        }
        found.extend(match chain.hook {
            Some(NF_INET_FORWARD) => forward(chains, rules, chain, view),
            Some(NF_INET_LOCAL_IN) => Some(input(chains, rules, chain, view)),
            _ => None,
        });
    }
    found
}

/// Whether `chain` is a base chain of firewalld's table.
fn is_firewalld_hook(chain: &Chain) -> bool {
    chain.family == NFPROTO_INET && chain.table == FIREWALLD && chain.hook.is_some()
}

/// Whether `chain` is another program's base chain that could drop
/// `view`'s traffic: on the forward hook, or the input hook where the
/// network has a DNS server; in a family that sees routed IP traffic (a
/// `bridge` family's chains see frames, its hook numbers mean other
/// things); not Shipyard's or firewalld's.
fn considered(chain: &Chain, view: &NetworkView<'_>) -> bool {
    let hook = match chain.hook {
        Some(NF_INET_FORWARD) => chain.family != NFPROTO_IPV6 || view.ipv6,
        // The DNS server is on IPv4: an IPv6 chain (ufw makes one of
        // each) drops nothing it gets.
        Some(NF_INET_LOCAL_IN) => view.dns.is_some() && chain.family != NFPROTO_IPV6,
        _ => false,
    };
    hook && matches!(chain.family, NFPROTO_INET | NFPROTO_IPV4 | NFPROTO_IPV6)
        && !chain.is_shipyards()
        && !chain.is_firewallds()
}

/// Whether `chain`'s table has a chain named `name`.
fn has_chain(chains: &[Chain], chain: &Chain, name: &str) -> bool {
    chains.iter().any(|other| {
        other.family == chain.family && other.table == chain.table && other.name == name
    })
}

/// Whether ufw is on in `chain`'s table, with its chain for the user's
/// rules `user`: there, and its own chain before it holding rules (a
/// disabled ufw leaves its chains, flushed).
fn ufw_on(chains: &[Chain], rules: &[Rule], chain: &Chain, user: &str) -> bool {
    let before = if user == UFW_INPUT {
        UFW_BEFORE_INPUT
    } else {
        UFW_BEFORE_FORWARD
    };
    has_chain(chains, chain, user) && rules.iter().any(|rule| in_chain(rule, chain, before))
}

/// Whether `rule` is in the chain `name` of `chain`'s table.
fn in_chain(rule: &Rule, chain: &Chain, name: &str) -> bool {
    rule.family == chain.family && rule.table == chain.table && rule.chain == name
}

/// firewalld's zones, for `view`: what its default zone would reject (an
/// interface in no zone is in it), unless the bridge is in the trusted zone.
fn firewalld(rules: &[Rule], view: &NetworkView<'_>) -> Option<Conflict> {
    let bridge = view.bridge;
    let theirs = || {
        rules
            .iter()
            .filter(|rule| rule.family == NFPROTO_INET && rule.table == FIREWALLD)
    };
    // firewalld sends what comes from an interface of a zone to that zone's
    // chains (`…_trusted`, by a jump or goto, depending on its version);
    // a rule naming no interface sends everything there: the default zone
    // is trusted.
    let trusted = theirs().any(|rule| {
        rule.goes_to()
            .is_some_and(|chain| chain.ends_with("_trusted"))
            && match &rule.iifname {
                Some(_) => rule.from_interface(bridge),
                None => rule.oifname.is_none(),
            }
    });
    // Its forward chains accept what leaves by an interface of a zone that
    // forwards (`forward: yes`, firewalld 1.0's default): then the default
    // zone's containers get out.
    let forwards = |uplink: &str| {
        theirs().any(|rule| {
            rule.chain.starts_with("filter_F")
                && rule.accepts()
                && !rule.narrower
                && rule.oifname.is_some()
                && rule.to_interface(uplink)
                && rule.from_interface(bridge)
        })
    };
    let blocks = Blocks {
        way_out: view.outward
            && !trusted
            && uplinks(view, NFPROTO_INET)
                .into_iter()
                .any(|uplink| !forwards(uplink)),
        dns: view.dns.is_some(),
        ..Blocks::default()
    };
    (blocks != Blocks::default()).then(|| Conflict {
        firewall: Firewall::Firewalld,
        bridge: bridge.to_owned(),
        blocks,
        dns: view.dns,
        fixed: trusted,
        fix: vec![format!(
            "sudo firewall-cmd --permanent --zone=trusted --add-interface={} && sudo \
             firewall-cmd --reload",
            quoted(bridge)
        )],
    })
}

/// A chain dropping what's forwarded: the rules that let `view`'s traffic
/// through, in its firewall's terms.
fn forward(
    chains: &[Chain],
    rules: &[Rule],
    chain: &Chain,
    view: &NetworkView<'_>,
) -> Option<Conflict> {
    let blocks = Blocks {
        way_out: view.outward,
        ports: view.outward,
        each_other: view.bridged,
        dns: false,
    };
    if blocks == Blocks::default() {
        return None;
    }
    let ipv6 = chain.family == NFPROTO_IPV6;
    let tool = if ipv6 { "ip6tables" } else { "iptables" };
    let docker = Form::Iptables {
        tool,
        chain: DOCKER_USER,
    };
    // ufw first: with Docker too, Docker's chains fall through to ufw's,
    // whose rules then let the traffic through, and ufw keeps them across
    // reboots (Docker's `DOCKER-USER` rules count all the same).
    let (firewall, target, form, also) = if ufw_on(chains, rules, chain, UFW_FORWARD) {
        // ufw's rules are for both families: said once, for IPv4's.
        if ipv6 {
            return None;
        }
        let also = has_chain(chains, chain, DOCKER_USER).then_some((DOCKER_USER, docker));
        (Firewall::Ufw, UFW_FORWARD, Form::Ufw, also)
    } else if has_chain(chains, chain, DOCKER_USER) {
        (Firewall::Docker { ipv6 }, DOCKER_USER, docker, None)
    } else if iptables_table(chain) {
        let form = Form::Iptables {
            tool,
            chain: &chain.name,
        };
        let at = nft_name(chain);
        (Firewall::Chain { at }, chain.name.as_str(), form, None)
    } else {
        let at = nft_name(chain);
        let form = Form::Nft { at: at.clone() };
        (Firewall::Chain { at }, chain.name.as_str(), form, None)
    };
    // ufw's rules hold for both families, so both ways out.
    let family = if firewall == Firewall::Ufw {
        NFPROTO_INET
    } else {
        chain.family
    };
    let passes = passes(view, family);
    let fix = passes.iter().flat_map(|pass| form.lines(pass)).collect();
    let fixed = applied(rules, chain, target, &form, &passes)
        || also.is_some_and(|(target, form)| applied(rules, chain, target, &form, &passes));
    Some(Conflict {
        firewall,
        bridge: view.bridge.to_owned(),
        blocks,
        dns: None,
        fixed,
        fix,
    })
}

/// Whether each of `passes`, written as `form`, is in `chain`'s table's
/// chain `target`: as many rules letting it through as lines printed.
fn applied(rules: &[Rule], chain: &Chain, target: &str, form: &Form<'_>, passes: &[Pass]) -> bool {
    passes.iter().all(|pass| {
        let there = rules
            .iter()
            .filter(|rule| in_chain(rule, chain, target) && pass.let_through(rule))
            .count();
        there >= form.lines(pass).len()
    })
}

/// A chain dropping what comes in to the host: the rules that let the
/// containers' queries reach the network's DNS server.
fn input(chains: &[Chain], rules: &[Rule], chain: &Chain, view: &NetworkView<'_>) -> Conflict {
    let bridge = quoted(view.bridge);
    let (firewall, target, fix, needed) = if ufw_on(chains, rules, chain, UFW_INPUT) {
        // One command, two rules (TCP and UDP).
        let fix = vec![format!("sudo ufw allow in on {bridge} to any port 53")];
        (Firewall::Ufw, UFW_INPUT, fix, 2)
    } else if iptables_table(chain) {
        let name = quoted(&chain.name);
        let fix: Vec<String> = ["udp", "tcp"]
            .iter()
            .map(|protocol| {
                format!("sudo iptables -I {name} -i {bridge} -p {protocol} --dport 53 -j ACCEPT")
            })
            .collect();
        (
            Firewall::Chain {
                at: nft_name(chain),
            },
            chain.name.as_str(),
            fix,
            2,
        )
    } else {
        let at = nft_name(chain);
        let rule = format!(
            "insert rule {at} iifname \"{}\" meta l4proto {{ udp, tcp }} th dport 53 accept",
            view.bridge
        );
        let fix = vec![format!("sudo nft {}", quoted(&rule))];
        (Firewall::Chain { at }, chain.name.as_str(), fix, 1)
    };
    // Rules for the port are narrower than the bridge's alone: any from the
    // bridge counts (a rule for another port too, which this can't tell).
    let there = rules
        .iter()
        .filter(|rule| {
            in_chain(rule, chain, target)
                && rule.accepts()
                && !rule.ct
                && rule.iifname.is_some()
                && rule.from_interface(view.bridge)
        })
        .count();
    Conflict {
        firewall,
        bridge: view.bridge.to_owned(),
        blocks: Blocks {
            dns: true,
            ..Blocks::default()
        },
        dns: view.dns,
        fixed: there >= needed,
        fix,
    }
}

/// Whether `chain`'s table is one iptables makes (`ip filter`, `ip6
/// filter`): its fix is in iptables' terms, as its owner (Docker, ufw, a
/// distribution's service) adds rules.
fn iptables_table(chain: &Chain) -> bool {
    chain.table == "filter" && matches!(chain.family, NFPROTO_IPV4 | NFPROTO_IPV6)
}

/// `chain` as nft names it: family, table, name.
fn nft_name(chain: &Chain) -> String {
    format!("{} {} {}", chain.family_name(), chain.table, chain.name)
}

/// The ways out for `family`: IPv4's, IPv6's, or both (`inet`, and ufw's).
fn uplinks<'a>(view: &NetworkView<'a>, family: u8) -> Vec<&'a str> {
    let mut uplinks = Vec::new();
    if family != NFPROTO_IPV6 {
        uplinks.extend(view.uplink);
    }
    if (family == NFPROTO_IPV6 || (family == NFPROTO_INET && view.ipv6))
        && let Some(uplink6) = view.uplink6
        && !uplinks.contains(&uplink6)
    {
        uplinks.push(uplink6);
    }
    uplinks
}

/// A rule letting part of a network's forwarded traffic through (names as
/// they are, quoted when printed).
#[derive(Debug, Clone)]
struct Pass {
    /// From this interface; from any where `None`.
    input: Option<String>,
    /// To this one.
    output: String,
    /// Replies, and what's been DNAT'd to the containers (published ports).
    replies: bool,
}

impl Pass {
    /// Whether `rule` lets this pass's traffic through: it accepts it, for
    /// the same interfaces or more, with no other condition (a rule for one
    /// port doesn't), and without needing a connection's state, unless this
    /// is for replies.
    fn let_through(&self, rule: &Rule) -> bool {
        let input = match &self.input {
            Some(input) => rule.from_interface(input),
            None => rule.iifname.is_none(),
        };
        rule.accepts()
            && !rule.narrower
            && (self.replies || !rule.ct)
            && input
            && rule.to_interface(&self.output)
    }
}

/// What lets `view`'s forwarded traffic through a chain of `family` that
/// drops it: between its containers; out by the host's ways out (only
/// those: never into another program's bridges, Docker's, whose containers
/// its own rules keep apart); and replies and published ports coming in.
fn passes(view: &NetworkView<'_>, family: u8) -> Vec<Pass> {
    let bridge = view.bridge.to_owned();
    let mut passes = vec![Pass {
        input: Some(bridge.clone()),
        output: bridge.clone(),
        replies: false,
    }];
    if view.outward {
        // With no way out known, there's nothing to let out.
        for uplink in uplinks(view, family) {
            passes.push(Pass {
                input: Some(bridge.clone()),
                output: uplink.to_owned(),
                replies: false,
            });
        }
        passes.push(Pass {
            input: None,
            output: bridge,
            replies: true,
        });
    }
    passes
}

/// How a forwarding fix is written, for whose firewall.
#[derive(Debug)]
enum Form<'a> {
    /// `iptables` (or `ip6tables`) lines inserting in `chain`.
    Iptables { tool: &'a str, chain: &'a str },
    /// ufw's own commands.
    Ufw,
    /// `nft` lines inserting in the chain `at`.
    Nft { at: String },
}

impl Form<'_> {
    /// `pass`, written this way.
    fn lines(&self, pass: &Pass) -> Vec<String> {
        match self {
            Form::Iptables { tool, chain } => vec![iptables_line(tool, chain, pass)],
            Form::Ufw => vec![ufw_route(pass)],
            Form::Nft { at } => nft_forward_lines(at, pass),
        }
    }
}

/// `pass` as an `iptables` (`tool`) line inserting it in `chain`.
fn iptables_line(tool: &str, chain: &str, pass: &Pass) -> String {
    let input = pass
        .input
        .as_deref()
        .map_or_else(String::new, |input| format!(" -i {}", quoted(input)));
    let state = if pass.replies {
        " -m conntrack --ctstate RELATED,ESTABLISHED,DNAT"
    } else {
        ""
    };
    format!(
        "sudo {tool} -I {}{input} -o {}{state} -j ACCEPT",
        quoted(chain),
        quoted(&pass.output)
    )
}

/// `pass` as ufw's rule: ufw's own chains already let replies through, so
/// replies are only published ports, routed out to the bridge.
fn ufw_route(pass: &Pass) -> String {
    let input = pass
        .input
        .as_deref()
        .map_or_else(String::new, |input| format!(" in on {}", quoted(input)));
    format!(
        "sudo ufw route allow{input} out on {}",
        quoted(&pass.output)
    )
}

/// `pass` as `nft` lines inserting it in the chain `at`, each one shell
/// word (names in nft's quotes; two lines for replies: nft names a
/// connection's state and its DNAT apart).
fn nft_forward_lines(at: &str, pass: &Pass) -> Vec<String> {
    let input = pass
        .input
        .as_deref()
        .map_or_else(String::new, |input| format!(" iifname \"{input}\""));
    let head = format!("insert rule {at}{input} oifname \"{}\"", pass.output);
    let rules = if pass.replies {
        vec![
            format!("{head} ct state related,established accept"),
            format!("{head} ct status dnat accept"),
        ]
    } else {
        vec![format!("{head} accept")]
    };
    rules
        .iter()
        .map(|rule| format!("sudo nft {}", quoted(rule)))
        .collect()
}

/// `word` as one shell word: as it is if it's plain, else single-quoted.
/// (An interface's name comes from the kernel and may hold anything but
/// `/`, NUL and whitespace.)
fn quoted(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-@%:,".contains(&b));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::nft::Verdict;

    /// The input and forward hooks; drop and accept policies.
    const INPUT: Option<u32> = Some(NF_INET_LOCAL_IN);
    const FORWARD: Option<u32> = Some(NF_INET_FORWARD);
    const DROP: Option<u32> = Some(0);
    const ACCEPT: Option<u32> = Some(1);
    const INET: u8 = NFPROTO_INET;
    const IPV4: u8 = NFPROTO_IPV4;
    const IPV6: u8 = NFPROTO_IPV6;

    fn chain(family: u8, table: &str, name: &str, hook: Option<u32>, policy: Option<u32>) -> Chain {
        Chain {
            family,
            table: table.to_owned(),
            name: name.to_owned(),
            kind: hook.map(|_| "filter".to_owned()),
            hook,
            policy,
        }
    }

    /// An interface name as `iifname` compares it (NUL-padded).
    fn name(name: &str) -> Vec<u8> {
        let mut bytes = name.as_bytes().to_vec();
        bytes.resize(16, 0);
        bytes
    }

    /// A rule in `family`'s `table` `chain`, from `iif` to `oif`.
    fn rule(at: (u8, &str, &str), iif: Option<&str>, oif: Option<&str>, verdict: Verdict) -> Rule {
        Rule {
            family: at.0,
            table: at.1.to_owned(),
            chain: at.2.to_owned(),
            iifname: iif.map(name),
            oifname: oif.map(name),
            verdict: Some(verdict),
            ..Rule::default()
        }
    }

    fn view() -> NetworkView<'static> {
        NetworkView {
            bridge: "shipyard0",
            uplink: Some("eth0"),
            uplink6: None,
            outward: true,
            ipv6: false,
            bridged: true,
            dns: None,
        }
    }

    fn with_dns() -> NetworkView<'static> {
        NetworkView {
            dns: Some("10.89.0.1".parse().unwrap()),
            ..view()
        }
    }

    fn docker() -> Vec<Chain> {
        vec![
            chain(IPV4, "filter", "FORWARD", FORWARD, DROP),
            chain(IPV4, "filter", DOCKER_USER, None, None),
            chain(IPV6, "filter", "FORWARD", FORWARD, DROP),
            chain(IPV6, "filter", DOCKER_USER, None, None),
        ]
    }

    const DOCKER_FIX: [&str; 3] = [
        "sudo iptables -I DOCKER-USER -i shipyard0 -o shipyard0 -j ACCEPT",
        "sudo iptables -I DOCKER-USER -i shipyard0 -o eth0 -j ACCEPT",
        "sudo iptables -I DOCKER-USER -o shipyard0 -m conntrack --ctstate \
         RELATED,ESTABLISHED,DNAT -j ACCEPT",
    ];

    /// The rules [`DOCKER_FIX`] adds, as iptables-nft writes them.
    fn docker_fix_applied() -> Vec<Rule> {
        let at = (IPV4, "filter", DOCKER_USER);
        vec![
            rule(at, Some("shipyard0"), Some("shipyard0"), Verdict::Accept),
            rule(at, Some("shipyard0"), Some("eth0"), Verdict::Accept),
            Rule {
                ct: true,
                ..rule(at, None, Some("shipyard0"), Verdict::Accept)
            },
        ]
    }

    #[test]
    fn docker_gets_the_whole_fix_in_docker_user() {
        let found = assess(&docker(), &[], &view());
        assert_eq!(found.len(), 1, "IPv4 only, for an IPv4 network: {found:?}");
        assert_eq!(found[0].firewall, Firewall::Docker { ipv6: false });
        assert!(!found[0].fixed);
        assert_eq!(found[0].fix, DOCKER_FIX);
        assert_eq!(
            found[0].blocks,
            Blocks {
                way_out: true,
                ports: true,
                each_other: true,
                dns: false
            }
        );
        // Never `-i shipyard0` alone: that would open Docker's bridges.
        assert!(
            found[0]
                .fix
                .iter()
                .all(|line| !line.ends_with("-i shipyard0 -j ACCEPT"))
        );
        // IPv6's own, by its own way out.
        let ipv6 = NetworkView {
            ipv6: true,
            uplink6: Some("wg0"),
            ..view()
        };
        let found = assess(&docker(), &[], &ipv6);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].firewall, Firewall::Docker { ipv6: true });
        assert_eq!(
            found[1].fix[1],
            "sudo ip6tables -I DOCKER-USER -i shipyard0 -o wg0 -j ACCEPT"
        );
    }

    #[test]
    fn a_fix_applied_is_recognised_and_part_of_one_isnt() {
        let applied = docker_fix_applied();
        assert!(assess(&docker(), &applied, &view())[0].fixed);
        // Stage 7's two rules, twice over: still no same-bridge rule.
        let twice: Vec<Rule> = [&applied[1..], &applied[1..]].concat();
        assert!(!assess(&docker(), &twice, &view())[0].fixed);
        // A rule for a former way out doesn't count.
        let moved = NetworkView {
            uplink: Some("wlan0"),
            ..view()
        };
        assert!(!assess(&docker(), &applied, &moved)[0].fixed);
        // Nor rules for another bridge, nor narrowed to a port, nor that
        // drop.
        let changed = |change: fn(&mut Rule)| -> Vec<Rule> {
            let mut rules = applied.clone();
            rules.iter_mut().for_each(change);
            rules
        };
        for rules in [
            changed(|rule| rule.oifname = Some(name("docker0"))),
            changed(|rule| rule.narrower = true),
            changed(|rule| rule.verdict = Some(Verdict::Drop)),
        ] {
            assert!(!assess(&docker(), &rules, &view())[0].fixed, "{rules:?}");
        }
        // The way out needs no connection state: a rule that has one
        // doesn't let new connections out.
        let mut stateful = applied.clone();
        stateful[1].ct = true;
        assert!(!assess(&docker(), &stateful, &view())[0].fixed);
        // A wildcard covers the bridge (`-i shipyard+`).
        let mut wildcard = applied;
        wildcard[0].iifname = Some(b"shipyard".to_vec());
        assert!(assess(&docker(), &wildcard, &view())[0].fixed);
    }

    /// firewalld 2.4's table on Fedora 44 (read from a real one), for the
    /// bridge in no zone (`public` is the default, and forwards).
    fn fedora() -> (Vec<Chain>, Vec<Rule>) {
        let chains = vec![
            chain(INET, FIREWALLD, "filter_FORWARD", FORWARD, ACCEPT),
            chain(INET, FIREWALLD, "filter_INPUT", INPUT, ACCEPT),
            chain(INET, FIREWALLD, "filter_FORWARD_POLICIES", None, None),
            chain(INET, FIREWALLD, "filter_FWD_public_allow", None, None),
        ];
        let policies = (INET, FIREWALLD, "filter_FORWARD_POLICIES");
        let public = || Verdict::Jump("filter_FWD_public".to_owned());
        let reject = || Rule {
            rejects: true,
            ..rule(policies, None, None, Verdict::Other(-1))
        };
        let rules = vec![
            rule(policies, Some("eth0"), Some("eth0"), public()),
            rule(policies, Some("eth0"), None, public()),
            rule(policies, None, Some("eth0"), public()),
            rule(policies, None, None, public()),
            reject(),
            rule(
                (INET, FIREWALLD, "filter_FWD_public_allow"),
                None,
                Some("eth0"),
                Verdict::Accept,
            ),
        ];
        (chains, rules)
    }

    #[test]
    fn firewalld_forwarding_by_default_keeps_only_dns_from_the_containers() {
        let (chains, rules) = fedora();
        assert_eq!(
            to_read(&chains, &view()),
            [(INET, FIREWALLD.to_owned(), None)]
        );
        // The default network (no DNS server) gets out: nothing to say.
        assert_eq!(assess(&chains, &rules, &view()), []);
        // A network with a DNS server doesn't get answers.
        let found = assess(&chains, &rules, &with_dns());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].firewall, Firewall::Firewalld);
        assert_eq!(
            found[0].blocks,
            Blocks {
                dns: true,
                ..Blocks::default()
            }
        );
        assert_eq!(
            found[0].fix,
            [
                "sudo firewall-cmd --permanent --zone=trusted --add-interface=shipyard0 && sudo \
              firewall-cmd --reload"
            ]
        );
        // Where its zone doesn't forward (firewalld before 1.0, or `forward:
        // no`), no way out either.
        let shut: Vec<Rule> = rules[..5].to_vec();
        assert!(assess(&chains, &shut, &view())[0].blocks.way_out);
    }

    #[test]
    fn firewalld_trusts_the_bridge_once_its_zone_says_so() {
        let (chains, mut rules) = fedora();
        let policies = (INET, FIREWALLD, "filter_FORWARD_POLICIES");
        let trusted = || Verdict::Jump("filter_FWD_trusted".to_owned());
        // Another interface in the trusted zone, toward the bridge: not it.
        rules.insert(
            0,
            rule(policies, Some("eth0"), Some("shipyard0"), trusted()),
        );
        assert!(!assess(&chains, &rules, &with_dns())[0].fixed);
        // What `firewall-cmd --zone=trusted --add-interface=shipyard0` adds.
        rules.insert(
            0,
            rule(policies, Some("shipyard0"), Some("eth0"), trusted()),
        );
        assert!(assess(&chains, &rules, &with_dns())[0].fixed);
        // firewalld 0.9's way: a goto from its zone-dispatch chain.
        let (chains, mut rules) = fedora();
        rules.push(rule(
            (INET, FIREWALLD, "filter_FORWARD_IN_ZONES"),
            Some("shipyard0"),
            None,
            Verdict::Goto("filter_FWDI_trusted".to_owned()),
        ));
        assert!(assess(&chains, &rules, &with_dns())[0].fixed);
        // A trusted default zone: every interface is.
        let (chains, mut rules) = fedora();
        rules.push(rule(policies, None, None, trusted()));
        assert!(assess(&chains, &rules, &with_dns())[0].fixed);
    }

    #[test]
    fn ufw_gets_its_own_rules_for_both_families() {
        let chains = vec![
            chain(IPV4, "filter", "FORWARD", FORWARD, DROP),
            chain(IPV4, "filter", UFW_FORWARD, None, None),
            chain(IPV4, "filter", "INPUT", INPUT, DROP),
            chain(IPV4, "filter", UFW_INPUT, None, None),
            chain(IPV6, "filter", "FORWARD", FORWARD, DROP),
            chain(IPV6, "filter", UFW_FORWARD, None, None),
        ];
        let view = NetworkView {
            ipv6: true,
            uplink6: Some("wg0"),
            ..with_dns()
        };
        // ufw's own rules, as it has them while it's on.
        let on: Vec<Rule> = [
            (IPV4, UFW_BEFORE_FORWARD),
            (IPV4, UFW_BEFORE_INPUT),
            (IPV6, UFW_BEFORE_FORWARD),
        ]
        .iter()
        .map(|&(family, before)| Rule {
            ct: true,
            ..rule((family, "filter", before), None, None, Verdict::Accept)
        })
        .collect();
        // Off, it leaves its chains behind, flushed: not ufw's then.
        assert!(
            assess(&chains, &[], &view)
                .iter()
                .all(|found| found.firewall != Firewall::Ufw)
        );
        let found = assess(&chains, &on, &view);
        assert_eq!(found.len(), 2, "said once for both families: {found:?}");
        assert_eq!(found[0].firewall, Firewall::Ufw);
        assert_eq!(
            found[0].fix,
            [
                "sudo ufw route allow in on shipyard0 out on shipyard0",
                "sudo ufw route allow in on shipyard0 out on eth0",
                "sudo ufw route allow in on shipyard0 out on wg0",
                "sudo ufw route allow out on shipyard0",
            ]
        );
        assert_eq!(
            found[1].fix,
            ["sudo ufw allow in on shipyard0 to any port 53"]
        );
        // ufw's rules, as it writes them, in its own chains.
        let forward = (IPV4, "filter", UFW_FORWARD);
        let mut rules = on.clone();
        rules.extend([
            rule(
                forward,
                Some("shipyard0"),
                Some("shipyard0"),
                Verdict::Accept,
            ),
            rule(forward, Some("shipyard0"), Some("eth0"), Verdict::Accept),
            rule(forward, Some("shipyard0"), Some("wg0"), Verdict::Accept),
            rule(forward, None, Some("shipyard0"), Verdict::Accept),
        ]);
        for _ in ["tcp", "udp"] {
            rules.push(Rule {
                narrower: true,
                ..rule(
                    (IPV4, "filter", UFW_INPUT),
                    Some("shipyard0"),
                    None,
                    Verdict::Accept,
                )
            });
        }
        assert!(
            assess(&chains, &rules, &view)
                .iter()
                .all(|found| found.fixed)
        );
    }

    #[test]
    fn with_ufw_and_docker_ufws_rules_are_given_and_either_counts() {
        let chains = vec![
            chain(IPV4, "filter", "FORWARD", FORWARD, DROP),
            chain(IPV4, "filter", DOCKER_USER, None, None),
            chain(IPV4, "filter", UFW_FORWARD, None, None),
        ];
        let on = Rule {
            ct: true,
            ..rule(
                (IPV4, "filter", UFW_BEFORE_FORWARD),
                None,
                None,
                Verdict::Accept,
            )
        };
        let found = assess(&chains, std::slice::from_ref(&on), &view());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].firewall, Firewall::Ufw);
        assert!(!found[0].fixed);
        let mut docker = docker_fix_applied();
        docker.push(on);
        assert!(assess(&chains, &docker, &view())[0].fixed);
    }

    #[test]
    fn another_chain_gets_nft_lines_or_iptables_ones() {
        let chains = vec![
            chain(INET, "custom", "forward", FORWARD, DROP),
            chain(IPV4, "filter", "INPUT", INPUT, DROP),
            chain(IPV6, "filter", "INPUT", INPUT, DROP),
        ];
        let found = assess(&chains, &[], &with_dns());
        assert_eq!(found.len(), 2, "IPv4's input only: {found:?}");
        assert_eq!(
            found[0].firewall,
            Firewall::Chain {
                at: "inet custom forward".to_owned()
            }
        );
        assert_eq!(
            found[0].fix,
            [
                r#"sudo nft 'insert rule inet custom forward iifname "shipyard0" oifname "shipyard0" accept'"#,
                r#"sudo nft 'insert rule inet custom forward iifname "shipyard0" oifname "eth0" accept'"#,
                r#"sudo nft 'insert rule inet custom forward oifname "shipyard0" ct state related,established accept'"#,
                r#"sudo nft 'insert rule inet custom forward oifname "shipyard0" ct status dnat accept'"#,
            ]
        );
        assert_eq!(
            found[1].fix,
            [
                "sudo iptables -I INPUT -i shipyard0 -p udp --dport 53 -j ACCEPT",
                "sudo iptables -I INPUT -i shipyard0 -p tcp --dport 53 -j ACCEPT",
            ]
        );
        assert_eq!(found[1].dns, with_dns().dns);
    }

    #[test]
    fn a_chain_ending_in_a_reject_is_one_that_drops() {
        // iptables-services: an accept policy, then `-j REJECT` last.
        let chains = vec![chain(IPV4, "filter", "FORWARD", FORWARD, ACCEPT)];
        let at = (IPV4, "filter", "FORWARD");
        let reject = Rule {
            rejects: true,
            ..rule(at, None, None, Verdict::Other(-1))
        };
        let found = assess(&chains, std::slice::from_ref(&reject), &view());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].fix[0],
            "sudo iptables -I FORWARD -i shipyard0 -o shipyard0 -j ACCEPT"
        );
        // A reject for some traffic only isn't one for all.
        let some = Rule {
            iifname: Some(name("eth1")),
            ..reject
        };
        assert_eq!(assess(&chains, &[some], &view()), []);
    }

    #[test]
    fn only_what_matters_is_a_conflict() {
        let chains = [
            chain(INET, "shipyard", "forward", FORWARD, DROP),
            chain(INET, "shipyard", "input", INPUT, DROP),
            chain(INET, "filter", "forward", FORWARD, ACCEPT),
            // A chain on no hook drops nothing by policy.
            chain(IPV4, "filter", "custom", None, None),
            // A bridge family's: its hook 2 is the bridge's forward, and
            // routed traffic never meets it.
            chain(7, "filter", "FORWARD", FORWARD, DROP),
            // firewalld's, held while it reloads.
            chain(
                INET,
                "firewalld_policy_drop",
                "filter_FORWARD",
                FORWARD,
                DROP,
            ),
        ];
        assert_eq!(assess(&chains, &[], &with_dns()), []);
        assert_eq!(
            to_read(&chains, &with_dns()),
            [(INET, "filter".to_owned(), Some("forward".to_owned()))]
        );
        // An internal network on a host that doesn't filter bridged
        // traffic: a forward chain keeps nothing from it.
        let internal = NetworkView {
            outward: false,
            bridged: false,
            ..view()
        };
        assert_eq!(assess(&docker(), &[], &internal), []);
        let bridged = NetworkView {
            bridged: true,
            ..internal
        };
        assert_eq!(
            assess(&docker(), &[], &bridged)[0].fix,
            ["sudo iptables -I DOCKER-USER -i shipyard0 -o shipyard0 -j ACCEPT"]
        );
    }

    #[test]
    fn odd_names_are_quoted_for_the_shell() {
        assert_eq!(quoted("shipyard0"), "shipyard0");
        assert_eq!(quoted("br-x.1"), "br-x.1");
        assert_eq!(quoted("a;rm -rf"), "'a;rm -rf'");
        assert_eq!(quoted("it's"), r"'it'\''s'");
        assert_eq!(quoted("$(x)"), "'$(x)'");
        let odd = NetworkView {
            uplink: Some("up$(x)"),
            ..view()
        };
        let found = assess(&docker(), &[], &odd);
        assert!(
            found[0].fix[1].contains("-o 'up$(x)'"),
            "{:?}",
            found[0].fix
        );
    }
}
