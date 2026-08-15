//! Length-prefixed serde-JSON framing, over the protocol envelope.
//!
//! Each frame is a 4-byte big-endian payload length followed by exactly that
//! many bytes of JSON encoding one **envelope**:
//!
//! ```text
//! {"op": "create_session", "rid": 7, "body": { … }}
//! ```
//!
//! `op`, `rid` and `body` are reserved forever; `rid` is omitted when the frame
//! carries no correlation id. The length is validated against
//! [`MAX_FRAME_BYTES`] *before* any buffer is allocated, so a hostile or
//! corrupt prefix cannot make the reader allocate.
//!
//! The in-memory [`Frame`] is still one internally tagged enum, and this module
//! is the whole of the transform between the two shapes: encode pops the `type`
//! tag into `op` and the `request_id` field into `rid`, decode puts them back.
//! Doing it here rather than with a hand-written `Serialize` per variant means
//! a new variant costs nothing and cannot forget the envelope.
//!
//! **Decoding is two-stage, and that is the point of the whole design**
//! (`docs/ade/protocol-compatibility.md` §2). Stage one parses the envelope
//! alone and must not depend on `op`; its failure is a protocol violation the
//! receiver may close over. Stage two parses `body` for that `op`; its failure
//! — and an `op` this build does not implement — is **request-scoped**, so the
//! connection and every attach riding on it survive one bad frame.
//!
//! Runtime-agnostic: these operate on `futures::io::{AsyncRead, AsyncWrite}`,
//! not on any executor's own IO traits.

use anyhow::{Context as _, Result, bail};
use futures::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proto::{Frame, KNOWN_OPS, error_code};

/// Largest JSON payload a single frame may carry, in bytes.
///
/// Terminal output is chunked well below this by the daemon; the cap exists so
/// a bad length prefix is an error rather than an allocation.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Bytes of the length prefix that precedes every payload.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// Longest `op` the envelope grammar allows: `[a-z0-9_]`, at most 64 bytes
/// (`docs/ade/protocol-compatibility.md` §2).
pub const MAX_OP_BYTES: usize = 64;

/// The most peer-supplied text that may appear inside an error `message`.
///
/// An error frame is itself a frame, and [`write_frame`] enforces
/// [`MAX_FRAME_BYTES`] on it — so interpolating something the peer chose the
/// length of makes the *reply* bigger than the request that caused it. A reply
/// that cannot be written breaks the writer task and takes the connection, and
/// every attach on it, with it. Nothing peer-derived reaches a message unbounded.
pub const MAX_QUOTED_BYTES: usize = 128;

/// `text`, cut to [`MAX_QUOTED_BYTES`] on a character boundary, with an ellipsis
/// when anything was dropped.
pub fn bounded(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= MAX_QUOTED_BYTES {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut cut = MAX_QUOTED_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    std::borrow::Cow::Owned(format!("{}…", &text[..cut]))
}

/// A value's `Debug` rendering, bounded the same way.
///
/// It *stops* the formatter at the bound rather than truncating afterwards, so
/// Debug-formatting a 16 MB `output` frame never allocates the 20 MB string it
/// would then throw away. `write_str` returning an error is how a `fmt::Write`
/// sink says "no more", and every derived `Debug` propagates it.
pub fn bounded_debug(value: &dyn std::fmt::Debug) -> String {
    use std::fmt::Write as _;

    struct Sink {
        out: String,
    }
    impl std::fmt::Write for Sink {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let room = MAX_QUOTED_BYTES - self.out.len();
            if text.len() <= room {
                self.out.push_str(text);
                return Ok(());
            }
            let mut cut = room;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            self.out.push_str(&text[..cut]);
            Err(std::fmt::Error)
        }
    }

    let mut sink = Sink { out: String::new() };
    if write!(sink, "{value:?}").is_err() {
        sink.out.push('…');
    }
    sink.out
}

