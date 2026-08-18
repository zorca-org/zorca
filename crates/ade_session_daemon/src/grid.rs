//! The screen a session's pty has painted, and how to repaint it on attach.
//!
//! Every byte a pty produces feeds a [`SessionGrid`] as well as the scrollback
//! ring, and [`Frame::Replay`](ade_session::proto::Frame::Replay) carries a
//! *repaint synthesized from this grid* rather than the ring's raw bytes. The
//! wire is unchanged — `Replay` was always opaque bytes and no client knows the
//! difference.
//!
//! Why: raw scrollback is only correct at the width it was produced at. A
//! client re-mounting a terminal view — a tab moved, a split, a workspace
//! reopened — re-attaches, and a full-screen app (htop, vim, less) replayed at
//! the wrong width is garbage. A repaint is correct at whatever size the grid
//! is *now*. Live output after the attach is still forwarded raw and
//! uninterpreted: the client's own emulator is the one drawing.
//!
//! What a repaint asserts, in the order it is emitted:
//!
//! 1. `CSI ? 2026 h` — synchronized output, so a client that understands it
//!    presents the whole repaint in one frame instead of blinking through the
//!    clear;
//! 2. the alternate screen if the session is in it, so the app's eventual
//!    `CSI ? 1049 l` puts the client back in its own scrollback;
//! 3. a reset pen, then a clear and home (pen first: the erase paints with the
//!    current background);
//! 4. every non-blank row, as `CUP` plus SGR-attributed runs;
//! 5. the scrolling region (`DECSTBM`), which a client that lost it would
//!    scroll wrongly for the rest of the session — after the rows, because
//!    `DECSTBM` homes the cursor;
//! 6. the private modes an app set that a re-attach must not silently drop —
//!    cursor keys, mouse reporting, bracketed paste. A client with the right
//!    pixels and the wrong keys is the failure this prevents;
//! 7. the pen the app is drawing with *now*, so the live bytes that follow the
//!    repaint land with the attributes their author assumed;
//! 8. the cursor position and visibility;
//! 9. `CSI ? 2026 l`.
//!
//! **Modes come from [`Term::mode`] wherever alacritty tracks them**, which is
//! most of them; only the handful it has no [`NamedPrivateMode`] for are
//! tracked out of band by scanning the stream (see [`SCANNED_MODES`]). That is
//! the opposite default from the Python twin, which had to scan for everything
//! because pyte silently drops every mode it does not implement.
//!
//! Known gaps, deliberate:
//!
//! - **A wrapped scrollback ring cannot be replayed.** A complete ring is sent
//!   before this repaint, but a wrapped one may begin inside an escape sequence;
//!   in that case attach safely restores only this visible screen.
//! - **After the ring wraps, the saved primary screen is not painted while an
//!   app is on the alt screen.** [`Term`] holds it, but only in a private field,
//!   and the one public door to it ([`Term::swap_alt`]) resets the alternate
//!   screen on the way back. Until the ring wraps, replaying its complete bytes
//!   reconstructs both screens before this repaint.
//! - **No charset-designation state.** `ESC ( 0` line-drawing is not re-emitted;
//!   cells are stored already translated, so the painted rows are right and only
//!   an app that leaves G0 non-default mid-stream would be off.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb, StdSyncHandler};

const CSI: &[u8] = b"\x1b[";

/// Scrollback kept by the emulator, in lines.
///
/// This lets the primary screen survive alternate-screen applications and
/// resizes like a real terminal.
const SCROLLBACK_LINES: usize = 64;

/// alacritty's own floor: a narrower grid panics inside the emulator.
const MIN_COLS: usize = 2;
const MIN_ROWS: usize = 1;

/// Longest incomplete escape sequence [`Scanner`] carries across a chunk
/// boundary. A `CSI ? … h` with a realistic parameter list is far shorter; the
/// bound is what stops a lone `ESC` in binary output from pinning bytes.
const MAX_PENDING: usize = 128;

/// The private modes replayed by scanning, because alacritty has no
/// [`NamedPrivateMode`](alacritty_terminal::vte::ansi::NamedPrivateMode) for
/// them and therefore no bit in [`TermMode`] to read back.
///
/// Everything else a repaint restores comes from [`Term::mode`], which cannot
/// drift from what the emulator actually did. These eight are the exceptions,
/// and every one of them is a mode whose loss a user would feel as a dead mouse
/// or an inverted screen rather than as a wrong pixel.
const SCANNED_MODES: &[u16] = &[
    5,    // DECSCNM, reverse video
    9,    // X10 mouse reporting
    47,   // legacy alternate screen buffer
    1001, // mouse: highlight tracking
    1015, // mouse: urxvt coordinates
    1016, // mouse: SGR pixel coordinates
    1047, // alternate screen buffer
    1048, // save/restore cursor for the alternate screen
];

