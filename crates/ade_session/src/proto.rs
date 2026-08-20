//! Wire types for the ADE session-daemon protocol (generation 3).
//!
//! Every frame that concerns a session carries a [`SessionId`]; that is what
//! multiplexes many sessions over a single connection. Frames are encoded by
//! [`crate::framing`] as a length-prefixed **envelope**:
//! `{"op": <string>, "rid": <u64, optional>, "body": <object>}`. The normative
//! contract is `docs/ade/protocol-compatibility.md`; this module implements
//! §2–§5 of it.
//!
//! The [`Frame`] enum stays a single internally tagged enum in memory — the
//! codec turns its `type` tag into `op` and lifts `request_id` out into `rid`,
//! so the in-memory shape and the wire shape are related by a mechanical
//! transform rather than by two hand-written representations.
//!
//! **Evolution rule:** this is plain serde JSON with the default
//! unknown-field behaviour (ignore). Never add `deny_unknown_fields`, and add
//! new fields only as `Option` / `#[serde(default)]` so that a peer negotiated
//! at a lower generation can still decode a newer peer's frames.
//!
//! A new *operation* is a new [`Frame`] variant, and it does **not** move the
//! generation: the generation gates the envelope and framing shape,
//! capabilities gate operations (§5) — and an op may additionally be declared
//! *generation-gated*, which [`Frame::FocusSession`] is. An op a peer has never
//! heard of is now a *request-scoped* failure — the receiver answers
//! [`error_code::UNKNOWN_OP`] and keeps serving — where before it killed the
//! connection. That safety net is not the plan: a sender must not emit an op
//! the peer's effective capability set, or its negotiated generation, lacks in
//! the first place.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::framing::bounded;

/// Lowest protocol generation this build can serve.
///
/// The envelope shipped as **generation 2**. Generation 1 is retroactively the
/// pre-cut protocol — `type`/`request_id` inline, no `op`/`rid`/`body` — and is
/// never advertised.
///
/// **Both peers serve a sliding window of two generations, current and
/// previous.** A change of meaning on an existing frame still bumps the
/// generation — `create_workspace` lost its combined workspace+first-shell arm
/// at 3 (§4.1) — but the previous meaning keeps being served, on connections
/// that negotiated the previous generation, for one window. Refusing a peer
/// over skew is retired: a daemon is pinned by the sessions it holds, and an
/// upgrade that orphans them is not an upgrade. Everything a generation
/// changed is therefore a per-connection decision on the receiver's side, not
/// a reason to hang up.
pub const MIN_GENERATION: u32 = 2;

/// Highest protocol generation this build can serve. See [`MIN_GENERATION`].
pub const MAX_GENERATION: u32 = 3;

/// Every `op` string this build can decode, in [`Frame`] declaration order.
///
/// This is the list [`crate::framing::read_frame`] consults to tell an op it
/// has never heard of (`unknown_op`, request-scoped) from a body it could not
/// parse (`malformed_body`, also request-scoped but a different bug on the
/// sender's side). It is kept honest by a test that encodes one frame of every
/// variant and asserts set-equality with this constant — hand-maintaining a
/// string list next to an enum is exactly the thing that rots silently, so the
/// test is not optional.
pub const KNOWN_OPS: &[&str] = &[
    // ---- client → daemon ----
    "hello",
    "create_workspace",
    "open_workspace",
    "list_workspaces",
    "update_layout",
    "rename_workspace",
    "kill_workspace",
    "create_session",
    "list_sessions",
    "attach",
    "detach",
    "write",
    "resize",
    "focus_session",
    "kill",
    "subscribe",
    "shutdown",
    // ---- daemon → client ----
    "hello_ack",
    "session_list",
    "created",
    "workspace",
    "workspace_list",
    "layout_changed",
    "workspace_removed",
    "removed",
    "replay",
    "output",
    "status",
    "exited",
    "shutdown_ack",
    "error",
];

