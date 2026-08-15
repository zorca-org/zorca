# ADE session-daemon protocol: compatibility contract

Status: **specification**, adopted 2026-08-09 after adversarial design review (Fable×Sol,
two rounds) and operator sign-off. Normative. The envelope, the generation/capability
negotiation, the request-scoped errors (§2–§7) and the `degraded` handshake flag shipped
with the compatibility-cut MR on 2026-08-09; §8's persist-before-ack contract (including
`persisted: false` acks) is not yet implemented.

Keywords MUST / MUST NOT / SHOULD / MAY are used in the RFC 2119 sense. "Peer" is either
end of a connection; "daemon" and "client" name the two roles. `DECISION:` marks a point
where a plausible alternative existed. All of them are settled: the markers are kept as a
record of the alternatives considered, and are no longer veto candidates.

Citations name the code **as it is today** — the shipped implementation of this contract —
except in §1 and §6's opening paragraph, which describe the pre-cut code the envelope
replaced. That code is only readable before commit `ce327c0ea9`, so those citations name
files and symbols rather than lines.

---

## 1. Problem statement

The wire is length-prefixed JSON (pre-cut `crates/ade_session/src/framing.rs`) decoding
directly into one internally tagged enum, `Frame` (pre-cut
`crates/ade_session/src/proto.rs`). Three consequences follow, and all three are defects:

1. **An unknown operation is a fatal decode error.** `read_frame` ends in
   `serde_json::from_slice` (pre-cut `read_frame`, `crates/ade_session/src/framing.rs`);
   serde has nowhere to put an unknown tag, so the whole frame fails. The receive loop
   treats any `recv` error as "the peer is gone" and breaks (pre-cut `serve_connection`,
   `crates/ade_session_daemon/src/server.rs`), so one unrecognised request kills the
   connection and every attach on it. This is already written down as the contract (the
   pre-cut module doc and `Frame::Shutdown`'s, `crates/ade_session/src/proto.rs`) — it is
   the thing being repealed.
2. **Version compatibility is exact equality.** `hello.protocol_version != PROTOCOL_VERSION`
   is rejected outright (pre-cut `handshake`, `crates/ade_session_daemon/src/server.rs`),
   so the version field can never be moved without a flag day, and therefore never is.
3. **A frame the daemon merely does not *expect* is already handled correctly**
   (`handle_frame`'s catch-all arm answers `Frame::Error`; post-cut,
   `crates/ade_session_daemon/src/server.rs:990`). That path is unreachable for anything
   the enum has not been compiled with — the failure happens one layer lower, in decode.

Everything below exists to move the failure boundary from the connection to the request.

---

## 2. The envelope

Every frame on the wire is a JSON object with three reserved top-level keys:

| key    | type              | required | meaning                                      |
| ------ | ----------------- | -------- | -------------------------------------------- |
| `op`   | string            | yes      | operation identifier                          |
| `rid`  | unsigned integer  | no       | request-correlation id                        |
| `body` | object            | yes      | operation payload; `{}` when the op has none  |

Rules:

- Decoding MUST be two-stage. Stage one decodes the envelope alone — `op`, `rid`, and
  `body` retained unparsed (`serde_json::value::RawValue`) — and MUST NOT depend on `op`.
  Stage two decodes `body` into the type for that `op`.
- A stage-one failure (not an object, missing `op`, missing `body`, `rid` not an unsigned
  integer) is a **protocol violation**: the receiver MUST reply with an error frame
  carrying `code = "malformed_frame"` and MAY then close the connection. Nothing else in
  this document permits closing a connection on a decode failure, with one exception: the
  handshake (§3), where any decode failure is fatal.
- An unknown `op`, or a `body` that fails stage two, is **request-scoped**: the receiver
  MUST reply with an error frame echoing `rid` (`code = "unknown_op"` /
  `"malformed_body"`) and — once the handshake is complete — MUST keep the connection
  serving. If such a frame carries no
  `rid`, the receiver MUST log and drop it: no reply could be correlated to anything, and
  the frame named no session to report the failure against.
- **The sender of a rejected frame MUST NOT treat the rejection as fatal either.** The
  keep-serving rule above has a mirror, and it is the half that is easy to miss: an
  `unknown_op` or `malformed_body` answer costs the one request it names, so a peer that
  ends its connection on one has rebuilt, from the other end, the defect this envelope
  repeals — one bad `resize` taking a live terminal down with it. Such a reply also names
  **no session**: it is generated before the body is understood, so there is no
  `session_id` to put in it. A receiver that routes error frames by session MUST NOT read
  that absent id as "this one is about the whole stream"
  (`error_code::is_request_scoped`, `crates/ade_session/src/proto.rs`, and `pump_output`,
  `crates/ade_session_daemon/src/attach.rs`). **One exception, and it is narrow:** a peer
  with exactly one frame outstanding — one frame sent that is owed a reply, and nothing
  else in flight that could be rejected — MAY read a request-scoped error it cannot match
  to a pending request as that frame's answer, because there is nothing else it can be
  about. That covers both shapes of unmatchable: an error carrying no `rid`, and one
  echoing a `rid` the peer is not waiting on — with a single request outstanding the
  second is a bug in the sender, and reading it as the answer beats waiting out a bound
  for a reply that has already been spent. That is the attach client between sending
  `attach` and its first reply: `attach` is the only frame it has sent, and the error
  frames it may send meanwhile are not requests and are never answered (bullet below), so
  it fails the attach on any error naming this session or naming none, rather than
  ignoring it (`await_replay`, `crates/ade_session_daemon/src/attach.rs`). The exception
  closes the moment a second frame is in flight — once that same client is also sending
  `write` and `resize`, an unmatchable rejection may name any of them, is the answer to
  nothing the client is waiting on, and MUST NOT end the terminal. It is an exception
  about *which one request* an error answers, never a licence to treat one as fatal.
  `malformed_frame` is deliberately **outside** this class — but not because the stream
  has become unreadable. The length prefix was read and the payload was consumed, so the
  stream is still in sync; the prefix that genuinely would desync is a transport failure,
  not a malformed frame (`ReadFrameError::Transport` vs `::MalformedFrame`,
  `crates/ade_session/src/framing.rs`). It is outside because it is attributable to
  nothing: `rid` lives in the envelope, so a frame whose envelope did not parse names no
  request, and the receiver can only answer it with `rid` absent (`rejection_frame`, same
  file). The layer that failed is also the one every frame from that peer shares, so
  nothing about the failure is confined to the operation it attempted. Hence closing is a
  MAY and not a MUST, and both readings ship and conform: the attach client keeps serving
  past one (`await_replay` and `pump_output`, `crates/ade_session_daemon/src/attach.rs`),
  and `DaemonBackend` closes on one (`DaemonConnection::recv_decodable`,
  `crates/ade_workspaces/src/daemon_backend.rs`). §3.3's `uncapable_peer` is inside the
  class — that receiver keeps serving too.
- Unknown top-level keys MUST be ignored. `op`, `rid`, and `body` are reserved forever; no
  future extension may give them a second meaning.
- `op` identifiers are `snake_case` ASCII (`[a-z0-9_]`, ≤ 64 bytes). They are permanent:
  once shipped, an `op` string MUST NOT be reused for a different operation, even after
  the original is retired.
- `rid` is chosen by the requester, unique among that connection's in-flight requests.
  Every direct reply — success or error — MUST echo it, **except for the four answers
  below, whose frames carry no `rid` field on the wire at all.** That list is closed:
  every other reply echoes. For the ops on it a requester MUST NOT wait for a rid-echoing
  reply — a peer that does hangs on a correct daemon, or spends the give-up bound at the
  end of this section on an answer that was never going to carry one — and MUST correlate
  as each entry says instead.
  - **`kill` → a rid-less `removed`.** The direct reply to a successful `kill` is a
    `removed` frame, identical in shape to the broadcast (`handle_frame`'s `Frame::Kill`
    arm, `crates/ade_session_daemon/src/server.rs:934`). That same `removed` is published to
    **two independent hubs, with no cross-dedup between them or with the direct reply**,
    both inside `SessionTable::remove_session`: the event hub, reaching every connection
    that sent `subscribe` (`crates/ade_session_daemon/src/sessions.rs:1487`), and the
    killed session's own output hub, reaching every connection attached to *that* session
    (`crates/ade_session_daemon/src/sessions.rs:1493`). So one kill puts **three**
    `removed` frames on a connection that is subscribed **and** attached to the session
    **and** issued the kill, and **two** on any connection that is registered with both
    hubs without having issued it. The requester MUST correlate by `session_id`, not
    `rid`, and MUST tolerate the repeats (`DaemonBackend::kill_daemon_session`,
    `crates/ade_workspaces/src/daemon_backend.rs`).
  - **`attach` → a rid-less `replay`, on success only.** `attach` carries a `rid`, but the
    frame that answers it has no correlation field: on success `SessionTable::attach`
    queues a `Frame::Replay` on the connection's outbound ahead of the live `output`
    (`crates/ade_session_daemon/src/sessions.rs:519`) and `handle_frame` returns nothing
    (its `Frame::Attach` arm, `crates/ade_session_daemon/src/server.rs:937`, and the
    function's own doc at `:776`). The requester MUST correlate the `replay` by
    `session_id`, exactly as it does `removed` (`await_replay`,
    `crates/ade_session_daemon/src/attach.rs`). Only the **failure** path echoes `rid`,
    as an ordinary error frame — so `attach` is answered by *either* a rid-less `replay`
    or a rid-bearing error, and a requester MUST accept both as the answer to it.
  - **`subscribe` → the rid-less `status` snapshot.** There is no `subscribe` ack. The
    answer is one `status` frame per session, queued while the registration is in place
    (`SessionTable::subscribe`, `crates/ade_session_daemon/src/sessions.rs:1275`), and a
    daemon with no sessions therefore answers with **nothing at all** — which is the
    honest snapshot of an empty table. A requester MUST NOT block on a reply to
    `subscribe`.
  - **`detach` → nothing at all, ever.** The protocol has no detach ack, and detaching
    something that was never attached is a no-op rather than an error
    (`handle_frame`'s `Frame::Detach` arm, `crates/ade_session_daemon/src/server.rs:944`).
    The frame MAY carry a `rid`; nothing will ever echo it.

  Unsolicited events (`output`, `status`, `exited`, `removed`, and the broadcast half of
  `layout_changed` / `workspace_removed`) MUST omit `rid` as well, as they do today —
  they and the frames above are the whole of `Frame::request_id`'s `None` arm
  (`crates/ade_session/src/proto.rs:925`).
- **Fire-and-forget ops (`write`, `resize`) carry no `rid` at all, and a sender MUST NOT
  attach one.** This is not "MAY omit": the two ops have no correlation field, a
  receiver's stage two silently drops any `rid` sent on them as an unknown field rather
  than rejecting the frame (`decode_frame`'s rid-injection comment,
  `crates/ade_session/src/framing.rs:330`), and their failure reply is rid-less by
  construction (`handle_frame`'s `Frame::Write` and `Frame::Resize` arms,
  `crates/ade_session_daemon/src/server.rs:952` and `:956`). A sender that attaches a
  `rid` to a `write` and expects the failure to come back under it waits for something the
  wire cannot carry. **Unsolicited error frames — an error carrying no `rid` at all —
  remain legal**: the daemon already emits one when a write fails
  (`crates/ade_session_daemon/src/server.rs:954`). A client MUST tolerate an error frame
  without `rid` at any time, and MUST route it to diagnostics, never to a
  pending request. This is a different case from the bullet above: here the daemon has
  something to say and names the session it concerns; there it has nobody to answer.
- **A received error frame is never a request, and MUST NOT be answered.** A receiver
  matches it to a pending request, or logs it, and replies nothing — not `invalid_argument`,
  not `unknown_op`, nothing. Two peers that each answered the other's unexpected frame with
  an error frame would echo one back and forth with nothing in the protocol to stop them,
  and the concrete case is already in this section: the attach client answers a frame it
  could not decode with an error frame of its own, so a daemon that took that for a request
  would reply `invalid_argument` — not request-scoped, therefore fatal to the client's
  output pump, the survivability net undoing itself. The arm that drops it therefore sits
  **ahead** of the catch-all that answers "unexpected frame from a client"
  (`handle_frame`'s `Frame::Error` arm, `crates/ade_session_daemon/src/server.rs:986`,
  before `:990`).
- **A requester MAY give up on a pending request, and MUST close the connection if it
  does.** Once a peer has received a frame that cannot be the reply it is waiting for — an
  unsolicited error, an error echoing another `rid`, or a frame it could not decode, and
  not one the one-frame-outstanding exception above has already claimed as the reply — the
  answer to that request has been spent, and waiting past it is otherwise waiting forever.
  A requester MAY therefore abandon the request after a bounded wait, and MUST then drop
  the connection rather than reuse it: a read abandoned mid-frame leaves the stream out of
  sync with its own next length prefix, so every later read on it is a guess. Dropping it
  is a detach and MUST NOT affect sessions (§7). The bound SHOULD start at that frame and
  not at the request — a peer that has merely said nothing yet is not misbehaving, and §8's
  persist-before-ack can legitimately put an fsync in front of an ack, so silence alone is
  not evidence of anything (`ANSWER_TIMEOUT`, `DaemonBackend::request` and
  `DaemonConnection::receive`, `crates/ade_workspaces/src/daemon_backend.rs`).
  **The bound is only as real as the mechanism that fires it, and an implementation MUST
  NOT assume it has one.** Against a peer that keeps talking, re-reading the clock before
  each read is enough. Against a peer that goes silent after the clock is armed there is
  no next read to check it against, so the bound rests entirely on a scheduled wakeup —
  and a wakeup scheduled on a resource the rest of the process can exhaust (a bounded
  worker pool that also carries work which may never return) is not guaranteed to fire.
  An implementation SHOULD schedule it on something that cannot be starved by unrelated
  work; one that cannot MUST record that its bound holds only while that resource is
  available, because a documented bound the runtime does not enforce is not a bound.

> DECISION: reserved keys are `op`/`rid`/`body`, not the current `type`/`request_id` with
> fields inline. Rationale: a nested `body` is what makes stage-one decoding independent of
> the operation at all; distinct key names also make a pre-cut peer fail loudly at the
> first frame instead of decoding a half-recognised object.

### 2.1 Error frames

The error op carries `code` (stable machine-readable string), `message` (human text, never
parsed), and optionally `session_id` / `workspace_id`. Codes defined at generation 2:
`malformed_frame`, `malformed_body`, `unknown_op`, `unsupported_generation`,
`uncapable_peer`, `not_found`, `stale_rev`, `invalid_argument`, `persist_failed`,
`declined`, `internal` (a failure inside the daemon — a spawn or io error the requester
did not cause). New codes MAY be added at any generation; a reader MUST treat an unrecognised
code as a generic failure and MUST NOT close the connection over one.

> DECISION: errors carry a code. Rationale: "not implemented" and "failed" are
> operationally different answers and a sender must be able to tell them apart without
> string-matching prose.

---

## 3. Handshake: generations and capabilities

`hello` and `hello_ack` replace the single `protocol_version` (`Hello` and `HelloAck`,
`crates/ade_session/src/proto.rs:511` and `:536`; the pre-cut `PROTOCOL_VERSION` constant
is gone, not deprecated).

**hello** body: `min_generation`, `max_generation` (unsigned), `capabilities` (array of
strings), plus the existing client-identifying fields.

**hello_ack** body: `min_generation`, `max_generation`, `generation` (the selected one),
`capabilities`, `degraded` (boolean — the ledger is read-only for this daemon, see §8.5),
plus today's `daemon_version`, `protocol_version`, `host_os`, `binary_hash`,
`upgrade_ready` (`HelloAck`, `crates/ade_session/src/proto.rs:544`).

Before `hello_ack` there is no negotiated connection to preserve. A first frame that fails
to decode — at either stage — is answered per §2's reply rules and the connection is then
closed: §2's request-scoped/connection-fatal distinction begins only once the handshake
has completed (`handshake`'s decode-failure arm,
`crates/ade_session_daemon/src/server.rs:685`). A first frame that decodes
but is not `hello` is answered `invalid_argument` and closed the same way.

### 3.1 Generation selection

- The **daemon** selects `G = min(client.max_generation, daemon.max_generation)`.
- If `G < max(client.min_generation, daemon.min_generation)`, the daemon MUST reply with an
  error frame, `code = "unsupported_generation"`, echoing `rid`, and MUST close the
  connection. This is the one negotiation outcome that is fatal by design.
- Otherwise the daemon echoes `G` in `hello_ack.generation` and serves at `G`.
- The client MUST verify `G` lies inside its own range and MUST close the connection with a
  legible error if it does not. `Connection::handshake` does exactly that, naming both
  ranges in the failure; everything else in the ack — `degraded`, `capabilities`,
  `upgrade_ready` — is returned unexamined and stays caller policy
  (`crates/ade_session/src/client.rs:78`).

> DECISION: the daemon selects and the client verifies, rather than both computing
> independently. Rationale: one authority means one place to read when a mismatch is
> reported, and the client already has the ack in hand.

`protocol_version` stays in `hello_ack` as a legacy informational field equal to `G`. It is
never used for a compatibility decision again.

### 3.2 Capabilities

- A capability identifier is `[a-z0-9_.-]`, ≤ 64 bytes. A peer MUST NOT advertise more than
  256. A list exceeding either bound is `invalid_argument` and fatal to the handshake.
- **Duplicates within one peer's list are deduplicated, not rejected.** A repeated
  identifier still says the peer is capable.
- **Unknown identifiers are ignored, never an error.** A receiver simply does not have them
  in its own list, so they fall out of the intersection.
- The **effective set** is the intersection of the two advertised sets. Both peers compute
  it; both compute the same thing.
- The effective set is fixed for the life of the connection. There is no re-negotiation
  frame; changing it means reconnecting.

> DECISION: duplicates are deduplicated. Rationale: a duplicate is an encoding artefact
> with no ambiguous reading, and killing a handshake over one trades a real connection for
> a cosmetic complaint.

### 3.3 Downgrade rules — what each side may use afterwards

After the handshake, at generation `G` with effective capability set `C`, a peer:

- MUST NOT send an `op` introduced above `G`.
- MUST NOT send an `op` gated by a capability not in `C`.
- MUST NOT send a field introduced above `G`, on any frame.
- MUST NOT require, in anything it sends, behaviour that only a peer above `G` provides.
- MUST accept everything defined at or below `G` for the whole connection, including ops it
  would not itself choose to send.
- MUST NOT infer capability from receipt: being *sent* a capability-gated op does not
  license replying in kind if that capability is absent from `C`. (A conforming peer will
  not send one; a buggy one must not bootstrap itself out of the contract.)

A peer that receives a capability-gated op outside `C` MUST answer `uncapable_peer` and
keep serving.

---

## 4. Additive fields

- Unknown fields on a known `op` MUST be ignored. This is serde's default and it is now
  contract, not incident: `#[serde(deny_unknown_fields)]` MUST NOT appear on any wire type
  (already asserted by the module's evolution rule,
  `crates/ade_session/src/proto.rs:15`).
- Every field added after the generation in which its op shipped MUST be `Option` or
  `#[serde(default)]`, and its **absent** meaning MUST be documented on the field. The
  existing `Shutdown::force` is the model: absent reads as `false`, which is the behaviour
  the frame always had (`Frame::Shutdown::force`, `crates/ade_session/src/proto.rs:772`).
- A reader MUST NOT require a field newer than `G`. A reader negotiated at `G` MUST behave,
  for any field introduced above `G`, exactly as if the field were absent — even if the
  peer sent it anyway.
- A sender MUST NOT omit a field that is required at `G`.
- Removing a field is a generation bump. Changing the meaning or type of an existing field
  is a generation bump. Widening a value set (a new enum variant in a payload) requires
  either a capability or a documented fallback for readers that do not know the variant.
- `LAYOUT_SCHEMA_VERSION` (`crates/ade_session/src/proto.rs:310`) and `STATE_VERSION`
  (`crates/ade_session_daemon/src/state.rs:31`) are **document** versions and are
  independent of the protocol generation. A generation bump does not imply either.

---

## 5. New operations

- A new op ships **behind a capability**, in the same release that defines the capability
  identifier. A sender MUST NOT emit it to a peer whose effective set lacks that
  capability; the sender's job is to have a working fallback or to disable the feature, not
  to try and handle the error.
- The generation gates the **envelope and framing shape**. Capabilities gate **operations
  and optional behaviours**. A new op therefore does *not* move the generation — the same
  split that `crates/ade_session/src/proto.rs:20` already argues for, now with a mechanism
  that makes it survivable.
- Capability identifiers are **permanent once shipped**: never reused for a different
  meaning, never recycled after a feature is withdrawn. A capability that has become
  universal MAY be advertised unconditionally forever; it MUST NOT be quietly dropped from
  the advertised list, because a peer that has not upgraded still tests for it.
- Unknown-op handling (§2) is the safety net, not the plan. Reaching it means a sender
  violated this section.

---

## 6. The one-time compatibility cut

This envelope is not backward compatible: a pre-cut daemon decoding `{"op":"hello",…}`
fails in `serde_json::from_slice` (pre-cut `read_frame`,
`crates/ade_session/src/framing.rs`) and its receive loop breaks the connection (pre-cut
`serve_connection`, `crates/ade_session_daemon/src/server.rs`) without sending anything.
**This is the last such break.** Every change after it goes through §3–§5. The
envelope ships as **generation 2**; generation 1 is retroactively the pre-cut protocol, and
is never advertised.

### 6.1 Observable failure

A post-cut client whose handshake ends in EOF with no reply MUST retry the handshake once,
after a short delay. A second identical EOF-with-no-bytes MUST be reported as "the daemon
on this host most likely predates the protocol cut and should be replaced", not as a
generic IO error. The desktop connect flow MUST ask before replacing it, state that every
session it owns will terminate, and proceed only after explicit confirmation. It MUST NOT
fall back to a competing plain terminal while that daemon may still own an agent writer.

> DECISION: no legacy probe. The client does not attempt a pre-cut `Hello` to sniff or use
> the daemon's old protocol. The one-retry rule filters transient EOFs; explicit consent to
> replace the daemon resolves the incompatible case without a permanent second codec.

### 6.2 Migration cost, precisely

Replacing a host's daemon is already gated, and the cut inherits those gates unchanged:

- **Polite path.** The client sends `shutdown`. The daemon accepts only while
  `SessionTable::expendable()` holds — every row is a lost tombstone or an idle shell
  (`crates/ade_session_daemon/src/sessions.rs:822`, decision at
  `crates/ade_session_daemon/src/server.rs:571`). Anything `Working` or `NeedsInput`, and
  any exited-but-unkilled row whose last screen exists only in that process, declines the
  shutdown. So the client **waits**: the upgrade lands when the user's terminals are quiet,
  and not before.
- **Forced path.** `shutdown { force: true }` skips the check (`Frame::Shutdown::force`,
  `crates/ade_session/src/proto.rs:772`; `crates/ade_session_daemon/src/server.rs:572`).
  Live PTYs die with the process. This is reachable only from an explicit human action —
  the click is the consent — and the cost is stated at the click: running agents are
  terminated.
- **Forced path, across the cut.** A pre-cut daemon cannot receive `shutdown` at all: the
  handshake the frame rides is what the cut broke, so it fails with §6.1's diagnosis. The
  confirmed forced path — and only that path — then terminates the daemon out of band over
  ssh: its own pidfile, `kill` (TERM then KILL), and the socket file removed so the
  deployment guard below opens (`HostLink::kill_pre_cut_daemon`,
  `crates/ade_workspaces/src/daemon_backend.rs`). The script never signals a pid whose
  command name is not `ade-daemon`. The unforced connect-time path keeps reporting the
  diagnosis instead — nothing without a human's click may hard-kill a daemon.
- **Either way**, persisted rows return under the replacement daemon as *lost* and the
  client's reconcile pass recreates their workspaces (`SessionTable::load`,
  `crates/ade_session_daemon/src/sessions.rs:637`; `StateStore::load`,
  `crates/ade_session_daemon/src/state.rs:177`).
- **Binary deployment** additionally refuses to overwrite the daemon binary while a socket
  file exists (`replace_daemon`, `crates/ade_session/src/deploy.rs:387`), so the
  replacement is ordered shutdown-then-deploy, not the reverse.

### 6.3 Scope of the cut

- The `--stdio-proxy` pump copies bytes and never parses a frame (`pump`,
  `crates/ade_session_daemon/src/proxy.rs:270`), so the envelope does not touch it.
- `--ensure` **does** handshake (`ensure`, `crates/ade_session_daemon/src/proxy.rs:147`).
  It is the same binary as the daemon on that host, so it is cut and replaced in the same
  step; no separate migration exists.
- **pydaemon** (the Python twin) is inside the cut and MUST ship the envelope, the
  generation range and the capability set together with the Rust daemon. Until it does, a
  post-cut client cannot talk to it. See §7.

---

## 7. Fixed constraints

These are inputs to the design, not consequences of it.

- **The socket path stays stable and versionless.** `~/.ade/daemon.sock`
  (`DEFAULT_SOCKET_PATH`, `crates/ade_session/src/deploy.rs:44`). No version suffix, no
  per-generation socket. Two implementations — the Rust daemon and pydaemon — bind the
  same path and whoever binds
  first wins; the identity of the winner is reported in `hello_ack.daemon_version`, which
  is the only supported way to tell them apart. Versioning the path would silently split
  the two into non-coexisting daemons and break exactly the workflow the twin exists for.
- **The wire stays length-prefixed JSON.** 4-byte big-endian length
  (`LENGTH_PREFIX_BYTES`, `crates/ade_session/src/framing.rs:45`), payload capped at
  `MAX_FRAME_BYTES` (`crates/ade_session/src/framing.rs:42`), length validated before
  allocation. No alternative encoding is negotiable — there is no `encoding` capability,
  now or later.
- **Detach never kills** stays true of every path here: an unknown op, a failed
  negotiation, or a closed connection detaches and nothing more (`serve_connection`'s
  `detach_all`, `crates/ade_session_daemon/src/server.rs:625`).

---

## 8. Persist-before-ack contract for mutations

### 8.1 The contract

> A success ack for a mutating request implies the mutation is durably recorded. A failure
> to record it MUST be reported to the requester as an error; it MUST NOT be reported as
> success.

Today the ordering is still the opposite, even though the write itself is no longer the
weak link: PR #15 (2026-08-09) made the ledger write durable. What remains is
ack-before-persist. `update_layout` mutates, publishes, then logs a warning if the write
fails (`crates/ade_session_daemon/src/sessions.rs:1144`, `:1147`, `:1159`);
`rename_workspace` (`:1181`), `kill_workspace` (`:1236`) and `kill` — through
`remove_session` (`:1516`) — do the same. The client is told the operation succeeded and
the ledger may disagree.

**Durability** means: `write_atomic` returned `Ok` — the temp file was written, its
contents were fsynced, the atomic rename completed, and on Unix the parent directory was
fsynced too (`crates/ade_session_daemon/src/state.rs:274`, shipped in PR #15).

> DECISION: durability includes the fsync chain (tmp → fsync → rename → dirsync), as
> settled contract v1.1 point 6 mandates and `write_atomic` implements. The lighter
> "rename returned" definition was proposed and rejected at review 2026-08-09: it
> contradicted the shipped code, and saved one syscall on a path that §8.2 moves off the
> hot path anyway.

### 8.2 Scheduling

Persistence MUST NOT run on the PTY drain path or inline under a request lock. It runs on a
single **persist worker**. The worker **replaces** the interim `persisting: Mutex<()>`
introduced by PR #15: there is exactly one serialization point for ledger writes, snapshots
are taken under the table locks at enqueue, and FIFO order makes a stale snapshot
overwriting a newer one impossible by construction rather than by lock discipline.
Contract:

- One worker, FIFO. Jobs are applied in the order the mutations were applied.
- Each job carries a state snapshot taken **at enqueue time, under the same lock that made
  the mutation** — not read at write time. `persist()` snapshots the whole table when it
  runs (`SessionTable::persist`, `crates/ade_session_daemon/src/sessions.rs:1647`); moving
  it off-thread without
  moving the snapshot would let a late job write an older state over a newer one.
- Consecutive pending jobs MAY be coalesced into the newest snapshot. Every waiter on a
  coalesced job resolves on the newest job's result — success for all, or `persist_failed`
  for all. Coalescing across class-B jobs (§8.3) makes the batch **atomic**: on failure the
  rollback restores, in one table-locked operation, the state as it stood before the
  **oldest** coalesced job, and every waiter resolves `persist_failed`. The per-request
  conditional rollbacks of §8.3 MUST NOT run after a coalesced failure — interleaved with
  the batch rollback they can settle on a state that matches no request. Class B publishes
  only after persist succeeds, so a failed batch has no published events to compensate.
- The ack awaits the job's result. Per-connection frame order is unaffected: the outbound
  queue is FIFO and written by one task (`serve_connection`'s writer task,
  `crates/ade_session_daemon/src/server.rs:488`) and `rid` correlates the
  reply regardless of latency.

### 8.3 Already-published events when persist fails

This is the hard case, and it does not have one answer, because mutations differ in whether
the world can be put back. Three classes:

**Class A — irreversible world change: `kill`, `kill_workspace`, `shutdown`.**
The child has been signalled (`crates/ade_session_daemon/src/sessions.rs:1502`) and an
attached client must be told immediately or it waits forever on a dead PTY (the output
hub's `publish_event`, `crates/ade_session_daemon/src/sessions.rs:1493`). Contract:

- Events (`removed`, the hub event, `workspace_removed`, the `layout_changed` from
  `scrub_layout` at `crates/ade_session_daemon/src/sessions.rs:1548`) publish **immediately**
  and unconditionally. They describe reality and reality does not roll back.
- The **ack** still awaits persistence. On failure the requester receives an error frame
  with `code = "persist_failed"`, and the published events **stand**.
- The meaning of that error is precisely *"this happened and the daemon could not record
  it"*. The client MUST NOT attempt to undo it. The recovery is already defined: after a
  restart the unpersisted removal reappears as a stale row, load prunes it from the layout
  (`SessionTable::load`, `crates/ade_session_daemon/src/sessions.rs:712`) and the reconcile
  pass settles it.

> DECISION: for class A, events are not withheld and are not compensated. Rationale:
> withholding `removed` behind a disk write risks a hung attached terminal, and a
> compensating "un-removed" event would be a lie about a process that is already dead. The
> ack is the only thing that can honestly carry the failure, so it is the only thing that
> does.

**Class B — record-only mutation: `update_layout`, `rename_workspace`.**
Nothing outside the daemon's own memory changed, so ordering can be strict. Contract:

- Apply under the lock, **persist, then** publish the broadcast event and send the ack.
- On persist failure: **roll back** the in-memory mutation, publish nothing, reply
  `persist_failed`. Nothing was ever observed, so the rollback is invisible.
- Rollback is conditional: restore the previous value only if this request's write is
  still the one the record holds. The condition MUST be a strictly monotonic per-record
  revision, compared under the table lock — never a value-equality check. Value equality
  is ABA-racy: A writes Y, B writes Z, C writes Y again, and A's late rollback clobbers C.
  `layout_rev` already qualifies (`crates/ade_session_daemon/src/sessions.rs:1131` rejects
  any `rev <= layout_rev`, so it only ever moves forward). `rename_workspace` has no
  revision today (`crates/ade_session_daemon/src/sessions.rs:1181`) and gains a `meta_rev`,
  bumped on every accepted rename. If a later accepted write has superseded the record,
  that write owns it and its own persist result — leave it alone, and still fail this
  request.

> DECISION: roll back rather than publish-then-compensate. Rationale: the broadcast is the
> only thing that makes a class-B mutation observable, and it has not gone out yet; a
> rollback before publication is total and silent, whereas a compensating `layout_changed`
> would move `layout_rev` again and force every other client through a spurious round.

**Class C — creation: `create_session`, `create_workspace`.**
A PTY exists before it can be persisted, but unlike class A it holds nothing the user has
put there yet. Contract:

- Spawn, persist, then ack `created` / `workspace`.
- On persist failure the daemon **kills the just-created session**, publishes `removed`,
  and replies `persist_failed`. The requester never received a success ack, and no
  subscriber ever saw a `created` for it.
- The workspace case is the same shape and the same reasoning: on persist failure for
  `create_workspace` the daemon removes the just-created workspace record and publishes
  `workspace_removed`. A workspace absent from the ledger cannot be described after a
  restart, and nothing the user put there exists yet.

> DECISION: compensate by killing rather than keeping an unpersisted live session.
> Rationale: a live session absent from the ledger is exactly the state that cannot be
> described after a restart, and the compensation costs a session that is milliseconds old
> and empty. The alternative — keep it, ack success, warn — is today's behaviour and is
> vetoable, at the price of the invariant.

### 8.4 What never persists

`write`, `resize`, `attach`, `detach`, `subscribe`, and every `output` / `status` /
`exited` event are non-mutating with respect to the ledger and MUST NOT enqueue a persist
job. `Frame::Status` is deliberately not persisted at all (`PersistedSession`,
`crates/ade_session_daemon/src/state.rs:47`).

### 8.5 Degraded mode

PR #15 introduced degraded persistence: a daemon that finds a ledger written by a **newer**
schema version serves normally but treats that ledger as read-only — a rewrite would
silently destroy fields the newer schema owns — so `save()` becomes a no-op returning
`Ok(())`. Untreated, that breaks §8.1's contract in the quietest possible way: the daemon
would ack a durability it does not provide. Contract:

- The daemon MUST advertise `degraded: true` in `hello_ack` (§3).
- In degraded mode, every mutating request that would persist is answered with a **success**
  ack carrying `persisted: false`. The mutation applies in memory and its events publish
  normally — class-A semantics (§8.3), generalized to every class.
- A client MUST NOT retry a `persisted: false` ack as if the mutation had failed. It
  happened; only its record did not.
- A client SHOULD surface degraded state to the user: work in these sessions will not
  survive a daemon restart.
- Degraded mode ends when a daemon at least as new as the ledger's writer runs on that
  host. Nothing else clears it, and no frame can.

> DECISION (operator, 2026-08-09): success-with-`persisted: false`, not an error ack and
> not refusal. An error ack invites a retry and therefore a duplicate mutation; refusing
> mutations makes the host read-only during exactly the version mix-up the mode exists to
> survive. The flag keeps sessions usable while never claiming a durability the daemon does
> not have.

---

## 9. Summary of what changes

| Area                   | Today                                                | Contract                                        |
| ---------------------- | ---------------------------------------------------- | ----------------------------------------------- |
| Unknown operation      | decode error → connection dies (pre-cut `framing.rs`, `server.rs`) | request-scoped `unknown_op` error  |
| Version check          | exact equality (pre-cut `server.rs`)                 | generation range + capability intersection      |
| Unknown fields         | ignored by accident                                  | ignored by contract                             |
| New operation          | may kill an old peer's connection (pre-cut `proto.rs`) | capability-gated, never sent to an uncapable peer |
| Mutation ack           | success before persist; failure is a log line        | ack after persist; failure is an error frame; degraded mode acks carry `persisted: false` |
| Degraded (newer ledger)| silent no-op save, acks look normal                  | advertised in `hello_ack`; acks carry `persisted: false` |
| Socket path / encoding | `~/.ade/daemon.sock`, length-prefixed JSON           | unchanged, permanently                          |