/// A mode alacritty tracks, and the DECSET number that sets it.
///
/// Read back from [`Term::mode`] at repaint time and compared against
/// [`TermMode::default`], so that a repaint states only what the *session*
/// changed. Modes that default to on — `DECAWM` (7), alternate scroll (1007) —
/// need no special case: they come out as `l` when an app has turned them off.
const TRACKED_MODES: &[(TermMode, u16)] = &[
    (TermMode::APP_CURSOR, 1),
    (TermMode::ORIGIN, 6),
    (TermMode::LINE_WRAP, 7),
    (TermMode::MOUSE_REPORT_CLICK, 1000),
    (TermMode::MOUSE_DRAG, 1002),
    (TermMode::MOUSE_MOTION, 1003),
    (TermMode::FOCUS_IN_OUT, 1004),
    (TermMode::UTF8_MOUSE, 1005),
    (TermMode::SGR_MOUSE, 1006),
    (TermMode::ALTERNATE_SCROLL, 1007),
    (TermMode::BRACKETED_PASTE, 2004),
];

/// The size of a grid, as [`Term`] wants to be told it.
///
/// `total_lines` is `screen_lines` plus the scrollback allowance; alacritty
/// reads this trait for both construction and resize.
#[derive(Clone, Copy, Debug)]
struct GridSize {
    cols: usize,
    rows: usize,
}

impl GridSize {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: usize::from(cols).max(MIN_COLS),
            rows: usize::from(rows).max(MIN_ROWS),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows + SCROLLBACK_LINES
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// One session's screen: what it looks like now, and how to say so.
pub struct SessionGrid {
    term: Term<VoidListener>,
    parser: Processor<StdSyncHandler>,
    scanner: Scanner,
}

impl SessionGrid {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = GridSize::new(cols, rows);
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, VoidListener),
            parser: Processor::default(),
            scanner: Scanner::default(),
        }
    }

    /// Advance the screen by `data`, and note any mode or margin change in it
    /// that [`Term`] does not record.
    pub fn feed(&mut self, data: &[u8]) {
        self.scanner.scan(data);
        self.parser.advance(&mut self.term, data);
    }

    /// Resize the screen. Content comes off the bottom and scrolls off the top
    /// only as far as keeping the cursor on screen requires — alacritty's own
    /// reflow, which is what a real terminal does.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(GridSize::new(cols, rows));
        // `Term::resize` resets the scroll region to the full screen, so the
        // scanned copy has to follow or a repaint would assert a stale one.
        self.scanner.scroll_region = None;
    }

    /// The bytes that reproduce this screen on a freshly opened terminal.
    ///
    /// Assumes a terminal at the grid's own size and otherwise at its defaults,
    /// which is exactly what a client that just mounted a terminal view has.
    /// Everything that differs from those defaults is stated explicitly; what
    /// matches them is left unsaid.
    pub fn repaint(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mode = *self.term.mode();

        // Synchronized output around the whole repaint, and nothing else: a
        // client that buffers it renders the clear and the paint as one frame
        // instead of blinking through an empty screen on every tab switch.
        out.extend_from_slice(CSI);
        out.extend_from_slice(b"?2026h");

        if mode.contains(TermMode::ALT_SCREEN) {
            out.extend_from_slice(CSI);
            out.extend_from_slice(b"?1049h");
        }
        // Pen first, then erase: the erase paints with the current background.
        out.extend_from_slice(CSI);
        out.extend_from_slice(b"0m");
        out.extend_from_slice(CSI);
        out.extend_from_slice(b"2J");
        out.extend_from_slice(CSI);
        out.extend_from_slice(b"H");

        let grid = self.term.grid();
        let columns = grid.columns();
        let default = Cell::default();
        let mut pen = default.clone();
        for row in 0..grid.screen_lines() {
            let line = &grid[Line(row as i32)];
            let cells: Vec<&Cell> = (0..columns).map(|column| &line[Column(column)]).collect();
            let Some((rendered, ended_with)) = render_row(&cells, &default, &pen) else {
                continue;
            };
            out.extend_from_slice(CSI);
            out.extend_from_slice(format!("{};1H", row + 1).as_bytes());
            out.extend_from_slice(&rendered);
            pen = ended_with;
        }

        // After the rows, because DECSTBM homes the cursor as a side effect.
        if let Some((top, bottom)) = self.scanner.scroll_region {
            out.extend_from_slice(CSI);
            out.extend_from_slice(format!("{top};{bottom}r").as_bytes());
        }

        out.extend_from_slice(&self.mode_bytes(mode));

        // The attributes the app is drawing with *now*, so the live bytes that
        // follow this repaint are not styled by whatever the last cell was.
        out.extend_from_slice(&sgr(&grid.cursor.template, &default));

        out.extend_from_slice(&self.cursor_bytes(mode));

        // The title the session last set, so a fresh attach names its tab the
        // way the running program (say, a Claude Code session) named itself —
        // not just the clients that happened to be watching when it spoke.
        if let Some(title) = self.scanner.title.as_deref() {
            out.extend_from_slice(b"\x1b]0;");
            out.extend_from_slice(title.as_bytes());
            out.push(0x07);
        }

        out.extend_from_slice(CSI);
        out.extend_from_slice(b"?2026l");
        out
    }

    fn mode_bytes(&self, mode: TermMode) -> Vec<u8> {
        let mut out = Vec::new();
        // Only what differs from a terminal that has just been opened. A
        // repaint asserts what *this session* changed; re-stating a default
        // would be noise on every attach, and would claim the app asked for
        // something it never mentioned.
        let baseline = TermMode::default();
        for (flag, code) in TRACKED_MODES {
            let on = mode.contains(*flag);
            if on == baseline.contains(*flag) {
                continue;
            }
            out.extend_from_slice(CSI);
            out.extend_from_slice(format!("?{code}{}", if on { 'h' } else { 'l' }).as_bytes());
        }
        for code in SCANNED_MODES {
            if self.scanner.is_on(*code) {
                out.extend_from_slice(CSI);
                out.extend_from_slice(format!("?{code}h").as_bytes());
            }
        }
        // Application keypad is DECKPAM, an escape of its own rather than a
        // private mode: there is no `CSI ? … h` that says it.
        if mode.contains(TermMode::APP_KEYPAD) {
            out.extend_from_slice(b"\x1b=");
        }
        out
    }

    fn cursor_bytes(&self, mode: TermMode) -> Vec<u8> {
        let grid = self.term.grid();
        let point = grid.cursor.point;
        let column = point.column.0.min(grid.columns().saturating_sub(1));
        let mut row = point.line.0.max(0);
        if mode.contains(TermMode::ORIGIN)
            && let Some((top, _)) = self.scanner.scroll_region
        {
            // Origin mode was re-emitted above, so CUP is now relative to it.
            row -= i32::from(top) - 1;
        }
        let mut out = Vec::new();
        out.extend_from_slice(CSI);
        out.extend_from_slice(format!("{};{}H", row.max(0) + 1, column + 1).as_bytes());
        out.extend_from_slice(CSI);
        out.extend_from_slice(if mode.contains(TermMode::SHOW_CURSOR) {
            b"?25h"
        } else {
            b"?25l"
        });
        out
    }
}