/// Machine-readable codes carried by [`Frame::Error`].
///
/// Deliberately `&'static str` constants and not an enum: the wire type is an
/// **open** string by contract. New codes may be added at any generation, and a
/// reader must treat one it does not recognise as a generic failure rather than
/// as a decode error (`docs/ade/protocol-compatibility.md` §2.1). An enum would
/// make an unknown code fail to parse, which is the opposite of that rule.
pub mod error_code {
    /// The envelope itself did not parse: not an object, no `op`, no `body`,
    /// `body` not an object, or `rid` not an unsigned integer. The one decode
    /// failure the receiver may close the connection over.
    pub const MALFORMED_FRAME: &str = "malformed_frame";
    /// The `op` was known but its `body` did not decode into that op's type.
    pub const MALFORMED_BODY: &str = "malformed_body";
    /// The `op` string is not one this peer implements.
    pub const UNKNOWN_OP: &str = "unknown_op";
    /// No generation is common to both peers' ranges. Fatal to the handshake.
    pub const UNSUPPORTED_GENERATION: &str = "unsupported_generation";
    /// A capability-gated op arrived from a peer whose effective set lacks it,
    /// or a generation-gated one — [`super::Frame::FocusSession`] — arrived on
    /// a connection below the generation that defines it.
    pub const UNCAPABLE_PEER: &str = "uncapable_peer";
    /// No such session / workspace.
    pub const NOT_FOUND: &str = "not_found";
    /// A revision-guarded write lost: `rev` at or below the stored one.
    pub const STALE_REV: &str = "stale_rev";
    /// A field's value is unusable — an empty name, a capability list past its
    /// bounds.
    pub const INVALID_ARGUMENT: &str = "invalid_argument";
    /// The mutation applied but could not be durably recorded (§8.1).
    pub const PERSIST_FAILED: &str = "persist_failed";
    /// A request the daemon understood and chose not to honour, e.g. a polite
    /// `shutdown` while sessions are busy.
    pub const DECLINED: &str = "declined";
    /// A daemon-internal failure with no better code — a spawn that failed, a
    /// lock that was poisoned. §2.1 defines it at generation 2, as "a failure
    /// inside the daemon — a spawn or io error the requester did not cause".
    pub const INTERNAL: &str = "internal";

    /// Whether an error frame carrying `code` is the peer's answer to **one
    /// frame we sent** rather than something that ends the stream (§2).
    ///
    /// The wire-side twin of
    /// [`ReadFrameError::is_request_scoped`](crate::framing::ReadFrameError::is_request_scoped):
    /// that one classifies a decode failure we hit, this one classifies the
    /// error frame the peer sends back for the same failure on its side. §2
    /// says the receiver of a bad `op` or a bad `body` "MUST keep the
    /// connection serving" — so the sender of that frame must keep going too,
    /// or one rejected `resize` takes a terminal down with it.
    ///
    /// [`UNCAPABLE_PEER`] is here for the same reason as [`UNKNOWN_OP`]: §3.3
    /// has its receiver answer and "keep serving", so the op is refused and the
    /// stream is not. A daemon serving a generation-2 connection emits it for
    /// [`super::Frame::FocusSession`]; without this, a client would end a
    /// terminal over a single gated op.
    ///
    /// [`MALFORMED_FRAME`] is deliberately not here, and the reason is not that
    /// the stream desynced — it does not. [`crate::framing::read_frame`] takes
    /// the length prefix, checks it, and reads the whole payload *before*
    /// decoding it, so a malformed envelope leaves the reader exactly where the
    /// next prefix begins; the reads that really would lose the length are
    /// classified [`crate::framing::ReadFrameError::Transport`]. It is that
    /// `rid` lives in the envelope, so a failure to read the envelope names no
    /// request and cannot be charged to one — and the layer that failed is the
    /// one every frame shares, so nothing says the next will parse either.
    /// Hence §2 makes closing over it a MAY: this repo's two clients answer it
    /// and then differ, both conformant.
    pub fn is_request_scoped(code: &str) -> bool {
        code == MALFORMED_BODY || code == UNKNOWN_OP || code == UNCAPABLE_PEER
    }
}

/// The generation two peers will speak, or `None` when their ranges are
/// disjoint.
///
/// `G = min(client.max, daemon.max)`, valid only when `G >= max(client.min,
/// daemon.min)`. The **daemon** calls this and the client verifies the answer
/// (`docs/ade/protocol-compatibility.md` §3.1) — one authority, so a mismatch
/// has one place to read.
pub fn select_generation(
    client_min: u32,
    client_max: u32,
    daemon_min: u32,
    daemon_max: u32,
) -> Option<u32> {
    let selected = client_max.min(daemon_max);
    (selected >= client_min.max(daemon_min)).then_some(selected)
}

/// Largest capability list a peer may advertise (§3.2).
pub const MAX_CAPABILITIES: usize = 256;

/// Longest capability identifier, in bytes (§3.2).
pub const MAX_CAPABILITY_BYTES: usize = 64;