/// Whether `op` is inside the envelope grammar: `[a-z0-9_]`, non-empty, at most
/// [`MAX_OP_BYTES`] bytes (§2).
fn op_is_well_formed(op: &str) -> bool {
    !op.is_empty()
        && op.len() <= MAX_OP_BYTES
        && op
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// The envelope's reserved keys are `op`, `rid` and `body` — spelled once each,
/// in [`Envelope`] and [`OutgoingEnvelope`] below. They are reserved forever; no
/// future extension may give them a second meaning.
///
/// The in-memory enum's internal tag, which becomes `op` on the wire.
const TAG_KEY: &str = "type";

/// The in-memory correlation field, which becomes `rid` on the wire.
const REQUEST_ID_KEY: &str = "request_id";

/// What a frame failed to be.
///
/// The split is the contract, not a convenience: [`ReadFrameError::Transport`]
/// is fatal to the connection, [`ReadFrameError::MalformedFrame`] *may* be, and
/// the last two are scoped to one request and must not be. A receive loop that
/// treats every error as "the peer is gone" is the defect this taxonomy exists
/// to make impossible to write by accident.
#[derive(Debug)]
pub enum ReadFrameError {
    /// EOF, an IO error, or a length prefix past [`MAX_FRAME_BYTES`]. Nothing
    /// can be read after one of these; the connection is over.
    Transport(anyhow::Error),
    /// Stage one failed: the payload is not an object, has no `op`, has no
    /// `body`, `body` is not an object, or `rid` is not an unsigned integer.
    /// The receiver replies `malformed_frame` and MAY close.
    MalformedFrame { detail: String },
    /// An `op` this build does not implement. Request-scoped.
    UnknownOp { op: String, rid: Option<u64> },
    /// A known `op` whose `body` did not decode. Request-scoped.
    MalformedBody {
        op: String,
        rid: Option<u64>,
        detail: String,
    },
}

impl std::fmt::Display for ReadFrameError {
    /// Everything interpolated here except the transport error is text the peer
    /// chose, and this string becomes an error frame's `message` by way of
    /// [`rejection_frame`] — so every piece of it goes through [`bounded`]
    /// first. See [`MAX_QUOTED_BYTES`] for why a reply that quotes its request
    /// in full is a way to lose the connection.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "{error:#}"),
            Self::MalformedFrame { detail } => write!(f, "malformed frame: {}", bounded(detail)),
            Self::UnknownOp { op, .. } => write!(f, "unknown operation {:?}", bounded(op)),
            Self::MalformedBody { op, detail, .. } => {
                write!(
                    f,
                    "malformed body for operation {:?}: {}",
                    bounded(op),
                    bounded(detail)
                )
            }
        }
    }
}

impl std::error::Error for ReadFrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // `anyhow::Error` is not itself an `Error`, but it derefs to one,
            // and that is what carries the `std::io::ErrorKind` a caller
            // inspects to tell EOF from a real IO failure.
            Self::Transport(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl ReadFrameError {
    /// The correlation id the failing frame carried, when it had one.
    pub fn rid(&self) -> Option<u64> {
        match self {
            Self::Transport(_) | Self::MalformedFrame { .. } => None,
            Self::UnknownOp { rid, .. } | Self::MalformedBody { rid, .. } => *rid,
        }
    }

    /// Whether this failure is scoped to one request rather than to the
    /// connection.
    pub fn is_request_scoped(&self) -> bool {
        matches!(self, Self::UnknownOp { .. } | Self::MalformedBody { .. })
    }

    fn malformed_frame(detail: impl Into<String>) -> Self {
        Self::MalformedFrame {
            detail: detail.into(),
        }
    }
}

/// The [`Frame::Error`] to send back for a decode failure, if any.
///
/// `None` means **log and drop**: a request-scoped failure with no `rid` could
/// not be correlated to anything on the far side, and the frame named no
/// session to report it against, so there is nobody to answer
/// (`docs/ade/protocol-compatibility.md` §2). A transport failure gets no reply
/// either — there is nothing left to write to.
///
/// Both the daemon's receive loop and the client's use this, so the two ends
/// cannot drift on which failures are answerable.
pub fn rejection_frame(error: &ReadFrameError) -> Option<Frame> {
    let reply = |code: &str, rid: Option<u64>| Frame::Error {
        session_id: None,
        workspace_id: None,
        code: code.to_owned(),
        message: error.to_string(),
        request_id: rid,
    };
    match error {
        ReadFrameError::Transport(_) => None,
        // The one decode failure that is answered without a `rid`: the receiver
        // could not read one, and may close the connection after saying so.
        ReadFrameError::MalformedFrame { .. } => Some(reply(error_code::MALFORMED_FRAME, None)),
        ReadFrameError::UnknownOp { rid, .. } => {
            rid.map(|rid| reply(error_code::UNKNOWN_OP, Some(rid)))
        }
        ReadFrameError::MalformedBody { rid, .. } => {
            rid.map(|rid| reply(error_code::MALFORMED_BODY, Some(rid)))
        }
    }
}

/// Stage one of decoding: the envelope, independent of `op`.
///
/// `#[serde(default)]` on `rid` — and no `deny_unknown_fields` — is what makes
/// "unknown top-level keys MUST be ignored" fall out of serde rather than out
/// of a hand-written check.
#[derive(Debug, Deserialize)]
struct Envelope {
    op: String,
    #[serde(default)]
    rid: Option<u64>,
    body: Value,
}

/// The wire shape, written in one place so encode cannot disagree with decode.
#[derive(Debug, Serialize)]
struct OutgoingEnvelope<'a> {
    op: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    rid: Option<u64>,
    body: Value,
}

