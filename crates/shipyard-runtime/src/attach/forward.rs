//! A command forwarded to the macOS guest (step 10.3, D24): what the Mac
//! CLI asks the guest's agent to run, as [`Frame::HelloRun`]'s request.
//! Answered as an attach client is: [`Frame::Welcome`], the output, then
//! [`Frame::Exited`] (or [`Frame::Refused`]). Kept as the Mac's `shipyard`'s
//! way (D13-1): Docker's clients use the API.
//!
//! [`Frame::HelloRun`]: super::Frame::HelloRun
//! [`Frame::Welcome`]: super::Frame::Welcome
//! [`Frame::Exited`]: super::Frame::Exited
//! [`Frame::Refused`]: super::Frame::Refused

use serde::{Deserialize, Serialize};

use super::{Decoder, Frame, FrameError, VERSION};

mod datagrams;
pub use datagrams::{FRAME_WAIT, Inbox, OUTBOX_LIMIT, Outbox};

/// The request's format, written in it from the first (H2): a newer one is
/// refused, never guessed at.
pub const REQUEST_VERSION: u32 = 4;

/// The version a request is written at unless it carries a credential
/// ([`Request::registry_auth`], version 3) or names a place shared beyond
/// the home ([`BEYOND_HOME_VERSION`]): a guest of an older build reads
/// every other request as before, and refuses only those.
const BASE_VERSION: u32 = 2;

/// The version of a request that names a place the VM shares beyond the
/// home (`/tmp`, `/Volumes`: step 13.4): a guest from before shares only
/// the home, and would bind its own empty directory there without a word,
/// so it refuses the request instead, as another build's (stage 13's
/// close).
pub const BEYOND_HOME_VERSION: u32 = 4;

/// The descriptor a forwarded command finds its handed credential on
/// ([`Request::registry_auth`]: a pipe, read whole and closed first thing),
/// named in [`REGISTRY_AUTH_FD_ENV`] (step 12.4, D12.4-2).
pub const REGISTRY_AUTH_FD: i32 = 3;

/// The variable that says a command was handed a credential on
/// [`REGISTRY_AUTH_FD`]: its value is that number, never the credential.
pub const REGISTRY_AUTH_FD_ENV: &str = "SHIPYARD_REGISTRY_AUTH_FD";

/// A credential the Mac hands a forwarded `push` or `pull` for its image's
/// registry (Docker's `X-Registry-Auth` encoding: base64url of its auth
/// JSON). Its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RegistryAuth(pub String);

impl std::fmt::Debug for RegistryAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RegistryAuth(<redacted>)")
    }
}

/// The guest agent's vsock port.
pub const AGENT_PORT: u32 = 1024;

/// A request that isn't a command: the guest's stop, which the VM's host
/// asks of the agent (PID 1 then stops the containers and powers off).
pub const STOP_REQUEST: &str = "__guest-stop";

/// A request that isn't a command: the guest's boot ID, which the VM's host
/// asks every few seconds to tell a guest rebooted in place (a kernel panic:
/// the framework restarts it under the host) or hung from the one it booted
/// (step 13.2, F90). The agent answers it itself, before the guest is ready:
/// [`Frame::Welcome`], the ID in [`Frame::Stdout`], [`Frame::Exited`] 0. An
/// older guest runs it as a command it doesn't know, which fails.
///
/// [`Frame::Stdout`]: super::Frame::Stdout
pub const BOOT_REQUEST: &str = "__guest-boot";

/// The most a boot ID is read as (a UUID's 36 characters and a newline).
pub const MAX_BOOT_ANSWER: usize = 64;

/// The code a guest that can't read its own boot ID ends [`BOOT_REQUEST`]'s
/// answer with: alive, though not watched for a reboot (told from an older
/// guest's failure, the unknown command's).
pub const BOOT_UNREADABLE: i32 = 75;

/// A request that isn't a command: the ports the guest's active containers
/// publish, which the VM's host asks of the agent to listen on the Mac
/// (step 10.3c1). The agent runs it as any command (`shipyard
/// __guest-ports`, a process of its own): [`Frame::Welcome`], a
/// [`PortsAnswer`] in [`Frame::Stdout`] frames, then [`Frame::Exited`]: at
/// the version its argument names ([`PORTS_VERSION`], as this host asks:
/// v3; `2` is v2, without what comes back), else v1 (an older host's: TCP
/// listed, UDP counted): each host reads what it knows.
///
/// [`Frame::Stdout`]: super::Frame::Stdout
pub const PORTS_REQUEST: &str = "__guest-ports";

/// A request that isn't a command: the guest's own `doctor` checks (the
/// engine's, as on a Linux host), which a Mac's `doctor` shows (stage 10's
/// close). Run as any command: a `GuestDoctor` (the engine's) in
/// [`Frame::Stdout`] frames, then [`Frame::Exited`].
pub const DOCTOR_REQUEST: &str = "__guest-doctor";

/// The most the guest's checks are read as.
pub const MAX_DOCTOR_ANSWER: usize = 1024 * 1024;

/// A request that isn't a command: a connection to one of those ports
/// (`FORWARD_REQUEST IP PORT`, the port as the guest publishes it on `IP`).
/// Answered by [`Frame::Welcome`] (or
/// [`Frame::Refused`]), after which the connection carries the port's bytes
/// both ways, raw ([`Connection::into_parts`]).
pub const FORWARD_REQUEST: &str = "__guest-forward";

/// A request that isn't a command: the Mac's datagrams to a UDP port a
/// container publishes (`FORWARD_UDP_REQUEST IP PORT`, step 13.5a), one
/// stream for every Mac peer of it. Answered by [`Frame::Welcome`] (or
/// [`Frame::Refused`]), after which each datagram crosses either way as
/// [`write_datagram`] frames it, tagged with its Mac peer's ID (the Mac
/// gives each peer one; the guest answers on a socket of the ID's own).
/// Only ever sent to a guest whose ports answer listed the port as UDP
/// (v2): an older one has none.
pub const FORWARD_UDP_REQUEST: &str = "__guest-forward-udp";

/// The most a datagram carried by [`FORWARD_UDP_REQUEST`] holds: its
/// length's two bytes' most.
pub const MAX_DATAGRAM: usize = u16::MAX as usize;

/// Writes `datagram` of the Mac peer `peer` to `stream` as one frame: the
/// peer's ID (four bytes), its length (two), both big end first, then its
/// bytes. An empty one is a frame too (UDP carries them).
///
/// # Errors
///
/// [`std::io::ErrorKind::InvalidInput`] for one past [`MAX_DATAGRAM`]; the
/// stream's.
pub fn write_datagram(
    stream: &mut impl std::io::Write,
    peer: u32,
    datagram: &[u8],
) -> std::io::Result<()> {
    let length = u16::try_from(datagram.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "a datagram of {} bytes, past {MAX_DATAGRAM}",
                datagram.len()
            ),
        )
    })?;
    let mut frame = Vec::with_capacity(6 + datagram.len());
    frame.extend_from_slice(&peer.to_be_bytes());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(datagram);
    stream.write_all(&frame)
}