/// Check an advertised capability list against §3.2's bounds.
///
/// A violation is `invalid_argument` and **fatal to the handshake** — unlike an
/// unknown identifier, which is ignored and simply falls out of the
/// intersection. The returned string names the offender because the only person
/// who can fix it is looking at a log line — but names it through
/// [`crate::framing::bounded`], since that string travels to the peer as an
/// error frame's `message`.
pub fn validate_capabilities(capabilities: &[String]) -> Result<(), String> {
    if capabilities.len() > MAX_CAPABILITIES {
        return Err(format!(
            "{} capabilities advertised, at most {MAX_CAPABILITIES} are allowed",
            capabilities.len()
        ));
    }
    for capability in capabilities {
        if capability.is_empty() {
            return Err("an empty capability identifier is not allowed".to_owned());
        }
        if capability.len() > MAX_CAPABILITY_BYTES {
            // The one branch reachable with a capability of any length at all,
            // and this reason becomes an error frame's `message`
            // (`crates/ade_session_daemon/src/server.rs`'s handshake). Quoting
            // it whole would let a peer's megabyte-long identifier choose the
            // size of the frame rejecting it — past `MAX_FRAME_BYTES` once
            // Debug and JSON escaping have had their way, at which point the
            // handshake dies unanswered instead of being cleanly refused. The
            // branches below are reached only after this one passed, so 64
            // bytes is already their bound.
            return Err(format!(
                "capability {:?} is {} bytes, at most {MAX_CAPABILITY_BYTES} are allowed",
                bounded(capability),
                capability.len()
            ));
        }
        if let Some(bad) = capability
            .chars()
            .find(|c| !matches!(c, 'a'..='z' | '0'..='9' | '_' | '.' | '-'))
        {
            return Err(format!(
                "capability {capability:?} contains {bad:?}; only [a-z0-9_.-] is allowed"
            ));
        }
    }
    Ok(())
}

/// The effective capability set: the intersection of what both peers advertise.
///
/// Duplicates within one list are deduplicated rather than rejected — a
/// repeated identifier still says the peer is capable — and an identifier the
/// other side has never heard of simply falls out. Both peers compute this and
/// both get the same answer; it is fixed for the life of the connection (§3.2).
pub fn effective_capabilities(mine: &[String], theirs: &[String]) -> BTreeSet<String> {
    let theirs: BTreeSet<&str> = theirs.iter().map(String::as_str).collect();
    mine.iter()
        .filter(|capability| theirs.contains(capability.as_str()))
        .cloned()
        .collect()
}

/// Opaque identifier for a daemon-owned session.
///
/// Serde-transparent: encodes as a bare JSON string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for SessionId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

/// ADE's status-dot semantics, derived by the daemon from the session's
/// process and output activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Recent output from the agent process.
    Working,
    /// Bell, or silent past the needs-input threshold.
    NeedsInput,
    /// Alive but sitting at a shell prompt.
    Idle,
    /// The process is gone; the session is retained until killed.
    Exited,
}

/// Everything the client needs to render a session row without attaching.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub workspace_id: String,
    pub agent_kind: String,
    pub instance_label: String,
    pub cwd: String,
    /// Unix seconds.
    pub created_at: u64,
    pub status: SessionStatus,
}

/// Schema version stamped on every [`LayoutDoc`].
///
/// Bump only on a change an older peer could not read; new fields go in as
/// `Option` / `#[serde(default)]` like everywhere else in this module.
pub const LAYOUT_SCHEMA_VERSION: u32 = 1;

fn layout_schema_version() -> u32 {
    LAYOUT_SCHEMA_VERSION
}

/// Which way a [`LayoutNode::Split`] divides its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// One tab in a [`LayoutNode::Leaf`].
///
/// `Terminal` names a session **this daemon owns**, and is the only part of a
/// layout the daemon validates. `Editor` is an opaque path: what an editor tab
/// holds is the client's business, and the daemon never looks inside it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Tab {
    Terminal { session_id: SessionId },
    Editor { path: String },
}

/// The layout tree: splits all the way down to leaves holding tabs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LayoutNode {
    Split {
        dir: SplitDir,
        /// Fraction of the box given to the first child, `0.0..=1.0`.
        ratio: f32,
        children: Box<[LayoutNode; 2]>,
    },
    Leaf {
        tabs: Vec<Tab>,
        /// Index into `tabs`. Clamped by the daemon whenever tabs are pruned.
        #[serde(default)]
        active: usize,
        #[serde(default)]
        focused: bool,
    },
}