/// Turn a [`Frame`] into its envelope: `{"op", "rid"?, "body"}`.
///
/// The internally tagged enum already serializes to `{"type": …, <fields
/// inline>}`, so the transform is two removals: the tag becomes `op`, and the
/// variant's own `request_id` — present only when it was `Some`, because every
/// one of them is `skip_serializing_if = "Option::is_none"` — becomes `rid`.
/// Whatever is left is the body, in declaration order (serde_json is built with
/// `preserve_order` here).
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>> {
    let value = serde_json::to_value(frame).context("serializing frame")?;
    let Value::Object(mut body) = value else {
        // Unreachable: `Frame` is an internally tagged enum, and serde refuses
        // to compile one whose variants are not maps.
        bail!("a frame did not serialize to a JSON object");
    };
    let op = match body.shift_remove(TAG_KEY) {
        Some(Value::String(op)) => op,
        _ => bail!("a frame serialized without its {TAG_KEY:?} tag"),
    };
    let rid = match body.shift_remove(REQUEST_ID_KEY) {
        Some(Value::Number(rid)) => rid.as_u64(),
        _ => None,
    };
    serde_json::to_vec(&OutgoingEnvelope {
        op: &op,
        rid,
        body: Value::Object(body),
    })
    .context("serializing frame envelope")
}

/// Decode one envelope payload into a [`Frame`].
///
/// Stage one below is `serde_json::from_slice::<Envelope>` plus the `body`
/// object check; stage two re-inserts the tag and the correlation id so the
/// existing derived `Deserialize` for [`Frame`] does the rest.
pub fn decode_frame(payload: &[u8]) -> std::result::Result<Frame, ReadFrameError> {
    // ---- stage one: the envelope, and nothing about the operation ----
    let envelope: Envelope = serde_json::from_slice(payload)
        .map_err(|error| ReadFrameError::malformed_frame(error.to_string()))?;
    let Value::Object(mut body) = envelope.body else {
        return Err(ReadFrameError::malformed_frame("\"body\" is not an object"));
    };
    if !op_is_well_formed(&envelope.op) {
        // §2 bounds `op` to `[a-z0-9_]`, ≤ 64 bytes, and op identifiers are
        // permanent — so a string outside that grammar cannot name an operation
        // this or any future build implements. That makes it `unknown_op`, and
        // request-scoped like every other unknown op: the envelope itself
        // parsed, so there is nothing here worth closing a connection over.
        // Answering it before stage two is also what keeps a megabyte-long `op`
        // out of serde's error string.
        return Err(ReadFrameError::UnknownOp {
            op: envelope.op,
            rid: envelope.rid,
        });
    }

    // ---- stage two: the body, for this operation ----
    //
    // The rid-injection trick: rather than keep a parallel set of body structs,
    // put the envelope back into the shape the derived `Deserialize` already
    // knows — the internal tag plus the variant's own `request_id`. A body key
    // called `request_id` is dropped first, so the envelope's `rid` is the only
    // correlation source on the wire and the two can never disagree.
    //
    // A `rid` sent on an op whose variant has no `request_id` field (`write`,
    // `resize`, and every unsolicited event) is silently dropped by serde as an
    // unknown field. That is deliberate: those ops are fire-and-forget or
    // events, there is nothing to correlate, and rejecting the frame over a
    // field the sender merely should not have sent would violate §4.
    body.shift_remove(REQUEST_ID_KEY);
    body.insert(TAG_KEY.to_owned(), Value::String(envelope.op.clone()));
    if let Some(rid) = envelope.rid {
        body.insert(REQUEST_ID_KEY.to_owned(), Value::from(rid));
    }
    serde_json::from_value(Value::Object(body)).map_err(|error| {
        // Which of the two request-scoped failures this is depends only on
        // whether the op is one we implement — the same question `KNOWN_OPS`
        // answers, and the reason it has to be exhaustive.
        if KNOWN_OPS.contains(&envelope.op.as_str()) {
            ReadFrameError::MalformedBody {
                op: envelope.op,
                rid: envelope.rid,
                detail: error.to_string(),
            }
        } else {
            ReadFrameError::UnknownOp {
                op: envelope.op,
                rid: envelope.rid,
            }
        }
    })
}

/// Encode `frame` and write it, prefix first. Flushes before returning.
pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let payload = encode_frame(frame)?;
    if payload.len() > MAX_FRAME_BYTES {
        bail!(
            "frame of {} bytes exceeds the {MAX_FRAME_BYTES}-byte maximum",
            payload.len()
        );
    }
    let len = payload.len() as u32;
    writer
        .write_all(&len.to_be_bytes())
        .await
        .context("writing frame length")?;
    writer
        .write_all(&payload)
        .await
        .context("writing frame payload")?;
    writer.flush().await.context("flushing frame")?;
    Ok(())
}

/// Read one frame.
///
/// A [`ReadFrameError::Transport`] ends the connection; the other three do not
/// — the payload has been consumed either way, so the caller answers with
/// [`rejection_frame`] and reads the next frame.
pub async fn read_frame<R>(reader: &mut R) -> std::result::Result<Frame, ReadFrameError>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut length = [0u8; LENGTH_PREFIX_BYTES];
    reader
        .read_exact(&mut length)
        .await
        .context("reading frame length")
        .map_err(ReadFrameError::Transport)?;
    let len = u32::from_be_bytes(length) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(ReadFrameError::Transport(anyhow::anyhow!(
            "frame of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte maximum"
        )));
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("reading frame payload")
        .map_err(ReadFrameError::Transport)?;
    decode_frame(&payload)
}