/// Reads one [`write_datagram`] frame from `stream` into `buffer` (filled to
/// the datagram's length): its peer's ID, or `None` at the stream's end
/// before a frame. It waits for the frame whole: the relays read through an
/// [`Inbox`], which never waits (13.8c); this is the reference `Inbox` is
/// fuzzed against (`udp_frames`), and a test's reader.
///
/// # Errors
///
/// [`std::io::ErrorKind::UnexpectedEof`] for a frame cut short; the
/// stream's (an interrupted read is tried again).
pub fn read_datagram(
    stream: &mut impl std::io::Read,
    buffer: &mut Vec<u8>,
) -> std::io::Result<Option<u32>> {
    let mut head = [0u8; 6];
    loop {
        match stream.read(&mut head[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    stream.read_exact(&mut head[1..])?;
    let [a, b, c, d, high, low] = head;
    buffer.resize(usize::from(u16::from_be_bytes([high, low])), 0);
    stream.read_exact(buffer)?;
    Ok(Some(u32::from_be_bytes([a, b, c, d])))
}

/// A request of the Mac's host itself, never relayed to the guest (step
/// 13.6b1, F93, F100): on the host's own socket (`host.sock`, bound from its
/// lock to its end), where it stands: its phase and what's left of that
/// phase's bound (`shipyard_vm::phase::PhaseAnswer`), as `Stdout`, then
/// `Exited(0)`.
pub const HOST_PHASE_REQUEST: &str = "__host-phase";

/// [`HOST_PHASE_REQUEST`]'s sibling: the host asked to stop in order (taken
/// once its guest is up, never cutting a boot's disk check; at once before
/// its guest has started), answered with its phase as it's asked. What `vm
/// stop` asks: no PID is signalled.
pub const HOST_STOP_REQUEST: &str = "__host-stop";

/// A request that isn't a command: the control of a forwarded command
/// (`CONTROL_REQUEST TOKEN`, step 13.6a, F94), the token its
/// [`Frame::Session`] gave, in hex: answered by [`Frame::Welcome`] (or
/// [`Frame::Refused`]), after which the client's [`Frame::Signal`] and
/// [`Frame::Resize`] frames go to the command at once, whatever its input
/// waits on. Its end is no hang-up (a client may give up on its control
/// and go on): a hang-up is `Signal(SIGHUP)` sent on it.
///
/// [`Frame::Session`]: super::Frame::Session
/// [`Frame::Signal`]: super::Frame::Signal
/// [`Frame::Resize`]: super::Frame::Resize
pub const CONTROL_REQUEST: &str = "__guest-control";

/// Whether a request's `args` are one of the host's own asks (every
/// `__guest-…` request: its ports, its checks, a connection), not a user's
/// command: never given a session token ([`Frame::Session`]),
/// whose answer is read whole. The agent and the Mac's client both go by it.
#[must_use]
pub fn is_internal(args: &[String]) -> bool {
    args.first()
        .is_some_and(|first| first.starts_with("__guest-"))
}

/// A session's token as a request names it ([`CONTROL_REQUEST`],
/// [`API_HANG_UP`]): 32 hex digits, so a port (5 decimal digits at most)
/// is never one.
#[must_use]
pub fn session_token_text(token: u128) -> String {
    format!("{token:032x}")
}

/// The token a request names ([`session_token_text`]), if it is one.
#[must_use]
pub fn session_token_of(text: &str) -> Option<u128> {
    (text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| u128::from_str_radix(text, 16).ok())
        .flatten()
}

/// A request that isn't a command: a connection to the guest's Docker API
/// (step 11.5b1), its service started if none answers. Answered by
/// [`Frame::Welcome`] (or [`Frame::Refused`]), after which the connection
/// carries the API's bytes both ways, raw, as a [`FORWARD_REQUEST`]'s.
pub const API_REQUEST: &str = "__guest-api";

/// What the host asks of the guest's agent once the client of one of its
/// [`API_REQUEST`] connections has gone (step 11.7): `API_HANG_UP NAME`,
/// the connection named by the token its welcome's
/// [`Frame::Session`]
/// gave ([`session_token_text`], F155: a port can be another's by then),
/// or, from an older host, by the host's vsock port (the guest's peer's). Its
/// API socket is then shut both ways, which the guest's service takes for
/// the client's hang-up: Virtualization.framework passes the host's close
/// of a vsock on as a half-close only, which says less. Answered with no
/// frame; an older guest runs it as an unknown command, and fails.
pub const API_HANG_UP: &str = "__guest-api-hang-up";

/// How long an API connection waits for its second end
/// (`shipyard_sys::relay::both_ways_lingering`, the client its `one`).
/// After the client's (its input ended: `echo x | docker run -i …`): as
/// long as it takes, as the service ends each connection itself and a cut
/// would lose a container's output. After the service's (it closes a
/// pooled client's idle connection): a minute, then cut, as a client that
/// keeps a dead pooled connection would keep the VM up for good.
pub const API_LINGERS: shipyard_sys::relay::Lingers = shipyard_sys::relay::Lingers {
    after_one: std::time::Duration::MAX,
    after_other: std::time::Duration::from_secs(60),
};

/// What the VM's host refuses a client with while it's stopping (idle,
/// step 10.3c2): the client's command never reached the guest, and its
/// CLI tries again once the VM has stopped (starting it again).
pub const STOPPING: &str = "the VM is stopping";

/// What the guest's agent says, refusing a request, while the guest isn't
/// ready for commands (step 10.4, D10.4-4: its data disk still checked or
/// repaired): the Mac's client waits and asks again.
pub const NOT_READY: &str = "the VM is still starting: it's checking its data disk, or \
                             repairing it after an unclean stop (its console log says how far)";

/// What the guest's agent says, refusing a request newer than it reads
/// (F167): it's another build's than the client's, started before an
/// upgrade.
pub const ANOTHER_BUILD: &str = "the running VM is another build's than this shipyard (an older \
                                 one, started before an upgrade): `shipyard vm stop`, and it \
                                 starts again as this one's";

/// How an older guest refused a newer request, in its words (before
/// [`ANOTHER_BUILD`]): [`RequestError::Newer`]'s, its start and its middle.
const OLDER_GUESTS_NEWER: (&str, &str) = ("a request of version ", ", newer than this guest's ");

/// Why the VM's host or guest refused a client, as the client acts on
/// it (step 11.5b2b, F99): the meaning of a [`Decline`](super::Decline)
/// code (the wire's number), from a [`Frame::Declined`]'s code, or, from an
/// older peer's [`Frame::Refused`], by its words ([`STOPPING`],
/// [`NOT_READY`], [`ANOTHER_BUILD`], an older guest's newer-request words:
/// the only place they're compared).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The VM is stopping.
    Stopping,
    /// The guest is still starting.
    NotReady,
    /// The guest is another build's: an older one, which can't read the
    /// request (F167).
    Newer,
    /// Anything else, in words.
    Other(String),
}

impl Refusal {
    /// The refusal `frame` says, if it's one.
    #[must_use]
    pub fn of(frame: &Frame) -> Option<Self> {
        match frame {
            Frame::Declined { reason, why } => Some(Self::from_decline(*reason, why)),
            Frame::Refused(why) => Some(Self::from_words(why)),
            _ => None,
        }
    }

    /// A [`Frame::Declined`]'s: by its code, its words kept for one this
    /// side doesn't know.
    #[must_use]
    pub fn from_decline(reason: super::Decline, why: &str) -> Self {
        match reason {
            super::Decline::Stopping => Self::Stopping,
            super::Decline::NotReady => Self::NotReady,
            super::Decline::Newer => Self::Newer,
            super::Decline::Other(_) => Self::Other(why.to_owned()),
        }
    }

    /// An older peer's refusal by its words. Not ready is known by its
    /// start: older guests added "; try again shortly".
    #[must_use]
    pub fn from_words(why: &str) -> Self {
        if why == STOPPING {
            Self::Stopping
        } else if why.starts_with(NOT_READY) {
            Self::NotReady
        } else if why.starts_with(ANOTHER_BUILD)
            || (why.starts_with(OLDER_GUESTS_NEWER.0) && why.contains(OLDER_GUESTS_NEWER.1))
        {
            Self::Newer
        } else {
            Self::Other(why.to_owned())
        }
    }

    /// The frame that says it to a client of protocol version `client`:
    /// [`Frame::Declined`] from [`DECLINE_VERSION`](super::DECLINE_VERSION),
    /// else [`Frame::Refused`]'s words.
    #[must_use]
    pub fn frame(&self, client: u32) -> Frame {
        let why = self.to_string();
        let reason = match self {
            Self::Stopping => super::Decline::Stopping,
            Self::NotReady => super::Decline::NotReady,
            Self::Newer => super::Decline::Newer,
            Self::Other(_) => return Frame::Refused(why),
        };
        if client >= super::DECLINE_VERSION {
            Frame::Declined { reason, why }
        } else {
            Frame::Refused(why)
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stopping => STOPPING,
            Self::NotReady => NOT_READY,
            Self::Newer => ANOTHER_BUILD,
            Self::Other(why) => why,
        })
    }
}

/// The ports answer's format, written in it from the first (H2): v2 (step
/// 13.5a) lists UDP ports with their protocol; v1 counted them; v3 (13.6b2)
/// says how many containers would come back after a restart.
pub const PORTS_VERSION: u32 = PORTS_COMES_BACK;

/// The first ports answer's version that says how many containers would
/// come back after a restart ([`PortsAnswer::comes_back`]).
pub const PORTS_COMES_BACK: u32 = 3;

/// The first ports answer's version that lists UDP ports (and whose guest
/// dials a forwarded port's container itself).
pub const PORTS_LIST_UDP: u32 = 2;

/// How long a forwarded UDP port's Mac peer is remembered, on either side,
/// without a datagram either way (as a NAT forgets one).
pub const UDP_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// The most Mac peers one forwarded UDP port has at once, on either side.
pub const MAX_UDP_PEERS: usize = 256;

/// What [`PORTS_REQUEST`] answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortsAnswer {
    /// [`PORTS_VERSION`] when written.
    pub version: u32,
    /// How many containers are active (running, paused, stopping, waiting
    /// to restart): what keeps the VM from being idle.
    pub active: u32,
    /// The ports they publish (TCP only, from a v1 guest).
    pub ports: Vec<PublishedPort>,
    /// How many UDP ports a v1 guest's containers publish, which it didn't
    /// list (not forwarded: said). 0 from a v2 guest: it lists them. Always
    /// written: a v1 reader (an older host) requires it.
    #[serde(default)]
    pub udp: u32,
    /// How many containers a restart of the guest would start (their restart
    /// policies'), from a v3 guest; never written to an older reader, which
    /// would refuse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comes_back: Option<u32>,
}