impl LayoutNode {
    /// A leaf holding `tabs`, the first one active and unfocused.
    pub fn leaf(tabs: Vec<Tab>) -> Self {
        Self::Leaf {
            tabs,
            active: 0,
            focused: false,
        }
    }

    fn append_terminals(&self, out: &mut Vec<SessionId>) {
        match self {
            Self::Leaf { tabs, .. } => out.extend(tabs.iter().filter_map(|tab| match tab {
                Tab::Terminal { session_id } => Some(session_id.clone()),
                Tab::Editor { .. } => None,
            })),
            Self::Split { children, .. } => {
                for child in children.iter() {
                    child.append_terminals(out);
                }
            }
        }
    }

    /// Drop the terminal tabs `keep` rejects, collapsing whatever that empties.
    ///
    /// `None` means the node disappeared entirely. A split that loses one child
    /// becomes the other child, so pruning never leaves a split with a hole in
    /// it.
    fn pruned(self, keep: &mut impl FnMut(&SessionId) -> bool) -> Option<Self> {
        match self {
            Self::Leaf {
                tabs,
                active,
                focused,
            } => {
                let tabs: Vec<Tab> = tabs
                    .into_iter()
                    .filter(|tab| match tab {
                        Tab::Terminal { session_id } => keep(session_id),
                        Tab::Editor { .. } => true,
                    })
                    .collect();
                if tabs.is_empty() {
                    return None;
                }
                let active = active.min(tabs.len() - 1);
                Some(Self::Leaf {
                    tabs,
                    active,
                    focused,
                })
            }
            Self::Split {
                dir,
                ratio,
                children,
            } => {
                let [first, second] = *children;
                match (first.pruned(keep), second.pruned(keep)) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        dir,
                        ratio,
                        children: Box::new([first, second]),
                    }),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }
}

/// A workspace's layout, versioned so that a document outlives the code that
/// wrote it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutDoc {
    #[serde(default = "layout_schema_version")]
    pub schema_version: u32,
    pub root: LayoutNode,
}

impl LayoutDoc {
    /// One empty leaf — what a workspace whose terminals are all gone looks
    /// like.
    pub fn empty() -> Self {
        Self::new(LayoutNode::leaf(Vec::new()))
    }

    pub fn new(root: LayoutNode) -> Self {
        Self {
            schema_version: LAYOUT_SCHEMA_VERSION,
            root,
        }
    }

    /// One leaf holding one terminal tab: what a migrated flat session becomes.
    pub fn single_terminal(session_id: SessionId) -> Self {
        Self::new(LayoutNode::leaf(vec![Tab::Terminal { session_id }]))
    }

    /// Every session named by a terminal tab, in tree order.
    pub fn terminal_sessions(&self) -> Vec<SessionId> {
        let mut out = Vec::new();
        self.root.append_terminals(&mut out);
        out
    }

    /// Drop the terminal tabs `keep` rejects. Returns whether anything went.
    ///
    /// This is how a daemon restart stays honest: a pty cannot be resurrected,
    /// so the tabs naming dead sessions are removed rather than left dangling,
    /// and the splits and editor tabs around them survive.
    pub fn retain_sessions(&mut self, mut keep: impl FnMut(&SessionId) -> bool) -> bool {
        let before = self.terminal_sessions();
        let root = std::mem::replace(&mut self.root, LayoutNode::leaf(Vec::new()));
        self.root = root
            .pruned(&mut keep)
            .unwrap_or_else(|| LayoutNode::leaf(Vec::new()));
        before != self.terminal_sessions()
    }
}

impl Default for LayoutDoc {
    fn default() -> Self {
        Self::empty()
    }
}

/// A workspace as the daemon owns it: a project root, a layout, and the
/// sessions its terminal tabs name.
///
/// The daemon is the source of truth for all of it; the client renders this
/// and asks for changes with [`Frame::UpdateLayout`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub project_root: String,
    /// Unix seconds.
    pub created_at: u64,
    /// Bumped on every accepted [`Frame::UpdateLayout`]; the guard that makes
    /// last-writer-wins safe between two clients.
    pub layout_rev: u64,
    pub layout: LayoutDoc,
}

/// First frame on every connection.
///
/// The pre-cut `protocol_version` field is **gone**, not deprecated: the
/// envelope is a one-time compatibility break (§6), and leaving a field that
/// only ever meant "exactly 1" would invite a compatibility decision to be made
/// on it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub min_generation: u32,
    pub max_generation: u32,
    /// Optional behaviours this client implements. Absent reads as none, which
    /// is what every peer that advertises nothing means.
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
}