/// One row's bytes and the pen left set at the end of it, or `None` if the row
/// is entirely default and the clear already drew it.
///
/// Trailing default cells are dropped: a terminal that has just been cleared is
/// already showing them, and a row of 200 spaces costs 200 bytes per attach.
fn render_row(cells: &[&Cell], default: &Cell, pen: &Cell) -> Option<(Vec<u8>, Cell)> {
    let last = cells.iter().rposition(|cell| *cell != default)?;
    let mut pen = pen.clone();
    let mut out = Vec::new();
    for cell in &cells[..=last] {
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            // The stub cell beside a double-width character; the wide character
            // itself already claimed both columns.
            continue;
        }
        if !same_style(cell, &pen) {
            out.extend_from_slice(&sgr(cell, default));
            pen = (*cell).clone();
        }
        let mut buffer = [0u8; 4];
        out.extend_from_slice(cell.c.encode_utf8(&mut buffer).as_bytes());
        for zerowidth in cell.zerowidth().unwrap_or(&[]) {
            out.extend_from_slice(zerowidth.encode_utf8(&mut buffer).as_bytes());
        }
    }
    Some((out, pen))
}

/// Do two cells paint with the same pen? Content is not style.
fn same_style(one: &Cell, other: &Cell) -> bool {
    one.fg == other.fg && one.bg == other.bg && style_flags(one) == style_flags(other)
}

/// The flags that an SGR can express, without the layout bookkeeping ones.
fn style_flags(cell: &Cell) -> Flags {
    cell.flags
        & (Flags::INVERSE
            | Flags::BOLD
            | Flags::ITALIC
            | Flags::DIM
            | Flags::HIDDEN
            | Flags::STRIKEOUT
            | Flags::ALL_UNDERLINES)
}

/// A full SGR for `cell`'s attributes, always starting from a reset.
///
/// Absolute rather than differential on purpose: a repaint is read by an
/// emulator whose state we are asserting, not negotiating with, and the few
/// extra bytes buy a synthesis that cannot drift.
fn sgr(cell: &Cell, default: &Cell) -> Vec<u8> {
    if same_style(cell, default) {
        return [CSI, b"0m"].concat();
    }
    let flags = style_flags(cell);
    let mut params = vec![0u16];
    if flags.contains(Flags::BOLD) {
        params.push(1);
    }
    if flags.contains(Flags::DIM) {
        params.push(2);
    }
    if flags.contains(Flags::ITALIC) {
        params.push(3);
    }
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        params.push(21);
    } else if flags.intersects(Flags::ALL_UNDERLINES) {
        // Undercurl, dotted and dashed all degrade to a plain underline: the
        // `4:3` subparameter forms are not universally understood, and an
        // underline that is the wrong shape beats no underline at all.
        params.push(4);
    }
    if flags.contains(Flags::INVERSE) {
        params.push(7);
    }
    if flags.contains(Flags::HIDDEN) {
        params.push(8);
    }
    if flags.contains(Flags::STRIKEOUT) {
        params.push(9);
    }
    let mut out = params
        .iter()
        .map(|param| param.to_string())
        .collect::<Vec<_>>()
        .join(";");
    out.push_str(&colour(cell.fg, true));
    out.push_str(&colour(cell.bg, false));
    [CSI, out.as_bytes(), b"m"].concat()
}