/// A port's protocol.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// TCP (a v1 answer's every port).
    #[default]
    Tcp,
    /// UDP (step 13.5a).
    Udp,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        })
    }
}

/// A port published in the guest, on `ip` (every address, if unspecified).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedPort {
    /// The address it's published on in the guest.
    pub ip: std::net::IpAddr,
    /// The port.
    pub port: u16,
    /// Its protocol (TCP when unsaid: not written for TCP, so a v1 answer
    /// reads as an older host reads it, `deny_unknown_fields`).
    #[serde(default, skip_serializing_if = "Protocol::is_tcp")]
    pub protocol: Protocol,
}

impl Protocol {
    /// Whether it's TCP.
    #[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
    #[must_use]
    pub fn is_tcp(&self) -> bool {
        *self == Self::Tcp
    }
}

/// The most a [`PortsAnswer`] is read as: every port of a 65,535-port
/// range, on two addresses, fits.
pub const MAX_PORTS_ANSWER: usize = 8 * 1024 * 1024;

impl Default for PortsAnswer {
    fn default() -> Self {
        Self::new()
    }
}

impl PortsAnswer {
    /// This answer as v1 says it, for a host that asks for no version (an
    /// older one): its TCP ports, its UDP ones counted.
    #[must_use]
    pub fn as_v1(&self) -> Self {
        let (tcp, udp): (Vec<PublishedPort>, Vec<PublishedPort>) = self
            .ports
            .iter()
            .partition(|port| port.protocol == Protocol::Tcp);
        Self {
            version: 1,
            active: self.active,
            ports: tcp,
            udp: self
                .udp
                .saturating_add(u32::try_from(udp.len()).unwrap_or(u32::MAX)),
            comes_back: None,
        }
    }

    /// This answer as v2 says it, for a host that asks for v2 (13.5a's to
    /// 13.6b1's): all but what comes back.
    #[must_use]
    pub fn as_v2(&self) -> Self {
        Self {
            version: PORTS_LIST_UDP,
            comes_back: None,
            ..self.clone()
        }
    }