impl Hello {
    /// A `Hello` announcing this build's generation range and no capabilities.
    pub fn current() -> Self {
        Self {
            min_generation: MIN_GENERATION,
            max_generation: MAX_GENERATION,
            capabilities: Vec::new(),
            request_id: None,
        }
    }
}

impl Default for Hello {
    fn default() -> Self {
        Self::current()
    }
}

/// The daemon's reply to [`Hello`]. Returned by
/// [`Connection::handshake`](crate::client::Connection::handshake), which is
/// why it is a struct rather than bare enum fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub daemon_version: String,
    /// Legacy informational field, equal to [`HelloAck::generation`]. Kept
    /// because a human reading a frame dump still looks for it; it is never
    /// used for a compatibility decision again (§3.1).
    pub protocol_version: u32,
    pub host_os: String,
    /// The daemon's own range, for diagnostics: a client that fails
    /// verification can report both ranges rather than only its own.
    pub min_generation: u32,
    pub max_generation: u32,
    /// The generation the **daemon selected** and will serve on this
    /// connection. The client verifies it lies inside its own range and closes
    /// the connection with a legible error if it does not (§3.1).
    pub generation: u32,
    /// Optional behaviours this daemon implements. Absent reads as none.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// The daemon's ledger is read-only for it — it found one written by a
    /// newer schema version — so mutations apply in memory and their acks carry
    /// `persisted: false` (§8.5). Absent reads as `false`, the normal case a
    /// daemon without the flag always meant. A client should surface this: work
    /// in these sessions will not survive a daemon restart.
    #[serde(default)]
    pub degraded: bool,
    /// Hex sha256 of the binary this daemon is running, hashed once at
    /// startup. The crate version cannot tell two dev builds apart; this can,
    /// and it is what a client compares against the binary it would deploy.
    /// `None` from a daemon that predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_hash: Option<String>,
    /// Whether the daemon holds nothing an **upgrade** may not sacrifice:
    /// every row is either a lost tombstone or an idle shell — a session whose
    /// child is alive but quiet, with a shell in the foreground, i.e. an empty
    /// terminal at a prompt. A session with something running in it, or an
    /// exited-but-unkilled row whose last screen exists only in this process,
    /// makes this `false`. A client seeing `Some(true)` together with a
    /// `binary_hash` it disagrees with may send [`Frame::Shutdown`] and deploy
    /// a fresh binary. `None` from a daemon that predates the field, which a
    /// client must read as "never".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_ready: Option<bool>,
    /// **Which daemon this is**, minted once and kept in its state dir across
    /// restarts, degraded ledger or not. A host may be spelled several ways —
    /// an IP, a hostname, an ssh alias — and each spelling gets its own client
    /// backend; this is what tells the client those backends are one daemon,
    /// so one workspace is not locked, cached, listed and persisted twice.
    ///
    /// Additive, so the generation does not move: `None` from a daemon that
    /// predates the field, and a client reading `None` falls back to the host
    /// spelling as the identity, which is what it always used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
}

/// The absent meaning of the `persisted` flag every mutation ack carries: a
/// peer that does not have the field recorded everything it acked (§8.5).
fn persisted_by_default() -> bool {
    true
}

fn was_persisted(persisted: &bool) -> bool {
    *persisted
}