/// One colour as SGR parameters, leading `;` included, or empty for the default.
///
/// Unlike the Python twin this keeps 256-colour cells *indexed*: alacritty
/// stores the index rather than resolving it against a palette, so `38;5;N`
/// goes back out as `38;5;N` and the client renders it with its own theme.
fn colour(colour: Color, foreground: bool) -> String {
    let (base, extended, default) = if foreground {
        (30, 38, NamedColor::Foreground)
    } else {
        (40, 48, NamedColor::Background)
    };
    match colour {
        Color::Named(named) if named == default => String::new(),
        Color::Named(named) => {
            let index = named as usize;
            match index {
                // The eight ANSI colours, then the bright eight, which are the
                // same eight offset by 60 in the aixterm range.
                0..=7 => format!(";{}", base + index),
                8..=15 => format!(";{}", base + 60 + index - 8),
                // Dim and cursor colours have no SGR of their own; the palette
                // index is the closest true statement.
                _ => format!(";{extended};5;{index}"),
            }
        }
        Color::Indexed(index) => format!(";{extended};5;{index}"),
        Color::Spec(Rgb { r, g, b }) => format!(";{extended};2;{r};{g};{b}"),
    }
}

/// The state a repaint needs that [`Term`] does not expose.
///
/// Tracked by scanning the byte stream for the things alacritty keeps private:
/// the DEC private modes it has no name for, the scrolling region, and the
/// window title (alacritty hands titles to its event listener, and the daemon's
/// listener is [`VoidListener`]). Sequences split across reads are the normal
/// case at 8 KiB chunks, so an unfinished tail is carried to the next call
/// rather than lost; a tail never holds a *complete* sequence, so nothing is
/// counted twice.
#[derive(Default)]
struct Scanner {
    /// Mode code → on. Only what the stream actually mentioned.
    modes: Vec<(u16, bool)>,
    /// `DECSTBM`, one-based and inclusive, or `None` for the full screen.
    scroll_region: Option<(u16, u16)>,
    /// The window title the session last set (OSC 0/2) — the program inside
    /// naming itself, e.g. Claude Code's live session summary. `None` until
    /// one is set; `Some("")` when the program explicitly cleared it, which a
    /// repaint must re-assert just as firmly.
    title: Option<String>,
    pending: Vec<u8>,
}

impl Scanner {
    fn is_on(&self, code: u16) -> bool {
        self.modes
            .iter()
            .find(|(mode, _)| *mode == code)
            .is_some_and(|(_, on)| *on)
    }

    fn set(&mut self, code: u16, on: bool) {
        match self.modes.iter_mut().find(|(mode, _)| *mode == code) {
            Some(entry) => entry.1 = on,
            None => self.modes.push((code, on)),
        }
    }

    fn scan(&mut self, data: &[u8]) {
        let buffer = if self.pending.is_empty() {
            data.to_vec()
        } else {
            [std::mem::take(&mut self.pending), data.to_vec()].concat()
        };
        let mut at = 0;
        while at < buffer.len() {
            if buffer[at] != 0x1b {
                at += 1;
                continue;
            }
            match parse_escape(&buffer[at..]) {
                Escape::Incomplete => {
                    // Carry it, unless it is too long to be a sequence we care
                    // about — an unterminated OSC string with a novel in it
                    // would otherwise pin bytes forever.
                    let tail = &buffer[at..];
                    if tail.len() <= MAX_PENDING {
                        self.pending = tail.to_vec();
                        return;
                    }
                    at += 1;
                }
                Escape::Skip(len) => at += len,
                Escape::Title { title, len } => {
                    self.title = Some(title);
                    at += len;
                }
                Escape::PrivateMode { params, on, len } => {
                    for code in params {
                        if SCANNED_MODES.contains(&code) {
                            self.set(code, on);
                        }
                    }
                    at += len;
                }
                Escape::ScrollRegion { top, bottom, len } => {
                    // `CSI r` with no useful parameters is "the whole screen",
                    // which is the absence of a region rather than one.
                    self.scroll_region = match (top, bottom) {
                        (Some(top), Some(bottom)) if top >= 1 && bottom > top => {
                            Some((top, bottom))
                        }
                        _ => None,
                    };
                    at += len;
                }
                Escape::Reset(len) => {
                    // RIS puts everything back to power-on defaults, and
                    // alacritty's own state goes with it.
                    self.modes.clear();
                    self.scroll_region = None;
                    at += len;
                }
            }
        }
    }
}

/// The title an OSC payload sets, or `None` for any other OSC.
///
/// OSC 0 (icon name and title) and OSC 2 (title) both count; everything else —
/// clipboard, hyperlinks, color queries — is not a title. The payload text is
/// arbitrary bytes on the wire, so it is read lossily rather than trusted to
/// be UTF-8.
fn osc_title(payload: &[u8]) -> Option<String> {
    let (code, title) = match payload.iter().position(|&byte| byte == b';') {
        Some(split) => (&payload[..split], &payload[split + 1..]),
        // `ESC ] 0 BEL` with no separator: a code alone titles nothing.
        None => return None,
    };
    matches!(code, b"0" | b"2").then(|| {
        String::from_utf8_lossy(title)
            .chars()
            // A control byte cannot be part of a title, and re-emitting one
            // from a repaint would corrupt the very OSC that carries it.
            .filter(|character| !character.is_control())
            .collect()
    })
}