    /// An answer of this version: none active, nothing published.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: PORTS_VERSION,
            active: 0,
            ports: Vec::new(),
            udp: 0,
            comes_back: Some(0),
        }
    }

    /// The answer, as its output carries it (in as many frames as it takes:
    /// a wide `-p` range's is long).
    ///
    /// # Errors
    ///
    /// [`RequestError::Unreadable`] if it can't be written (it always can).
    pub fn encode(&self) -> Result<Vec<u8>, RequestError> {
        serde_json::to_vec(self).map_err(|error| RequestError::Unreadable(error.to_string()))
    }

    /// An answer read back.
    ///
    /// # Errors
    ///
    /// [`RequestError::Newer`] for a newer version, else
    /// [`RequestError::Unreadable`].
    pub fn decode(bytes: &[u8]) -> Result<Self, RequestError> {
        shipyard_sys::versioned::read(bytes, PORTS_VERSION).map_err(RequestError::from)
    }
}

/// The descriptor the macOS guest's PID 1 hands its agent the agent's
/// listening vsock socket at (step 13.5b, F108): bound by PID 1 before
/// anything else runs and kept for the guest's life, so its port is never
/// free for a container to take while the agent restarts.
pub const AGENT_FD: i32 = 3;

/// The macOS guest's link to the Mac's NAT (Apple's DHCP gives it its
/// address): what no published port is reached from (step 13.5b, F108: only
/// the Mac reaches them, over vsock, never another VM on that network).
pub const GUEST_LINK: &str = "eth0";

/// The virtiofs tag the Mac's home is shared by.
pub const HOME_TAG: &str = "home";

/// The places beyond the home a Mac shares with its VM (step 13.4, D13.4-1:
/// Docker Desktop's set), at their real paths: its volumes (external
/// drives, disk images: a share reaches those mounted under it, later ones
/// too) and its temporary places (`/tmp`, the users' `$TMPDIR`s).
pub const MAC_SHARES: [&str; 3] = ["/Volumes", "/private/tmp", "/private/var/folders"];

/// The folders in a Mac's home that macOS guards (its privacy
/// permissions), each with the name macOS gives it: a program that launchd
/// started reads them only once the user allows it.
pub const GUARDED_FOLDERS: [(&str, &str); 4] = [
    ("Desktop", "Desktop"),
    ("Documents", "Documents"),
    ("Downloads", "Downloads"),
    ("Library/Mobile Documents", "iCloud Drive"),
];

/// macOS's own links to shared places (`/tmp` is `/private/tmp` there): the
/// guest binds each share at its link's path too, so a path means the same
/// whether the Mac's CLI or a Docker client names it.
pub const MAC_ALIASES: [(&str, &str); 2] = [
    ("/tmp", "/private/tmp"),
    ("/var/folders", "/private/var/folders"),
];

/// The kernel command line's key for the places shared beyond the home,
/// in [`places_value`]'s form: the `n`th by the tag [`share_tag`]`(n)`.
pub const SHARES_KEY: &str = "shipyard.shares=";

/// The virtiofs tag of the `n`th place shared beyond the home.
#[must_use]
pub fn share_tag(n: usize) -> String {
    format!("share{n}")
}