/// One protocol message, in either direction.
///
/// A single enum for both directions keeps [`crate::framing`] and
/// [`crate::client::Connection`] free of a direction parameter; a peer that
/// receives a frame it does not handle answers with [`Frame::Error`].
///
/// Request-bearing variants carry an optional `request_id` which the direct
/// response ([`Frame::HelloAck`], [`Frame::SessionList`], [`Frame::Created`],
/// [`Frame::Workspace`], [`Frame::Error`]) echoes back. Fire-and-forget
/// variants (`Write`, `Resize`) and unsolicited events (`Output`, `Status`,
/// `Exited`, `Removed`, and the broadcast half of `LayoutChanged` /
/// `WorkspaceRemoved`) have none.
///
/// `request_id` is the **in-memory** correlation slot; on the wire it is lifted
/// out of the payload into the envelope's `rid` by [`crate::framing`], which is
/// why every variant that has one keeps it `Option` with `skip_serializing_if`.
///
/// The `snake_case` rename is load-bearing, not cosmetic: the internal tag is
/// what the codec turns into the envelope's `op`, so the variant names *are*
/// the permanent op identifiers listed in [`KNOWN_OPS`]. Renaming a variant
/// renames an op, and an op string may never be reused for a different
/// operation (§2).
///
/// Not `Eq`: [`LayoutNode::Split`] carries a float ratio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    // ---- client → daemon ----
    /// Must be the first frame the client sends.
    Hello(Hello),
    /// Create a workspace record alone — no session, no layout — answered with
    /// [`Frame::Workspace`] and an empty `sessions`. What a panel row is the
    /// moment it appears; its first terminal is a separate
    /// [`Frame::CreateSession`] naming the id this returns.
    ///
    /// **At generation 2 this is the combined create**: record, first login
    /// shell and a one-leaf layout holding it, answered with that session in
    /// `sessions`. Losing that arm is why the generation moved (§4.1), and
    /// serving it on gen-2 connections is why the window exists.
    CreateWorkspace {
        /// Project root, resolved on the daemon's host.
        root: String,
        /// Defaults to the last component of `root`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Generation-2 request field: the first session's environment. A
        /// gen-3 receiver ignores it and a gen-3 sender must not emit it —
        /// kept only so the gen-2 request can still be decoded whole.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<(String, String)>,
        /// Generation-2 request field: the first session's pty size, 80x24 by
        /// default. See `env`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cols: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// Ask for a workspace's layout and sessions. Attaching to each session is
    /// a separate [`Frame::Attach`]; this frame never streams anything.
    OpenWorkspace {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    ListWorkspaces {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// Store a new layout. Last writer wins, guarded by `rev`: a `rev` at or
    /// below the stored `layout_rev` is stale and rejected with
    /// [`Frame::Error`], so the client must send `layout_rev + 1`.
    UpdateLayout {
        workspace_id: String,
        layout: LayoutDoc,
        rev: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// Give a workspace a new display name, answered with [`Frame::Workspace`]
    /// carrying the renamed record.
    ///
    /// **The id never changes.** A name is display metadata; the id is what
    /// every session, layout and client-side row is keyed by, so renaming is
    /// the one workspace field that can move without anything having to be
    /// re-linked. An empty or whitespace-only name, and an id the daemon does
    /// not hold, are both [`Frame::Error`] — nothing is created here.
    RenameWorkspace {
        workspace_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// Kill every session in the workspace and forget the workspace. The only
    /// workspace-level kill; closing one terminal tab is [`Frame::Kill`].
    KillWorkspace {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    CreateSession {
        /// The workspace this session belongs to, and it must already exist:
        /// creating a session never creates a record. An id the daemon does not
        /// hold is [`error_code::NOT_FOUND`], an empty one
        /// [`error_code::INVALID_ARGUMENT`].
        ///
        /// **At generation 2 both auto-create instead**: an unknown id makes
        /// the record under that id, an empty one mints a fresh id, rooted at
        /// `cwd` and named from `instance_label`.
        workspace_id: String,
        cwd: String,
        /// What to run on the new pty, resolved **on the daemon's host**.
        ///
        /// An **empty string means "the user's login shell"** — the daemon
        /// resolves it from its own `$SHELL` / passwd entry and spawns it
        /// directly as a login shell. This is the only correct way to ask for
        /// a shell: the client may be a different OS from the host, so a
        /// client-side path (a Windows Zed sending `/bin/sh`) is meaningless
        /// there. A non-empty string is run as `sh -lc 'exec <command>'`.
        command: String,
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
        agent_kind: String,
        instance_label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scrollback_bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    ListSessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// Ask for scrollback [`Frame::Replay`] followed by live [`Frame::Output`].
    Attach {
        session_id: SessionId,
        /// Which terminal view this client is the pty for, so
        /// [`Frame::FocusSession`] can name it. Opaque and never reused — a
        /// fresh id per view lifetime — so a focus left over from a view that
        /// is gone can never bind to a later one. Absent from an older client,
        /// and read as absent on a generation-2 connection however it arrives.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// Stop the output stream. Never kills the session.
    Detach {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    Write {
        session_id: SessionId,
        bytes: Vec<u8>,
    },
    Resize {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    /// Make one attached view's ask the pty's size, instead of the smallest
    /// ask among every attached client.
    ///
    /// **Carries no size on purpose.** The owner's ask is live: its attach
    /// client keeps sending [`Frame::Resize`] as its own terminal moves, and
    /// the daemon re-derives the pty size from whatever that last ask is. A
    /// size here would go stale the moment the focused view was dragged.
    ///
    /// Sent by the GUI over its control connection, not by the attach client
    /// that owns `view_id` — which is why the view is named rather than
    /// implied by the sender. A `view_id` nothing has attached with yet is
    /// remembered and takes effect when it does.
    ///
    /// **A generation-3 operation.** Generation 2 has no focus notion at all —
    /// its pty follows the last resize — so a receiver serving a gen-2
    /// connection refuses this with [`error_code::UNCAPABLE_PEER`] and keeps
    /// serving, and a sender must not emit it there.
    FocusSession {
        session_id: SessionId,
        view_id: String,
        /// A hover-born claim; the daemon may decline it while the session is
        /// being typed into (see the daemon's `SessionTable::focus`).
        #[serde(default)]
        hover: bool,
    },
    /// The only frame that ends a session.
    Kill {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// Opt in to the [`Frame::Status`] event stream for every session.
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// Ask the daemon to exit so its binary can be replaced.
    ///
    /// Honoured unforced only while the daemon is upgrade-ready (see
    /// [`HelloAck::upgrade_ready`]); a daemon holding a session with something
    /// running in it, or an exited session's last screen, answers
    /// [`Frame::Error`] and keeps serving. `force` skips that check — see the
    /// field. On success the daemon sends [`Frame::ShutdownAck`], unlinks its
    /// socket and exits.
    ///
    /// Idle shells go with it, and nothing about who else is connected is
    /// consulted: this frame is reached by a human asking for the upgrade from
    /// an app that is itself connected and busy with the daemon. Sessions that
    /// die with the process were already persisted, so they come back under
    /// the replacement daemon as lost rows the client answers by recreating
    /// their workspace. A daemon that predates this variant cannot decode it
    /// and drops the connection, which the client must treat as "declined".
    Shutdown {
        /// Skip the check entirely and exit over whatever is held.
        ///
        /// Set only when a human clicked "upgrade host daemon": the click is
        /// the consent, and a request the operator made must not be declined
        /// by the daemon's own idea of what is worth keeping. The sessions go
        /// with the process, exactly as they do for an accepted polite
        /// shutdown — they were persisted before they died, so they come back
        /// under the replacement daemon as lost rows and the client recreates
        /// their workspace.
        ///
        /// Absent on the wire — an older client — reads as `false`, which is
        /// the polite request this frame has always been.
        #[serde(default)]
        force: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },

    // ---- daemon → client ----
    HelloAck(HelloAck),
    SessionList {
        sessions: Vec<SessionInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    Created {
        session: SessionInfo,
        /// `false` when a degraded daemon applied the mutation in memory and
        /// could not record it (§8.5). It is not a failure and must never be
        /// retried — it happened, only its ledger row did not. Absent reads as
        /// `true`, which is what every peer without the field always meant.
        #[serde(
            default = "persisted_by_default",
            skip_serializing_if = "was_persisted"
        )]
        persisted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// A whole workspace: the answer to [`Frame::CreateWorkspace`] and to
    /// [`Frame::OpenWorkspace`], and the event announcing a new workspace to
    /// every other subscriber.
    Workspace {
        workspace: WorkspaceInfo,
        sessions: Vec<SessionInfo>,
        /// See [`Frame::Created::persisted`].
        #[serde(
            default = "persisted_by_default",
            skip_serializing_if = "was_persisted"
        )]
        persisted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    WorkspaceList {
        workspaces: Vec<WorkspaceInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// The accepted layout. Carries `request_id` back to the client that sent
    /// the [`Frame::UpdateLayout`], and goes out without one to every other
    /// subscriber.
    LayoutChanged {
        workspace_id: String,
        layout: LayoutDoc,
        rev: u64,
        /// See [`Frame::Created::persisted`].
        #[serde(
            default = "persisted_by_default",
            skip_serializing_if = "was_persisted"
        )]
        persisted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    WorkspaceRemoved {
        workspace_id: String,
        /// See [`Frame::Created::persisted`].
        #[serde(
            default = "persisted_by_default",
            skip_serializing_if = "was_persisted"
        )]
        persisted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    Removed {
        session_id: SessionId,
    },
    /// Scrollback replayed on attach; `truncated` when some of what the
    /// session printed is not in it — the buffer dropped older bytes, or the
    /// replay was cut to fit the connection's outbound bound.
    Replay {
        session_id: SessionId,
        bytes: Vec<u8>,
        truncated: bool,
    },
    Output {
        session_id: SessionId,
        bytes: Vec<u8>,
    },
    Status {
        session_id: SessionId,
        status: SessionStatus,
        /// Unix seconds the session entered `status`.
        since: u64,
    },
    Exited {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// The daemon accepted [`Frame::Shutdown`] and exits as soon as this frame
    /// is on the wire. The socket is gone by the time the client acts on it.
    ShutdownAck {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    /// A failure, named by a stable [`error_code`] and described in prose.
    ///
    /// Carries `request_id` when it answers a request. An error with **no**
    /// `request_id` is legal at any time — the daemon emits one when a write to
    /// a dead pty fails — and a client must route it to diagnostics rather than
    /// to a pending request (§2).
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        /// Machine-readable and stable; see [`error_code`]. Always set. A
        /// reader that does not recognise the code treats it as a generic
        /// failure and keeps the connection (§2.1).
        code: String,
        /// Human text. Never parsed.
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
}

impl Frame {
    /// The session this frame concerns, if any.
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Frame::Attach { session_id, .. }
            | Frame::Detach { session_id, .. }
            | Frame::Write { session_id, .. }
            | Frame::Resize { session_id, .. }
            | Frame::FocusSession { session_id, .. }
            | Frame::Kill { session_id, .. }
            | Frame::Removed { session_id }
            | Frame::Replay { session_id, .. }
            | Frame::Output { session_id, .. }
            | Frame::Status { session_id, .. }
            | Frame::Exited { session_id, .. } => Some(session_id),
            Frame::Created { session, .. } => Some(&session.id),
            Frame::Error { session_id, .. } => session_id.as_ref(),
            Frame::Hello(_)
            | Frame::HelloAck(_)
            | Frame::CreateSession { .. }
            | Frame::ListSessions { .. }
            | Frame::Subscribe { .. }
            | Frame::Shutdown { .. }
            | Frame::ShutdownAck { .. }
            | Frame::SessionList { .. }
            | Frame::CreateWorkspace { .. }
            | Frame::OpenWorkspace { .. }
            | Frame::ListWorkspaces { .. }
            | Frame::UpdateLayout { .. }
            | Frame::RenameWorkspace { .. }
            | Frame::KillWorkspace { .. }
            | Frame::Workspace { .. }
            | Frame::WorkspaceList { .. }
            | Frame::LayoutChanged { .. }
            | Frame::WorkspaceRemoved { .. } => None,
        }
    }

    /// The workspace this frame concerns, if any.
    pub fn workspace_id(&self) -> Option<&str> {
        match self {
            Frame::CreateSession { workspace_id, .. }
            | Frame::OpenWorkspace { workspace_id, .. }
            | Frame::UpdateLayout { workspace_id, .. }
            | Frame::RenameWorkspace { workspace_id, .. }
            | Frame::KillWorkspace { workspace_id, .. }
            | Frame::LayoutChanged { workspace_id, .. }
            | Frame::WorkspaceRemoved { workspace_id, .. } => Some(workspace_id),
            Frame::Workspace { workspace, .. } => Some(&workspace.id),
            Frame::Error { workspace_id, .. } => workspace_id.as_deref(),
            _ => None,
        }
    }

    /// The correlation id, for frames that carry one.
    pub fn request_id(&self) -> Option<u64> {
        match self {
            Frame::Hello(hello) => hello.request_id,
            Frame::HelloAck(ack) => ack.request_id,
            Frame::CreateSession { request_id, .. }
            | Frame::ListSessions { request_id }
            | Frame::Attach { request_id, .. }
            | Frame::Detach { request_id, .. }
            | Frame::Kill { request_id, .. }
            | Frame::Subscribe { request_id }
            | Frame::Shutdown { request_id, .. }
            | Frame::ShutdownAck { request_id }
            | Frame::SessionList { request_id, .. }
            | Frame::Created { request_id, .. }
            | Frame::CreateWorkspace { request_id, .. }
            | Frame::OpenWorkspace { request_id, .. }
            | Frame::ListWorkspaces { request_id }
            | Frame::UpdateLayout { request_id, .. }
            | Frame::RenameWorkspace { request_id, .. }
            | Frame::KillWorkspace { request_id, .. }
            | Frame::Workspace { request_id, .. }
            | Frame::WorkspaceList { request_id, .. }
            | Frame::LayoutChanged { request_id, .. }
            | Frame::WorkspaceRemoved { request_id, .. }
            | Frame::Error { request_id, .. } => *request_id,
            Frame::Write { .. }
            | Frame::Resize { .. }
            | Frame::FocusSession { .. }
            | Frame::Removed { .. }
            | Frame::Replay { .. }
            | Frame::Output { .. }
            | Frame::Status { .. }
            | Frame::Exited { .. } => None,
        }
    }
}