enum Escape {
    /// A prefix of a sequence: more bytes are needed to know what it is.
    Incomplete,
    /// Something this scanner does not care about; skip this many bytes.
    Skip(usize),
    /// OSC 0/2: the session set its window title.
    Title {
        title: String,
        len: usize,
    },
    PrivateMode {
        params: Vec<u16>,
        on: bool,
        len: usize,
    },
    ScrollRegion {
        top: Option<u16>,
        bottom: Option<u16>,
        len: usize,
    },
    Reset(usize),
}

/// Classify the escape sequence starting at the front of `data`.
///
/// A deliberately partial parser: it recognises the sequences the repaint
/// needs and, beyond that, only enough structure to *skip* the rest without
/// mistaking a byte inside one sequence for the start of another. OSC strings
/// are consumed whole either way, because a window title is the one place
/// arbitrary text can otherwise look like a mode change.
fn parse_escape(data: &[u8]) -> Escape {
    let Some(&second) = data.get(1) else {
        return Escape::Incomplete;
    };
    match second {
        b'c' => Escape::Reset(2),
        b']' => {
            // OSC: runs to BEL or ST (`ESC \`). Titles (OSC 0/2) are captured;
            // every other OSC is skipped whole.
            let mut at = 2;
            while at < data.len() {
                let len = if data[at] == 0x07 {
                    Some(at + 1)
                } else if data[at] == 0x1b && data.get(at + 1) == Some(&b'\\') {
                    Some(at + 2)
                } else {
                    None
                };
                if let Some(len) = len {
                    return match osc_title(&data[2..at]) {
                        Some(title) => Escape::Title { title, len },
                        None => Escape::Skip(len),
                    };
                }
                at += 1;
            }
            Escape::Incomplete
        }
        b'[' => {
            let mut at = 2;
            let private = data.get(at) == Some(&b'?');
            if private {
                at += 1;
            }
            let start = at;
            while at < data.len() && (data[at].is_ascii_digit() || data[at] == b';') {
                at += 1;
            }
            let Some(&final_byte) = data.get(at) else {
                return Escape::Incomplete;
            };
            let params = &data[start..at];
            let len = at + 1;
            match (private, final_byte) {
                (true, b'h') | (true, b'l') => Escape::PrivateMode {
                    params: params
                        .split(|byte| *byte == b';')
                        .filter_map(|part| std::str::from_utf8(part).ok()?.parse().ok())
                        .collect(),
                    on: final_byte == b'h',
                    len,
                },
                (false, b'r') => {
                    let mut parts = params
                        .split(|byte| *byte == b';')
                        .map(|part| std::str::from_utf8(part).ok()?.parse::<u16>().ok());
                    Escape::ScrollRegion {
                        top: parts.next().flatten(),
                        bottom: parts.next().flatten(),
                        len,
                    }
                }
                _ => Escape::Skip(len),
            }
        }
        // Every other two-byte escape, and the first byte of anything longer.
        // Skipping two is safe: the byte after an unrecognised introducer can
        // never itself be an ESC we would have wanted to see.
        _ => Escape::Skip(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh grid of the same size, painted only by `grid`'s repaint.
    ///
    /// The equivalence check throughout: "written to a fresh terminal of this
    /// size, these bytes reproduce this screen", with a second `SessionGrid`
    /// standing in for the client's emulator.
    fn roundtrip(grid: &SessionGrid) -> SessionGrid {
        let mut mirror = SessionGrid::new(
            grid.term.grid().columns() as u16,
            grid.term.grid().screen_lines() as u16,
        );
        mirror.feed(&grid.repaint());
        mirror
    }

    #[track_caller]
    fn assert_same_screen(one: &SessionGrid, other: &SessionGrid) {
        let (left, right) = (one.term.grid(), other.term.grid());
        assert_eq!(
            (left.columns(), left.screen_lines()),
            (right.columns(), right.screen_lines()),
            "size"
        );
        for row in 0..left.screen_lines() {
            for column in 0..left.columns() {
                let (a, b) = (
                    &left[Line(row as i32)][Column(column)],
                    &right[Line(row as i32)][Column(column)],
                );
                assert_eq!(a.c, b.c, "char at ({column}, {row})");
                assert_eq!(a.fg, b.fg, "fg at ({column}, {row})");
                assert_eq!(a.bg, b.bg, "bg at ({column}, {row})");
                assert_eq!(style_flags(a), style_flags(b), "flags at ({column}, {row})");
            }
        }
        assert_eq!(left.cursor.point, right.cursor.point, "cursor position");
        assert_eq!(
            one.term.mode().contains(TermMode::SHOW_CURSOR),
            other.term.mode().contains(TermMode::SHOW_CURSOR),
            "cursor visibility"
        );
        assert_eq!(
            left.cursor.template.fg, right.cursor.template.fg,
            "pen foreground"
        );
        assert_eq!(
            left.cursor.template.bg, right.cursor.template.bg,
            "pen background"
        );
    }

    fn grid_with(cols: u16, rows: u16, bytes: &[u8]) -> SessionGrid {
        let mut grid = SessionGrid::new(cols, rows);
        grid.feed(bytes);
        grid
    }

    /// The text of one viewport row, trailing blanks trimmed.
    ///
    /// Spacer cells are skipped: a double-width character occupies two columns
    /// but is one character, and the second column holds a blank placeholder
    /// that is not part of the text.
    fn row_text(grid: &SessionGrid, row: usize) -> String {
        let inner = grid.term.grid();
        let line = &inner[Line(row as i32)];
        (0..inner.columns())
            .map(|column| &line[Column(column)])
            .filter(|cell| {
                !cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            })
            .map(|cell| cell.c)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn screen_text(grid: &SessionGrid) -> String {
        (0..grid.term.grid().screen_lines())
            .map(|row| row_text(grid, row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn plain_text_and_cursor_position_survive_a_repaint() {
        let grid = grid_with(20, 5, b"hello\r\nworld");
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn attributes_survive_a_repaint() {
        let grid = grid_with(
            20,
            3,
            b"\x1b[1;31mbold red\x1b[0m plain \x1b[4;32munderlined\x1b[0m",
        );
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn indexed_and_truecolor_survive_a_repaint() {
        // The Python twin lost the palette index here, resolving it to a
        // truecolor triple on the way in; alacritty keeps it, so the round trip
        // is exact.
        let grid = grid_with(
            30,
            2,
            b"\x1b[38;5;208mindexed\x1b[0m \x1b[38;2;10;20;30mtrue\x1b[0m",
        );
        assert_same_screen(&grid, &roundtrip(&grid));
        let repaint = grid.repaint();
        let text = String::from_utf8_lossy(&repaint);
        assert!(text.contains("38;5;208"), "indexed colour kept: {text:?}");
        assert!(text.contains("38;2;10;20;30"), "truecolor kept: {text:?}");
    }

    #[test]
    fn the_pen_the_app_is_drawing_with_survives_a_repaint() {
        // The trailing SGR sets the pen but paints no cell; live bytes that
        // follow the repaint have to land in that colour.
        let grid = grid_with(20, 3, b"text\x1b[33m");
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn wide_characters_survive_a_repaint() {
        let grid = grid_with(20, 3, "日本語 ok".as_bytes());
        assert_same_screen(&grid, &roundtrip(&grid));
        assert_eq!(row_text(&roundtrip(&grid), 0), "日本語 ok");
    }

    #[test]
    fn a_hidden_cursor_stays_hidden() {
        let grid = grid_with(20, 3, b"\x1b[?25lhidden");
        assert!(!grid.term.mode().contains(TermMode::SHOW_CURSOR));
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn an_untouched_screen_repaints_to_an_empty_screen() {
        let grid = SessionGrid::new(20, 4);
        assert_same_screen(&grid, &roundtrip(&grid));
        assert_eq!(screen_text(&roundtrip(&grid)).trim(), "");
    }

    #[test]
    fn the_scroll_region_is_restored() {
        let grid = grid_with(20, 10, b"\x1b[3;8r\x1b[5;1Hinside");
        assert_eq!(grid.scanner.scroll_region, Some((3, 8)));
        let repaint = String::from_utf8_lossy(&grid.repaint()).into_owned();
        assert!(
            repaint.contains("\x1b[3;8r"),
            "DECSTBM replayed: {repaint:?}"
        );
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn a_reset_scroll_region_is_not_replayed() {
        let grid = grid_with(20, 10, b"\x1b[3;8r\x1b[r");
        assert_eq!(grid.scanner.scroll_region, None);
        assert!(!String::from_utf8_lossy(&grid.repaint()).contains('r'));
    }

    #[test]
    fn a_repaint_is_wrapped_in_synchronized_output() {
        let repaint = grid_with(20, 3, b"anything").repaint();
        assert!(
            repaint.starts_with(b"\x1b[?2026h"),
            "opens synchronized: {:?}",
            String::from_utf8_lossy(&repaint)
        );
        assert!(
            repaint.ends_with(b"\x1b[?2026l"),
            "closes synchronized: {:?}",
            String::from_utf8_lossy(&repaint)
        );
    }

    // -- resize --------------------------------------------------------------

    #[test]
    fn repaint_after_a_resize_is_at_the_new_size() {
        let mut grid = grid_with(40, 10, b"hello");
        grid.resize(20, 5);
        let mirror = roundtrip(&grid);
        assert_eq!(
            (
                mirror.term.grid().columns(),
                mirror.term.grid().screen_lines()
            ),
            (20, 5)
        );
        assert_same_screen(&grid, &mirror);
    }

    #[test]
    fn growing_keeps_the_content() {
        let mut grid = grid_with(20, 5, b"keep me");
        grid.resize(40, 12);
        assert_eq!(row_text(&grid, 0), "keep me");
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn shrinking_keeps_the_visible_output() {
        // The case the grid exists for: a client mounting a small split must
        // not lose the output it is mounting to look at.
        let mut grid = grid_with(40, 30, b"line one\r\nline two");
        grid.resize(40, 8);
        assert!(
            screen_text(&grid).contains("line one"),
            "kept: {:?}",
            screen_text(&grid)
        );
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn a_resize_resets_the_scroll_region() {
        let mut grid = grid_with(20, 10, b"\x1b[3;8r");
        assert_eq!(grid.scanner.scroll_region, Some((3, 8)));
        grid.resize(20, 6);
        assert_eq!(grid.scanner.scroll_region, None, "alacritty reset it too");
    }

    #[test]
    fn a_cursor_past_the_last_column_is_clamped() {
        let grid = grid_with(5, 2, b"abcde");
        let repaint = String::from_utf8_lossy(&grid.repaint()).into_owned();
        assert!(
            repaint.contains("\x1b[1;5H"),
            "clamped to col 5: {repaint:?}"
        );
    }

    // -- private modes -------------------------------------------------------

    #[test]
    fn modes_alacritty_tracks_are_replayed_from_its_own_state() {
        let grid = grid_with(20, 3, b"\x1b[?1h\x1b[?1006h\x1b[?1002h\x1b[?2004h");
        let repaint = String::from_utf8_lossy(&grid.repaint()).into_owned();
        for mode in ["?1h", "?1006h", "?1002h", "?2004h"] {
            assert!(repaint.contains(mode), "{mode} replayed: {repaint:?}");
        }
    }

    #[test]
    fn modes_alacritty_ignores_are_scanned_and_replayed() {
        // A re-attached client that lost these has working pixels and a dead
        // mouse, which is why they are tracked out of band at all.
        let grid = grid_with(20, 3, b"\x1b[?9h\x1b[?1015h\x1b[?1016h");
        let repaint = String::from_utf8_lossy(&grid.repaint()).into_owned();
        for mode in ["?9h", "?1015h", "?1016h"] {
            assert!(repaint.contains(mode), "{mode} replayed: {repaint:?}");
        }
    }

    #[test]
    fn a_mode_that_was_turned_off_is_not_replayed() {
        let grid = grid_with(20, 3, b"\x1b[?1h\x1b[?9h\x1b[?1l\x1b[?9l");
        let repaint = String::from_utf8_lossy(&grid.repaint()).into_owned();
        assert!(!repaint.contains("?1h"), "{repaint:?}");
        assert!(!repaint.contains("?9h"), "{repaint:?}");
    }

    #[test]
    fn a_mode_split_across_two_reads_is_still_tracked() {
        // The normal case at 8 KiB chunks, and the reason the scanner carries a
        // tail at all.
        let mut grid = SessionGrid::new(20, 3);
        grid.feed(b"text\x1b[?10");
        grid.feed(b"15h more");
        assert!(grid.scanner.is_on(1015), "carried across the boundary");
        assert!(String::from_utf8_lossy(&grid.repaint()).contains("?1015h"));
    }

    #[test]
    fn several_modes_in_one_sequence_are_all_tracked() {
        let grid = grid_with(20, 3, b"\x1b[?9;1015;1016h");
        for mode in [9, 1015, 1016] {
            assert!(grid.scanner.is_on(mode), "{mode}");
        }
    }

    #[test]
    fn an_escape_that_is_not_a_mode_does_not_wedge_the_scan() {
        let mut grid = SessionGrid::new(20, 3);
        grid.feed(b"\x1b]0;a window title with \x1b[?9h in it\x07");
        assert!(
            !grid.scanner.is_on(9),
            "a mode inside an OSC string is text, not a mode"
        );
        grid.feed(b"\x1b[?9h");
        assert!(grid.scanner.is_on(9), "and the scanner still works after");
    }

    // -- titles --------------------------------------------------------------

    #[test]
    fn the_last_title_the_session_set_is_replayed() {
        // Both OSC forms count, and only the newest survives — a repaint names
        // the session as it is now, not as it introduced itself.
        let grid = grid_with(20, 3, b"\x1b]0;first\x07text\x1b]2;second\x1b\\");
        let repaint = String::from_utf8_lossy(&grid.repaint()).into_owned();
        assert!(repaint.contains("\x1b]0;second\x07"), "{repaint:?}");
        assert!(
            !repaint.contains("first"),
            "only the last title: {repaint:?}"
        );
    }

    #[test]
    fn a_session_that_never_titled_itself_replays_no_title() {
        let repaint = grid_with(20, 3, b"plain").repaint();
        assert!(!repaint.windows(2).any(|pair| pair == b"\x1b]"));
    }

    #[test]
    fn a_title_split_across_two_reads_is_still_captured() {
        let mut grid = SessionGrid::new(20, 3);
        grid.feed(b"\x1b]0;a claude ses");
        grid.feed(b"sion title\x07");
        assert_eq!(
            grid.scanner.title.as_deref(),
            Some("a claude session title")
        );
    }

    #[test]
    fn an_explicitly_cleared_title_is_reasserted_as_cleared() {
        // A client attaching after the clear must not resurrect "busy" from
        // its own defaults — the empty title is a statement, not an absence.
        let grid = grid_with(20, 3, b"\x1b]0;busy\x07\x1b]0;\x07");
        assert_eq!(grid.scanner.title.as_deref(), Some(""));
        let repaint = String::from_utf8_lossy(&grid.repaint()).into_owned();
        assert!(repaint.contains("\x1b]0;\x07"), "{repaint:?}");
    }

    #[test]
    fn a_non_title_osc_is_not_a_title() {
        let grid = grid_with(20, 3, b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(grid.scanner.title, None);
    }

    #[test]
    fn control_bytes_in_a_title_are_not_captured() {
        // A title is the one place arbitrary text could smuggle a control byte
        // back into the repaint's own OSC and cut it short.
        let grid = grid_with(20, 3, b"\x1b]0;sneaky\x1b[2Jtitle\x07");
        assert_eq!(grid.scanner.title.as_deref(), Some("sneaky[2Jtitle"));
    }

    #[test]
    fn autowrap_off_is_replayed() {
        let grid = grid_with(20, 3, b"\x1b[?7l");
        assert!(String::from_utf8_lossy(&grid.repaint()).contains("?7l"));
        // On is the default and needs no assertion in the repaint.
        let default = grid_with(20, 3, b"plain");
        assert!(!String::from_utf8_lossy(&default.repaint()).contains("?7l"));
    }

    #[test]
    fn modes_a_fresh_terminal_already_has_are_not_asserted() {
        // Alternate scroll and autowrap are on in a terminal nobody has touched.
        // Re-stating them on every attach would be noise, and would claim the
        // session asked for something it never mentioned.
        let repaint = String::from_utf8_lossy(&grid_with(20, 3, b"plain").repaint()).into_owned();
        for quiet in ["?1007h", "?7h", "?1042h"] {
            assert!(
                !repaint.contains(quiet),
                "{quiet} not asserted: {repaint:?}"
            );
        }
    }

    #[test]
    fn a_default_on_mode_turned_off_is_replayed_as_off() {
        let repaint =
            String::from_utf8_lossy(&grid_with(20, 3, b"\x1b[?1007l").repaint()).into_owned();
        assert!(repaint.contains("?1007l"), "{repaint:?}");
    }

    #[test]
    fn application_keypad_is_replayed_as_deckpam() {
        // DECKPAM has no `CSI ? … h` form, so the repaint has to say `ESC =`.
        let grid = grid_with(20, 3, b"\x1b=");
        assert!(grid.term.mode().contains(TermMode::APP_KEYPAD));
        assert!(grid.repaint().windows(2).any(|pair| pair == b"\x1b="));
    }

    #[test]
    fn a_reset_clears_scanned_state() {
        let mut grid = grid_with(20, 5, b"\x1b[?9h\x1b[2;4r");
        grid.feed(b"\x1bc");
        assert!(!grid.scanner.is_on(9));
        assert_eq!(grid.scanner.scroll_region, None);
    }

    // -- alternate screen ----------------------------------------------------

    #[test]
    fn alacritty_keeps_both_buffers_across_1049() {
        // The pyte gap this port closes. The Python twin had one buffer, so an
        // app's last frame stayed on the primary screen after it exited and the
        // user saw leftover htop rows above their prompt.
        let grid = grid_with(20, 5, b"primary output\x1b[?1049h\x1b[HALT SCREEN");
        assert!(
            screen_text(&grid).contains("ALT SCREEN"),
            "on the alt screen: {:?}",
            screen_text(&grid)
        );
        assert!(
            !screen_text(&grid).contains("primary output"),
            "the primary is not visible from the alt screen"
        );
    }

    #[test]
    fn leaving_the_alternate_screen_restores_the_primary() {
        // The requirement: enter, draw, exit, and the repaint is the primary
        // screen with no leftovers from the app.
        let grid = grid_with(
            20,
            5,
            b"primary output\x1b[?1049h\x1b[HALT SCREEN\x1b[?1049l",
        );
        let text = screen_text(&grid);
        assert!(
            text.contains("primary output"),
            "primary restored: {text:?}"
        );
        assert!(!text.contains("ALT SCREEN"), "no alt leftovers: {text:?}");

        let repaint = String::from_utf8_lossy(&grid.repaint()).into_owned();
        assert!(
            !repaint.contains("?1049h"),
            "not replayed as alt: {repaint:?}"
        );
        assert!(repaint.contains("primary output"), "{repaint:?}");
        assert!(!repaint.contains("ALT SCREEN"), "{repaint:?}");
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn the_alternate_screen_is_entered_before_the_repaint_paints() {
        let grid = grid_with(20, 5, b"\x1b[?1049h\x1b[Hin the app");
        let repaint = grid.repaint();
        let text = String::from_utf8_lossy(&repaint).into_owned();
        // Order matters: 1049h has to come before the clear, or the clear lands
        // on the client's scrollback instead of the alternate buffer.
        let enter = text.find("?1049h").expect("alt screen entered");
        let clear = text.find("2J").expect("screen cleared");
        assert!(enter < clear, "enter before clear: {text:?}");
        assert!(
            text.find("?2026h").expect("synchronized") < enter,
            "synchronized wraps everything: {text:?}"
        );
        assert_same_screen(&grid, &roundtrip(&grid));
    }

    #[test]
    fn a_resize_while_on_the_alternate_screen_resizes_both() {
        let mut grid = grid_with(40, 20, b"primary\x1b[?1049h\x1b[Happ");
        grid.resize(30, 10);
        assert_eq!(grid.term.grid().screen_lines(), 10);
        assert_same_screen(&grid, &roundtrip(&grid));
        // And the primary is still there underneath, at the new size.
        grid.feed(b"\x1b[?1049l");
        assert!(
            screen_text(&grid).contains("primary"),
            "{:?}",
            screen_text(&grid)
        );
    }
}