/// Whether `tag` is one a place of the Mac's is shared by (the home's or
/// another's), as a mount's source names it.
#[must_use]
pub fn is_mac_share_tag(tag: &str) -> bool {
    tag == HOME_TAG
        || tag
            .strip_prefix("share")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The tag of Rosetta's share (step 13.3): the Mac's Rosetta for Linux,
/// which the guest registers with `binfmt_misc` to run amd64 programs.
pub const ROSETTA_TAG: &str = "rosetta";

/// The kernel command line's word saying the guest has Rosetta's share
/// (the Mac has Rosetta installed).
pub const ROSETTA_WORD: &str = "shipyard.rosetta=1";

/// The kernel command line's key for the home's path in the guest.
pub const HOME_KEY: &str = "shipyard.home=";

/// The kernel command line's key for places in the shared home the guest
/// hides from what runs in it (the VM's own disk and boot files: a
/// container given the home mustn't rewrite them, stage 10's close): each
/// path in hex (the line can't carry a space), comma-separated.
pub const HIDE_KEY: &str = "shipyard.hide=";

/// `paths` as a value of [`HIDE_KEY`] or [`SHARES_KEY`].
#[must_use]
pub fn places_value(paths: &[&str]) -> String {
    paths
        .iter()
        .map(|path| shipyard_sys::hex::lower(path.as_bytes()))
        .collect::<Vec<_>>()
        .join(",")
}

/// The paths a kernel command line's `key` ([`HIDE_KEY`], [`SHARES_KEY`])
/// names, in order (those it can't read, left out).
#[must_use]
pub fn places_of(cmdline: &str, key: &str) -> Vec<String> {
    numbered_places_of(cmdline, key)
        .into_iter()
        .map(|(_, place)| place)
        .collect()
}

/// [`places_of`], each with its place in the line's list (counted before
/// any is left out): the `n` of [`share_tag`]`(n)`.
#[must_use]
pub fn numbered_places_of(cmdline: &str, key: &str) -> Vec<(usize, String)> {
    let Some(value) = cmdline
        .split_ascii_whitespace()
        .find_map(|word| word.strip_prefix(key))
    else {
        return Vec::new();
    };
    value
        .split(',')
        .enumerate()
        .filter_map(|(n, hex)| {
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            Some((n, String::from_utf8(bytes).ok()?))
        })
        .filter(|(_, path)| !path.is_empty())
        .collect()
}

/// The caller's variables a command may get: how its terminal behaves, and
/// its language. Nothing else (a store's place, a credential) is sent: the
/// Mac filters with it, and the guest again.
pub const PASSED_ENV: &[&str] = &[
    "TERM",
    "COLORTERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "NO_COLOR",
];

/// Whether `name` is one a command may not be given from the caller's
/// shell (a bare `-e KEY`): one the guest's own `shipyard` reads (its
/// `PATH`, `HOME`, `TMPDIR`, its stores, its log level) or the loader's. The
/// Mac refuses it, and the guest again.
#[must_use]
pub fn reserved(name: &str) -> bool {
    matches!(name, "PATH" | "HOME" | "TMPDIR" | "RUST_LOG" | "USER")
        || ["SHIPYARD_", "XDG_", "LD_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// The variable a forwarded command's `shipyard` in the guest finds the
/// caller's named variables in (a bare `-e KEY`'s, [`Request::named_env`]),
/// as a JSON object: never its own environment, which would change what
/// that `shipyard` does (`HTTPS_PROXY` its pulls, `SSL_CERT_FILE` its
/// trust), where Docker's CLI resolves them on the client.
pub const CALLER_ENV: &str = "SHIPYARD_CALLER_ENV";

/// `named` as [`CALLER_ENV`] carries them.
#[must_use]
pub fn caller_env_value(named: &[(String, String)]) -> String {
    let object: serde_json::Map<String, serde_json::Value> = named
        .iter()
        .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
        .collect();
    serde_json::Value::Object(object).to_string()
}

/// The caller's value of `name` in a [`CALLER_ENV`] `value`: `None` if it
/// has none (or the value isn't one).
#[must_use]
pub fn caller_var(value: &str, name: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(value)
        .ok()?
        .get(name)?
    {
        serde_json::Value::String(found) => Some(found.clone()),
        _ => None,
    }
}

/// Of `vars`, those a command gets ([`PASSED_ENV`]), as text (a variable
/// that isn't UTF-8 is left out).
pub fn passed_env<I, K, V>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<std::ffi::OsStr>,
    V: AsRef<std::ffi::OsStr>,
{
    vars.into_iter()
        .filter_map(|(name, value)| {
            let name = name.as_ref().to_str()?;
            let value = value.as_ref().to_str()?;
            PASSED_ENV
                .contains(&name)
                .then(|| (name.to_owned(), value.to_owned()))
        })
        .collect()
}

/// A forwarded command's connection, either end: whole frames sent and
/// read, blocking.
#[derive(Debug)]
pub struct Connection<S> {
    stream: S,
    decoder: Decoder,
}

/// Why a connection's frame can't be sent or read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectionError {
    /// The stream failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The other end sent something that isn't a frame.
    #[error(transparent)]
    Frame(#[from] FrameError),
}

impl<S: std::io::Read + std::io::Write> Connection<S> {
    /// A connection over `stream`.
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            decoder: Decoder::new(),
        }
    }

    /// Sends `frame`.
    ///
    /// # Errors
    ///
    /// The stream's.
    pub fn send(&mut self, frame: &Frame) -> Result<(), ConnectionError> {
        send(&mut self.stream, frame)
    }

    /// The next whole frame: `None` once the other end has closed.
    ///
    /// # Errors
    ///
    /// The stream's, or a frame that can't be read.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ConnectionError> {
        let mut buffer = [0; 16 * 1024];
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            match self.stream.read(&mut buffer) {
                Ok(0) => return Ok(None),
                Ok(read) => self.decoder.push(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// The stream itself (another handle on it, to write from elsewhere).
    pub fn stream(&self) -> &S {
        &self.stream
    }

    /// The stream, and the bytes read from it past the last frame taken:
    /// for a connection that carries raw bytes after its frames (a
    /// forwarded port's, step 10.3c1), whose first ones the decoder may
    /// have read already.
    pub fn into_parts(self) -> (S, Vec<u8>) {
        let unread = self.decoder.unread().to_vec();
        (self.stream, unread)
    }
}

/// Sends `frame` on `stream`, whole.
///
/// # Errors
///
/// The stream's.
pub fn send(stream: &mut impl std::io::Write, frame: &Frame) -> Result<(), ConnectionError> {
    let mut bytes = Vec::new();
    frame.encode(&mut bytes);
    stream.write_all(&bytes)?;
    Ok(())
}

/// The hello that asks for `request`: [`Frame::HelloRun`] at this
/// protocol's version.
///
/// # Errors
///
/// [`RequestError::TooLong`].
pub fn hello(request: &Request) -> Result<Frame, RequestError> {
    Ok(Frame::HelloRun {
        version: VERSION,
        request: request.encode()?,
    })
}

/// This machine's clock, as [`Request::time`] carries it: `None` for one
/// set before the epoch (the guest never follows that).
#[must_use]
pub fn clock() -> Option<u64> {
    Some(shipyard_sys::time::unix_seconds()).filter(|&seconds| seconds > 0)
}

/// The most an encoded request may be: what one frame carries, less the
/// hello's version.
pub const MAX_REQUEST: usize = super::MAX_PAYLOAD - 4;

/// What to run in the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// [`REQUEST_VERSION`] when written.
    pub version: u32,
    /// The command line, without the program: `shipyard`'s arguments.
    pub args: Vec<String>,
    /// Where it runs: the caller's directory, at the same path in the
    /// guest (the home is shared there), or `/`.
    pub cwd: String,
    /// The environment it gets, beyond the guest's own: a few of the
    /// caller's (`TERM`), never a store's.
    pub env: Vec<(String, String)>,
    /// Variables the command names to be copied from the caller's shell
    /// (a bare `-e KEY`), with their values there (version 2).
    #[serde(default)]
    pub named_env: Vec<(String, String)>,
    /// Whether the command reads its input (`run -i`, `attach`, `cp -`):
    /// without, its stdin is empty, and the caller's isn't read
    /// (version 2).
    #[serde(default)]
    pub input: bool,
    /// The caller's terminal's size, if the command gets a terminal in the
    /// guest (version 2; step 10.3b2).
    #[serde(default)]
    pub terminal: Option<crate::terminal::Size>,
    /// The caller's clock (seconds since the epoch), for the guest's to
    /// follow after the Mac slept (version 2).
    #[serde(default)]
    pub time: Option<u64>,
    /// The Mac's credential for a `push`'s or `pull`'s registry, handed to
    /// that command alone (version 3, step 12.4): never stored in the
    /// guest, never in any process's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_auth: Option<RegistryAuth>,
}

impl From<shipyard_sys::versioned::Error> for RequestError {
    fn from(error: shipyard_sys::versioned::Error) -> Self {
        match error {
            shipyard_sys::versioned::Error::Newer(version) => Self::Newer(version),
            shipyard_sys::versioned::Error::Unreadable(error) => {
                Self::Unreadable(error.to_string())
            }
        }
    }
}

/// Why a request can't be made or read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RequestError {
    /// Longer, encoded, than [`MAX_REQUEST`].
    #[error("the command is too long to forward ({0} bytes, at most {MAX_REQUEST})")]
    TooLong(usize),
    /// Not a request this version reads.
    #[error("not a request this guest reads: {0}")]
    Unreadable(String),
    /// A version newer than [`REQUEST_VERSION`].
    #[error("a request of version {0}, newer than this guest's {REQUEST_VERSION}")]
    Newer(u32),
}

impl Request {
    /// `args`, to run in `cwd` with `env`.
    #[must_use]
    pub fn new(args: Vec<String>, cwd: String, env: Vec<(String, String)>) -> Self {
        Self {
            version: BASE_VERSION,
            args,
            cwd,
            env,
            named_env: Vec::new(),
            input: false,
            terminal: None,
            time: None,
            registry_auth: None,
        }
    }

    /// The same, handing the command `auth` (written at version 3, which a
    /// guest of an older build refuses, saying so: never a push or pull
    /// that goes on anonymously by surprise).
    #[must_use]
    pub fn with_registry_auth(mut self, auth: RegistryAuth) -> Self {
        self.registry_auth = Some(auth);
        self.version = self.version.max(3);
        self
    }

    /// The same, naming a place shared beyond the home (written at
    /// [`BEYOND_HOME_VERSION`], which a guest that shares only the home
    /// refuses).
    #[must_use]
    pub fn beyond_home(mut self) -> Self {
        self.version = self.version.max(BEYOND_HOME_VERSION);
        self
    }

    /// A request that isn't a command (`args`: [`PORTS_REQUEST`],
    /// [`DOCTOR_REQUEST`], [`STOP_REQUEST`], a forward), from `/`, with
    /// this machine's clock (the guest's follows it: the VM's host asks
    /// every few seconds, so a guest that slept with the Mac catches up
    /// though no command is run).
    #[must_use]
    pub fn internal(args: &[&str]) -> Self {
        let mut request = Self::new(
            args.iter().map(|&arg| arg.to_owned()).collect(),
            "/".to_owned(),
            Vec::new(),
        );
        request.time = clock();
        request
    }

    /// The request, as [`Frame::HelloRun`] carries it.
    ///
    /// # Errors
    ///
    /// [`RequestError::TooLong`].
    pub fn encode(&self) -> Result<Vec<u8>, RequestError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| RequestError::Unreadable(error.to_string()))?;
        if bytes.len() > MAX_REQUEST {
            return Err(RequestError::TooLong(bytes.len()));
        }
        Ok(bytes)
    }

    /// A request read back.
    ///
    /// # Errors
    ///
    /// [`RequestError::Newer`] for a newer version, else
    /// [`RequestError::Unreadable`].
    pub fn decode(bytes: &[u8]) -> Result<Self, RequestError> {
        shipyard_sys::versioned::read(bytes, REQUEST_VERSION).map_err(RequestError::from)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A token is 32 hex digits both ways; anything else (a port, a short
    /// one, a sign) isn't one.
    #[test]
    fn a_session_token_is_32_hex_digits() {
        let token = 0x0123_4567_89ab_cdef_0011_2233_4455_6677_u128;
        assert_eq!(session_token_of(&session_token_text(token)), Some(token));
        assert_eq!(session_token_text(1).len(), 32);
        let signed = format!("+{}", "f".repeat(31));
        for not in ["1024", "", "zz", &"f".repeat(31), &"f".repeat(33), &signed] {
            assert_eq!(session_token_of(not), None, "{not}");
        }
    }

    /// A refusal is read by its code from a version 5 peer, by its words
    /// from an older one; it's sent as the client's version reads it, and
    /// one without a code (its words) as words to any.
    #[test]
    fn a_refusal_goes_by_its_code_and_falls_back_to_words() {
        use super::super::{DECLINE_VERSION, Decline};
        let declined = |reason, why: &str| Frame::Declined {
            reason,
            why: why.to_owned(),
        };
        assert_eq!(
            Refusal::of(&declined(Decline::Stopping, "other words")),
            Some(Refusal::Stopping)
        );
        assert_eq!(
            Refusal::of(&declined(Decline::NotReady, "")),
            Some(Refusal::NotReady)
        );
        assert_eq!(
            Refusal::of(&declined(Decline::Other(9), "a newer reason")),
            Some(Refusal::Other("a newer reason".to_owned()))
        );
        assert_eq!(
            Refusal::of(&Frame::Refused(STOPPING.to_owned())),
            Some(Refusal::Stopping)
        );
        assert_eq!(
            Refusal::of(&Frame::Refused(NOT_READY.to_owned())),
            Some(Refusal::NotReady)
        );
        // An older guest's words, with its tail.
        assert_eq!(
            Refusal::from_words(&format!("{NOT_READY}; try again shortly")),
            Refusal::NotReady
        );
        // Another build's guest (F167): by its code, by this build's words,
        // and by an older guest's own (`RequestError::Newer`'s).
        assert_eq!(
            Refusal::of(&declined(Decline::Newer, "")),
            Some(Refusal::Newer)
        );
        assert_eq!(Refusal::from_words(ANOTHER_BUILD), Refusal::Newer);
        assert_eq!(
            Refusal::from_words(&RequestError::Newer(3).to_string()),
            Refusal::Newer
        );
        assert!(Refusal::Newer.to_string().contains("`shipyard vm stop`"));
        // Words that merely quote it (a path the user named) aren't it.
        assert!(matches!(
            Refusal::from_words("/Users/me/newer than this guest's isn't in the guest"),
            Refusal::Other(_)
        ));
        assert!(matches!(
            Refusal::Newer.frame(DECLINE_VERSION),
            Frame::Declined {
                reason: Decline::Newer,
                ..
            }
        ));
        assert_eq!(
            Refusal::Newer.frame(DECLINE_VERSION - 1),
            Frame::Refused(ANOTHER_BUILD.to_owned())
        );
        assert_eq!(Refusal::of(&Frame::Exited(0)), None);
        assert_eq!(
            Refusal::Stopping.frame(DECLINE_VERSION),
            declined(Decline::Stopping, STOPPING)
        );
        assert_eq!(
            Refusal::NotReady.frame(DECLINE_VERSION - 1),
            Frame::Refused(NOT_READY.to_owned())
        );
        assert_eq!(
            Refusal::Other("no".to_owned()).frame(DECLINE_VERSION),
            Frame::Refused("no".to_owned())
        );
        // On the wire and back, an unknown code kept as its number.
        for frame in [
            declined(Decline::Stopping, STOPPING),
            declined(Decline::Other(200), "?"),
        ] {
            let mut bytes = Vec::new();
            frame.encode(&mut bytes);
            let mut decoder = Decoder::new();
            decoder.push(&bytes);
            assert_eq!(decoder.next_frame(), Ok(Some(frame)));
        }
    }

    /// The caller's named variables go to the guest's `shipyard` in one
    /// variable of its own, read back by name; any value (`=`, quotes, a
    /// newline) as it was; a name it hasn't, or a value that isn't one,
    /// is none.
    #[test]
    fn the_callers_variables_travel_in_one_of_their_own() {
        let named = [
            ("HTTPS_PROXY".to_owned(), "http://127.0.0.1:3128".to_owned()),
            ("TOKEN".to_owned(), "a=b \"c\"\nd".to_owned()),
        ];
        let value = caller_env_value(&named);
        assert_eq!(
            caller_var(&value, "HTTPS_PROXY").as_deref(),
            Some("http://127.0.0.1:3128")
        );
        assert_eq!(caller_var(&value, "TOKEN").as_deref(), Some("a=b \"c\"\nd"));
        assert_eq!(caller_var(&value, "OTHER"), None);
        assert_eq!(caller_var("not json", "TOKEN"), None);
        assert_eq!(caller_var(r#"{"TOKEN":1}"#, "TOKEN"), None);
        assert_eq!(caller_env_value(&[]), "{}");
    }

    /// The hidden places cross the kernel's command line whole, spaces and
    /// all; one that can't be read is left out.
    #[test]
    fn hidden_places_cross_the_command_line() {
        let places = [
            "/Users/me/Library/Application Support/Shipyard/vm",
            "/Users/me/g",
        ];
        let line = format!(
            "console=hvc0 {HIDE_KEY}{} {SHARES_KEY}{} quiet",
            places_value(&places),
            places_value(&MAC_SHARES)
        );
        assert!(!line.contains("Application Support"));
        assert_eq!(places_of(&line, HIDE_KEY), places);
        assert_eq!(places_of(&line, SHARES_KEY), MAC_SHARES);
        assert_eq!(places_of(&format!("{HIDE_KEY}zz,2f61"), HIDE_KEY), ["/a"]);
        // Numbered as sent: one that can't be read keeps the next's number.
        assert_eq!(
            numbered_places_of(&format!("{SHARES_KEY}zz,2f61"), SHARES_KEY),
            [(1, "/a".to_owned())]
        );
        assert!(places_of("console=hvc0", HIDE_KEY).is_empty());
    }

    /// The shares' tags: the home's and each other's, nothing else.
    #[test]
    fn the_macs_share_tags_are_told() {
        assert!(is_mac_share_tag(HOME_TAG));
        assert!(is_mac_share_tag(&share_tag(0)));
        assert!(is_mac_share_tag(&share_tag(12)));
        for other in ["share", "sharex", "rosetta", "share-1", ""] {
            assert!(!is_mac_share_tag(other), "{other}");
        }
    }

    /// An internal request carries the clock (the guest follows it), from
    /// `/`, with no environment.
    #[test]
    fn an_internal_request_carries_the_clock() {
        let request = Request::internal(&[PORTS_REQUEST]);
        assert_eq!(request.args, [PORTS_REQUEST]);
        assert_eq!(request.cwd, "/");
        assert!(request.env.is_empty());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(request.time.is_some_and(|time| now.abs_diff(time) <= 1));
    }

    /// A ports answer reads back as written; a newer one is refused.
    #[test]
    fn a_ports_answer_is_versioned() {
        let answer = PortsAnswer {
            version: PORTS_VERSION,
            active: 2,
            ports: vec![
                PublishedPort {
                    ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
                    port: 8080,
                    protocol: Protocol::Tcp,
                },
                PublishedPort {
                    ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
                    port: 53,
                    protocol: Protocol::Udp,
                },
            ],
            udp: 0,
            comes_back: Some(3),
        };
        let bytes = answer.encode().unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains(r#""protocol":"udp""#));
        assert!(String::from_utf8_lossy(&bytes).contains(r#""comes_back":3"#));
        assert_eq!(PortsAnswer::decode(&bytes).unwrap(), answer);
        // An older guest's: its ports TCP, its UDP a count.
        let v1 = br#"{"version":1,"active":0,"ports":[{"ip":"127.0.0.1","port":80}],"udp":2}"#;
        let older = PortsAnswer::decode(v1).unwrap();
        assert_eq!(older.ports[0].port, 80);
        assert_eq!(older.ports[0].protocol, Protocol::Tcp);
        assert_eq!(older.udp, 2);
        // A v2 guest's: nothing said of what comes back (never taken for 0).
        let v2 = br#"{"version":2,"active":1,"ports":[],"udp":0}"#;
        assert_eq!(PortsAnswer::decode(v2).unwrap().comes_back, None);
        let newer = br#"{"version":4,"active":0,"ports":[],"more":1}"#;
        assert_eq!(PortsAnswer::decode(newer), Err(RequestError::Newer(4)));
    }

    /// Datagrams cross a stream whole, one frame each with its peer, an
    /// empty one too, read across short reads; one past the bound isn't
    /// written, a cut one isn't read, and the stream's end before a frame is
    /// said as such.
    #[test]
    fn datagrams_are_framed() {
        // One byte a read, as a stream may give them.
        struct Trickle<'a>(&'a [u8]);
        impl std::io::Read for Trickle<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let Some((&first, rest)) = self.0.split_first() else {
                    return Ok(0);
                };
                if buffer.is_empty() {
                    return Ok(0);
                }
                buffer[0] = first;
                self.0 = rest;
                Ok(1)
            }
        }
        let mut stream = Vec::new();
        let biggest = vec![7u8; MAX_DATAGRAM];
        for (peer, datagram) in [(1, &b"hello"[..]), (u32::MAX, b""), (7, &biggest[..])] {
            write_datagram(&mut stream, peer, datagram).unwrap();
        }
        let too_big = write_datagram(&mut Vec::new(), 1, &vec![0; MAX_DATAGRAM + 1]).unwrap_err();
        assert_eq!(too_big.kind(), std::io::ErrorKind::InvalidInput);
        let mut reader = Trickle(&stream);
        let mut buffer = Vec::new();
        assert_eq!(read_datagram(&mut reader, &mut buffer).unwrap(), Some(1));
        assert_eq!(buffer, b"hello");
        assert_eq!(
            read_datagram(&mut reader, &mut buffer).unwrap(),
            Some(u32::MAX)
        );
        assert!(buffer.is_empty());
        assert_eq!(read_datagram(&mut reader, &mut buffer).unwrap(), Some(7));
        assert_eq!(buffer.len(), MAX_DATAGRAM);
        assert_eq!(
            read_datagram(&mut reader, &mut buffer).unwrap(),
            None,
            "the end"
        );
        let cut = read_datagram(&mut &stream[..8], &mut buffer).unwrap_err();
        assert_eq!(cut.kind(), std::io::ErrorKind::UnexpectedEof);
        let cut_head = read_datagram(&mut &[0u8, 0, 0][..], &mut buffer).unwrap_err();
        assert_eq!(cut_head.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// An older host's answer: the TCP ports, the UDP ones counted (with an
    /// older guest's own count), the active ones as they are.
    #[test]
    fn an_older_host_is_answered_at_v1() {
        // As an older host reads it (v1's own shape: `udp` required, nothing
        // else allowed), with no UDP port at all too.
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code, reason = "read only to be refused or not")]
        struct OlderPort {
            ip: std::net::IpAddr,
            port: u16,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code, reason = "read only to be refused or not")]
        struct OlderAnswer {
            version: u32,
            active: u32,
            ports: Vec<OlderPort>,
            udp: u32,
        }
        let mut answer = PortsAnswer::new();
        answer.active = 3;
        for (port, protocol) in [
            (80, Protocol::Tcp),
            (53, Protocol::Udp),
            (5353, Protocol::Udp),
        ] {
            answer.ports.push(PublishedPort {
                ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
                port,
                protocol,
            });
        }
        let v1 = answer.as_v1();
        assert_eq!((v1.version, v1.active, v1.udp), (1, 3, 2));
        assert_eq!(v1.ports.len(), 1);
        assert_eq!(v1.ports[0].port, 80);
        let bytes = String::from_utf8(v1.encode().unwrap()).unwrap();
        assert!(!bytes.contains("protocol"), "{bytes}");
        let mut tcp_only = PortsAnswer::new();
        tcp_only.ports.push(PublishedPort {
            ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
            port: 80,
            protocol: Protocol::Tcp,
        });
        for answer in [answer.as_v1(), tcp_only.as_v1()] {
            let bytes = answer.encode().unwrap();
            let older: OlderAnswer = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)));
            assert_eq!(older.version, 1);
        }
    }

    /// A wide range's answer (`-p 8000-9999`, on two addresses) is written
    /// and read whole, within what's read of it.
    #[test]
    fn a_wide_ranges_answer_fits() {
        let mut answer = PortsAnswer::new();
        for port in 8000..10_000 {
            for ip in [
                std::net::Ipv4Addr::UNSPECIFIED.into(),
                std::net::Ipv6Addr::UNSPECIFIED.into(),
            ] {
                answer.ports.push(PublishedPort {
                    ip,
                    port,
                    protocol: Protocol::Tcp,
                });
            }
        }
        let bytes = answer.encode().unwrap();
        assert!(bytes.len() > super::super::MAX_PAYLOAD && bytes.len() < MAX_PORTS_ANSWER);
        assert_eq!(PortsAnswer::decode(&bytes).unwrap(), answer);
    }

    /// Bytes that came in the same read as a frame, after it, are handed
    /// back with the stream: none lost when the connection turns raw.
    #[test]
    fn raw_bytes_after_a_frame_are_kept() {
        let mut bytes = Vec::new();
        Frame::Exited(0).encode(&mut bytes);
        bytes.extend_from_slice(b"raw after");
        let mut connection = Connection::new(std::io::Cursor::new(bytes));
        assert_eq!(connection.next_frame().unwrap(), Some(Frame::Exited(0)));
        let (stream, unread) = connection.into_parts();
        let mut rest = unread;
        std::io::Read::read_to_end(&mut { stream }, &mut rest).unwrap();
        assert_eq!(rest, b"raw after");
    }

    /// Only the terminal's and the language's variables pass, as text:
    /// a store's place, a secret, or a variable that isn't UTF-8, doesn't.
    #[test]
    fn only_some_variables_pass() {
        use std::os::unix::ffi::OsStrExt as _;
        let vars = [
            (
                std::ffi::OsStr::new("TERM"),
                std::ffi::OsStr::new("xterm-256color"),
            ),
            ("LANG".as_ref(), "en_GB.UTF-8".as_ref()),
            ("SHIPYARD_IMAGE_STORE".as_ref(), "/Users/me/images".as_ref()),
            ("AWS_SECRET_ACCESS_KEY".as_ref(), "x".as_ref()),
            ("LC_ALL".as_ref(), std::ffi::OsStr::from_bytes(b"\xff")),
        ];
        assert_eq!(
            passed_env(vars),
            [
                ("TERM".to_owned(), "xterm-256color".to_owned()),
                ("LANG".to_owned(), "en_GB.UTF-8".to_owned()),
            ]
        );
    }

    /// The guest's own `shipyard`'s variables, and the loader's, can't be
    /// copied in; a command's own can.
    #[test]
    fn the_guests_own_variables_are_reserved() {
        for name in [
            "PATH",
            "HOME",
            "TMPDIR",
            "SHIPYARD_IMAGE_STORE",
            "XDG_RUNTIME_DIR",
            "LD_PRELOAD",
        ] {
            assert!(reserved(name), "{name}");
        }
        for name in ["TOKEN", "DATABASE_URL", "PATHS", "HOMEBREW_PREFIX"] {
            assert!(!reserved(name), "{name}");
        }
    }

    /// A connection's frames, sent whole, read back one by one, then its
    /// end.
    #[test]
    fn a_connection_sends_and_reads_frames() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut a = Connection::new(a);
        let mut b = Connection::new(b);
        a.send(&Frame::Stdout(b"out".to_vec())).unwrap();
        a.send(&Frame::Exited(3)).unwrap();
        drop(a);
        assert_eq!(
            b.next_frame().unwrap(),
            Some(Frame::Stdout(b"out".to_vec()))
        );
        assert_eq!(b.next_frame().unwrap(), Some(Frame::Exited(3)));
        assert_eq!(b.next_frame().unwrap(), None);
    }

    /// A request reads back as it was written; one too long isn't made;
    /// a newer version is refused, not guessed at; one with a field this
    /// version doesn't know isn't read.
    #[test]
    fn a_request_round_trips_and_its_limits_hold() {
        let request = Request::new(
            vec!["run".to_owned(), "--rm".to_owned(), "alpine".to_owned()],
            "/Users/me/app".to_owned(),
            vec![("TERM".to_owned(), "xterm-256color".to_owned())],
        );
        let bytes = request.encode().unwrap();
        assert_eq!(Request::decode(&bytes).unwrap(), request);
        let long = Request::new(vec!["x".repeat(MAX_REQUEST)], "/".to_owned(), Vec::new());
        assert!(matches!(long.encode(), Err(RequestError::TooLong(_))));
        let near = Request::new(
            vec!["x".repeat(MAX_REQUEST - 200)],
            "/".to_owned(),
            Vec::new(),
        );
        assert!(near.encode().unwrap().len() <= MAX_REQUEST, "the control");
        assert_eq!(
            Request::decode(br#"{"version":5,"args":[],"cwd":"/","env":[]}"#),
            Err(RequestError::Newer(5))
        );
        // Without a credential, version 2 (an older guest reads it); with
        // one, 3 (an older guest refuses it, never runs it anonymously),
        // read back whole, its `Debug` never showing it.
        assert_eq!(request.version, 2);
        let handed = request
            .clone()
            .with_registry_auth(RegistryAuth("c2VjcmV0".to_owned()));
        assert_eq!(handed.version, 3);
        // A place beyond the home: 4, which a stage-12 guest (it reads up
        // to 3) refuses; with a credential too, still 4.
        let beyond = request.clone().beyond_home();
        assert_eq!(beyond.version, BEYOND_HOME_VERSION);
        assert_eq!(handed.clone().beyond_home().version, BEYOND_HOME_VERSION);
        assert_eq!(
            beyond
                .with_registry_auth(RegistryAuth("x".to_owned()))
                .version,
            4
        );
        assert_eq!(Request::decode(&handed.encode().unwrap()).unwrap(), handed);
        assert!(!format!("{handed:?}").contains("c2VjcmV0"));
        assert!(
            !String::from_utf8(request.encode().unwrap())
                .unwrap()
                .contains("registry_auth")
        );
        // Version 1, as 10.3a's CLI wrote it: read, its new fields their
        // defaults.
        let v1 = Request::decode(br#"{"version":1,"args":["ps"],"cwd":"/","env":[]}"#).unwrap();
        assert_eq!((v1.input, v1.terminal, v1.time), (false, None, None));
        assert!(v1.named_env.is_empty());
        assert!(matches!(
            Request::decode(br#"{"version":1,"args":[],"cwd":"/","env":[],"x":1}"#),
            Err(RequestError::Unreadable(_))
        ));
    }
}
