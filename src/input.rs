//! Parser for bytes arriving on stdin: legacy key encodings, SGR mouse reports and
//! replies to our terminal queries (which share the same stream).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub logo: bool,
}

impl Mods {
    pub const NONE: Mods = Mods { shift: false, alt: false, ctrl: false, logo: false };

    /// xterm modifier parameter: 1 + (1 shift | 2 alt | 4 ctrl | 8 meta).
    fn from_xterm(p: u32) -> Self {
        let b = p.saturating_sub(1);
        Mods { shift: b & 1 != 0, alt: b & 2 != 0, ctrl: b & 4 != 0, logo: b & 8 != 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Mods,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    Motion,
    Wheel { dx: i32, dy: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub action: MouseAction,
    /// Coordinates exactly as reported (1-based cells, or pixels in SGR-Pixels mode).
    pub x: u32,
    pub y: u32,
    pub mods: Mods,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// `CSI 6 ; height ; width t`
    CellSize { width: u32, height: u32 },
    /// `CSI 4 ; height ; width t`
    WindowSize { width: u32, height: u32 },
    /// `CSI 8 ; rows ; cols t`
    TextAreaChars { cols: u32, rows: u32 },
    /// `CSI ? ... c`
    DeviceAttributes,
    /// `CSI ? mode ; state $ y`
    Mode { mode: u32, state: u32 },
    /// `APC G i=<id> ; <message> ST`
    Graphics { id: u32, ok: bool, message: String },
    /// XTVERSION: `DCS > | <name and version> ST`
    TerminalName(String),
    /// A reply-shaped sequence whose parameters could not be parsed.
    Malformed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Reply(Reply),
}

use std::time::Duration;

/// Longest escape sequence we wait for before discarding the buffer.
const MAX_SEQUENCE: usize = 64 * 1024;
/// CSI sequences longer than this are not ours to interpret and are skipped whole.
const MAX_CSI: usize = 256;
/// How long an incomplete sequence other than a lone ESC may take to complete: those
/// come from the terminal (a mouse report or reply split on the way), not from a key.
pub const SEQUENCE_TIMEOUT: Duration = Duration::from_millis(500);

enum Parsed {
    Event(usize, Event),
    Skip(usize),
    Incomplete,
}

#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if bytes of an unfinished sequence are buffered.
    pub fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// How long to wait for more input before calling [`Parser::flush`]: `lone_esc`
    /// when only ESC is buffered (the Escape key, or ESC pressed twice), otherwise
    /// [`SEQUENCE_TIMEOUT`].
    pub fn pending_timeout(&self, lone_esc: Duration) -> Option<Duration> {
        match self.buf.as_slice() {
            [] => None,
            [0x1b] | [0x1b, 0x1b] => Some(lone_esc),
            _ => Some(SEQUENCE_TIMEOUT),
        }
    }

    pub fn feed(&mut self, data: &[u8], out: &mut Vec<Event>) {
        self.buf.extend_from_slice(data);
        let mut i = 0;
        while i < self.buf.len() {
            match parse_one(&self.buf[i..]) {
                Parsed::Event(n, ev) => {
                    out.push(ev);
                    i += n;
                }
                Parsed::Skip(n) => i += n,
                Parsed::Incomplete => break,
            }
        }
        self.buf.drain(..i);
        if self.buf.len() > MAX_SEQUENCE {
            self.buf.clear();
        }
    }

    /// Resolves what is still buffered after [`Parser::pending_timeout`]: a lone ESC is
    /// the Escape key and ESC plus a character that could start a sequence (`[`, `O`,
    /// `]`, ...) is Alt+character. Anything longer is a sequence from the terminal cut
    /// short on the way; it is dropped rather than turned into an Escape key, which is
    /// the default quit key.
    pub fn flush(&mut self, out: &mut Vec<Event>) {
        let buf = std::mem::take(&mut self.buf);
        match buf.as_slice() {
            [0x1b] => out.push(key(Key::Escape, Mods::NONE)),
            &[0x1b, c @ 0x20..=0x7e] => out.push(key(Key::Char(c as char), Mods { alt: true, ..Mods::NONE })),
            [0x1b, 0x1b, ..] => {
                out.push(key(Key::Escape, Mods::NONE));
                self.feed(&buf[1..], out);
                self.flush(out);
            }
            _ => {}
        }
    }
}

fn key(key: Key, mods: Mods) -> Event {
    Event::Key(KeyEvent { key, mods })
}

fn parse_one(b: &[u8]) -> Parsed {
    match b[0] {
        0x1b => parse_escape(b),
        0x00..=0x1f | 0x7f => Parsed::Event(1, control(b[0])),
        _ => match decode_utf8(b) {
            Some(Ok((c, n))) => Parsed::Event(n, key(Key::Char(c), Mods::NONE)),
            Some(Err(n)) => Parsed::Skip(n),
            None => Parsed::Incomplete,
        },
    }
}

fn control(c: u8) -> Event {
    let ctrl = Mods { ctrl: true, ..Mods::NONE };
    match c {
        b'\r' | b'\n' => key(Key::Enter, Mods::NONE),
        b'\t' => key(Key::Tab, Mods::NONE),
        0x7f => key(Key::Backspace, Mods::NONE),
        0x08 => key(Key::Backspace, ctrl),
        0x1b => key(Key::Escape, Mods::NONE),
        0x00 => key(Key::Char(' '), ctrl),
        0x01..=0x1a => key(Key::Char((b'a' + c - 1) as char), ctrl),
        0x1c => key(Key::Char('\\'), ctrl),
        0x1d => key(Key::Char(']'), ctrl),
        0x1e => key(Key::Char('6'), ctrl),
        _ => key(Key::Char('-'), ctrl),
    }
}

/// `None` if more bytes are needed; `Err(n)` to skip `n` invalid bytes.
fn decode_utf8(b: &[u8]) -> Option<Result<(char, usize), usize>> {
    let n = match b[0] {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Some(Err(1)),
    };
    if b.len() < n {
        return if b[1..].iter().all(|&c| (0x80..0xc0).contains(&c)) { None } else { Some(Err(1)) };
    }
    match std::str::from_utf8(&b[..n]) {
        Ok(s) => Some(Ok((s.chars().next().unwrap(), n))),
        Err(_) => Some(Err(1)),
    }
}

fn parse_escape(b: &[u8]) -> Parsed {
    let Some(&second) = b.get(1) else { return Parsed::Incomplete };
    match second {
        b'[' => parse_csi(b),
        b'O' => match b.get(2) {
            None => Parsed::Incomplete,
            Some(&f) => match ss3_key(f) {
                Some(k) => Parsed::Event(3, key(k, Mods::NONE)),
                None => Parsed::Skip(3),
            },
        },
        b'_' | b'P' | b']' | b'^' => parse_string(b),
        // ESC before a cursor or function key sequence is Alt (rxvt); otherwise the
        // first ESC is the Escape key.
        0x1b => match b.get(2) {
            None => Parsed::Incomplete,
            Some(b'[' | b'O') => match parse_escape(&b[1..]) {
                Parsed::Event(n, Event::Key(mut k)) => {
                    k.mods.alt = true;
                    Parsed::Event(n + 1, Event::Key(k))
                }
                Parsed::Incomplete => Parsed::Incomplete,
                _ => Parsed::Event(1, key(Key::Escape, Mods::NONE)),
            },
            Some(_) => Parsed::Event(1, key(Key::Escape, Mods::NONE)),
        },
        _ => match parse_one(&b[1..]) {
            Parsed::Event(n, Event::Key(mut k)) => {
                k.mods.alt = true;
                Parsed::Event(n + 1, Event::Key(k))
            }
            Parsed::Incomplete => Parsed::Incomplete,
            _ => Parsed::Skip(1),
        },
    }
}

fn ss3_key(f: u8) -> Option<Key> {
    Some(match f {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'M' => Key::Enter,
        b'P'..=b'S' => Key::F(f - b'P' + 1),
        _ => return None,
    })
}

/// APC / DCS / OSC / PM strings, terminated by ST (or BEL for OSC).
fn parse_string(b: &[u8]) -> Parsed {
    let mut i = 2;
    while i < b.len() {
        if b[i] == 0x07 && b[1] == b']' {
            return Parsed::Skip(i + 1);
        }
        if b[i] == 0x1b {
            match b.get(i + 1) {
                None => return Parsed::Incomplete,
                Some(b'\\') => {
                    let n = i + 2;
                    if b[1] == b'_' && b.get(2) == Some(&b'G') {
                        return Parsed::Event(n, Event::Reply(graphics_reply(&b[3..i])));
                    }
                    if b[1] == b'P' && b[2..i].starts_with(b">|") {
                        let name = String::from_utf8_lossy(&b[4..i]).into_owned();
                        return Parsed::Event(n, Event::Reply(Reply::TerminalName(name)));
                    }
                    return Parsed::Skip(n);
                }
                // An unterminated string followed by a new sequence: drop the string.
                Some(_) => return Parsed::Skip(i),
            }
        }
        i += 1;
    }
    if b.len() > MAX_SEQUENCE {
        Parsed::Skip(b.len())
    } else {
        Parsed::Incomplete
    }
}

fn graphics_reply(body: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(body);
    let (ctrl, msg) = text.split_once(';').unwrap_or((&text, ""));
    let id = ctrl.split(',').find_map(|kv| kv.strip_prefix("i=")).and_then(|v| v.parse().ok());
    match id {
        Some(id) => Reply::Graphics { id, ok: msg == "OK", message: msg.to_string() },
        None => Reply::Malformed(format!("graphics reply {text:?}")),
    }
}

fn parse_csi(b: &[u8]) -> Parsed {
    let mut i = 2;
    while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
        // A new sequence or typed text ends a broken one.
        if b[i] == 0x1b || b[i] >= 0x80 {
            return Parsed::Skip(i);
        }
        i += 1;
    }
    if i >= b.len() {
        return Parsed::Incomplete;
    }
    let n = i + 1;
    if i > MAX_CSI {
        return Parsed::Skip(n);
    }
    let body = &b[2..i];
    let fin = b[i];
    if body.is_empty() && fin == b'M' {
        // X10 mouse report (terminals without SGR mode): three payload bytes follow.
        return if b.len() < n + 3 { Parsed::Incomplete } else { Parsed::Skip(n + 3) };
    }
    match interpret_csi(body, fin) {
        Some(ev) => Parsed::Event(n, ev),
        None => Parsed::Skip(n),
    }
}

fn interpret_csi(body: &[u8], fin: u8) -> Option<Event> {
    let private = body.first().copied().filter(|c| b"<?>=".contains(c));
    let rest = if private.is_some() { &body[1..] } else { body };
    if private == Some(b'<') && matches!(fin, b'M' | b'm') {
        // SGR mouse. Pixel coordinates go negative when a drag leaves the window: clamp
        // them, as dropping a release would leave the button held in the compositor.
        let text = std::str::from_utf8(rest).ok()?;
        let v: Vec<i64> = text.split(';').map(|p| p.parse().ok()).collect::<Option<_>>()?;
        let at = |i: usize| v.get(i).map(|&n| n.clamp(0, u32::MAX as i64) as u32);
        return mouse(at(0)?, at(1)?, at(2)?, fin == b'M').map(Event::Mouse);
    }
    let inter: Vec<u8> = rest.iter().copied().filter(|c| (0x20..=0x2f).contains(c)).collect();
    let raw: Vec<u8> = rest.iter().copied().filter(|c| !(0x20..=0x2f).contains(c)).collect();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    // Unparsable parameters become None so replies can be reported as malformed.
    let params: Option<Vec<u32>> = if raw.is_empty() {
        Some(Vec::new())
    } else {
        raw.split(';').map(|p| if p.is_empty() { Some(0) } else { p.parse().ok() }).collect()
    };
    let p = |i: usize| params.as_ref().and_then(|v| v.get(i).copied());
    let mods = Mods::from_xterm(p(1).unwrap_or(1));
    match (private, inter.as_slice(), fin) {
        (None, [], b't') => {
            let malformed = || Some(Event::Reply(Reply::Malformed(format!("CSI {raw} t"))));
            let Some(v) = params.as_ref().filter(|v| v.len() == 3) else { return malformed() };
            Some(Event::Reply(match v[0] {
                6 => Reply::CellSize { height: v[1], width: v[2] },
                4 => Reply::WindowSize { height: v[1], width: v[2] },
                8 => Reply::TextAreaChars { rows: v[1], cols: v[2] },
                _ => return None,
            }))
        }
        (Some(b'?'), [], b'c') => Some(Event::Reply(Reply::DeviceAttributes)),
        (Some(b'?'), [b'$'], b'y') => match (p(0), p(1)) {
            (Some(mode), Some(state)) => Some(Event::Reply(Reply::Mode { mode, state })),
            _ => Some(Event::Reply(Reply::Malformed(format!("CSI ?{raw}$y")))),
        },
        (None, [], b'A') => Some(key(Key::Up, mods)),
        (None, [], b'B') => Some(key(Key::Down, mods)),
        (None, [], b'C') => Some(key(Key::Right, mods)),
        (None, [], b'D') => Some(key(Key::Left, mods)),
        (None, [], b'H') => Some(key(Key::Home, mods)),
        (None, [], b'F') => Some(key(Key::End, mods)),
        (None, [], b'Z') => Some(key(Key::Tab, Mods { shift: true, ..Mods::NONE })),
        // With parameters other than "1;mods", CSI R is a cursor position report.
        (None, [], b'P' | b'Q' | b'R' | b'S') if p(0).unwrap_or(1) <= 1 => Some(key(Key::F(fin - b'P' + 1), mods)),
        (None, [], b'u') => codepoint_key(p(0)?, mods),
        (None, [], b'~') => {
            let k = match p(0)? {
                27 => return codepoint_key(p(2)?, mods),
                1 | 7 => Key::Home,
                2 => Key::Insert,
                3 => Key::Delete,
                4 | 8 => Key::End,
                5 => Key::PageUp,
                6 => Key::PageDown,
                n @ 11..=15 => Key::F((n - 10) as u8),
                n @ 17..=21 => Key::F((n - 11) as u8),
                n @ 23..=24 => Key::F((n - 12) as u8),
                _ => return None,
            };
            Some(key(k, mods))
        }
        _ => None,
    }
}

/// A key given by its code point: xterm's modifyOtherKeys (`CSI 27 ; mods ; code ~`) and
/// fixterms (`CSI code ; mods u`). Ghostty sends Shift+Enter, Ctrl+Enter and
/// Alt+Shift+Enter this way even when nobody asked for it; WezTerm does with its CSI u
/// option.
fn codepoint_key(code: u32, mods: Mods) -> Option<Event> {
    let k = match code {
        13 => Key::Enter,
        9 => Key::Tab,
        27 => Key::Escape,
        8 | 127 => Key::Backspace,
        c => Key::Char(char::from_u32(c).filter(|c| !c.is_control())?),
    };
    Some(key(k, mods))
}

fn mouse(b: u32, x: u32, y: u32, press: bool) -> Option<MouseEvent> {
    let mods = Mods { shift: b & 4 != 0, alt: b & 8 != 0, ctrl: b & 16 != 0, logo: false };
    let base = b & !(4 | 8 | 16 | 32);
    let action = if b & 64 != 0 && base < 128 {
        if !press {
            return None;
        }
        match base & 3 {
            0 => MouseAction::Wheel { dx: 0, dy: -1 },
            1 => MouseAction::Wheel { dx: 0, dy: 1 },
            2 => MouseAction::Wheel { dx: -1, dy: 0 },
            _ => MouseAction::Wheel { dx: 1, dy: 0 },
        }
    } else if b & 32 != 0 {
        MouseAction::Motion
    } else {
        let button = match base {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            128 => MouseButton::Back,
            129 => MouseButton::Forward,
            _ => return None,
        };
        if press {
            MouseAction::Press(button)
        } else {
            MouseAction::Release(button)
        }
    };
    Some(MouseEvent { action, x, y, mods })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(bytes, &mut out);
        out
    }

    fn k(key: Key) -> Event {
        Event::Key(KeyEvent { key, mods: Mods::NONE })
    }

    fn km(key: Key, mods: Mods) -> Event {
        Event::Key(KeyEvent { key, mods })
    }

    #[test]
    fn plain_text_and_utf8() {
        assert_eq!(
            parse("aÅあ😀".as_bytes()),
            vec![k(Key::Char('a')), k(Key::Char('Å')), k(Key::Char('あ')), k(Key::Char('😀'))]
        );
    }

    #[test]
    fn split_utf8_is_buffered() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        let bytes = "あ".as_bytes();
        p.feed(&bytes[..1], &mut out);
        assert!(out.is_empty() && p.pending());
        p.feed(&bytes[1..], &mut out);
        assert_eq!(out, vec![k(Key::Char('あ'))]);
        assert!(!p.pending());
    }

    #[test]
    fn invalid_utf8_is_skipped() {
        assert_eq!(parse(b"\xffa\xc3("), vec![k(Key::Char('a')), k(Key::Char('('))]);
    }

    #[test]
    fn control_keys() {
        let ctrl = Mods { ctrl: true, ..Mods::NONE };
        assert_eq!(
            parse(b"\r\t\x7f\x03\x1d\x00"),
            vec![
                k(Key::Enter),
                k(Key::Tab),
                k(Key::Backspace),
                km(Key::Char('c'), ctrl),
                km(Key::Char(']'), ctrl),
                km(Key::Char(' '), ctrl)
            ]
        );
    }

    #[test]
    fn cursor_and_function_keys() {
        let ctrl = Mods { ctrl: true, ..Mods::NONE };
        let shift = Mods { shift: true, ..Mods::NONE };
        assert_eq!(
            parse(b"\x1b[A\x1bOB\x1b[1;5C\x1b[3~\x1b[5;2~\x1bOP\x1b[15~\x1b[24~\x1b[Z\x1b[1;2R"),
            vec![
                k(Key::Up),
                k(Key::Down),
                km(Key::Right, ctrl),
                k(Key::Delete),
                km(Key::PageUp, shift),
                k(Key::F(1)),
                k(Key::F(5)),
                k(Key::F(12)),
                km(Key::Tab, shift),
                km(Key::F(3), shift),
            ]
        );
    }

    #[test]
    fn keys_by_code_point() {
        let m = |shift, alt, ctrl| Mods { shift, alt, ctrl, logo: false };
        assert_eq!(
            // Ghostty: Shift+Enter, Alt+Shift+Enter, Ctrl+Enter; modifyOtherKeys Ctrl+Shift+a;
            // fixterms Shift+Enter, Ctrl+Tab, Alt+Esc, Shift+Backspace, Ctrl+あ.
            parse(b"\x1b[27;2;13~\x1b[27;4;13~\x1b[27;5;13~\x1b[27;6;97~\x1b[13;2u\x1b[9;5u\x1b[27;3u\x1b[127;2u\x1b[12354;5u"),
            vec![
                km(Key::Enter, m(true, false, false)),
                km(Key::Enter, m(true, true, false)),
                km(Key::Enter, m(false, false, true)),
                km(Key::Char('a'), m(true, false, true)),
                km(Key::Enter, m(true, false, false)),
                km(Key::Tab, m(false, false, true)),
                km(Key::Escape, m(false, true, false)),
                km(Key::Backspace, m(true, false, false)),
                km(Key::Char('あ'), m(false, false, true)),
            ]
        );
        // Control code points other than the keys above, and malformed ones, are dropped.
        assert_eq!(parse(b"\x1b[27;5;1~\x1b[27;5~\x1b[1114112;2ux"), vec![k(Key::Char('x'))]);
    }

    #[test]
    fn alt_prefix() {
        let alt = Mods { alt: true, ..Mods::NONE };
        assert_eq!(parse(b"\x1bx\x1b\x7f"), vec![km(Key::Char('x'), alt), km(Key::Backspace, alt)]);
    }

    #[test]
    fn lone_escape_needs_flush() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(b"\x1b", &mut out);
        assert!(out.is_empty() && p.pending());
        p.flush(&mut out);
        assert_eq!(out, vec![k(Key::Escape)]);
        assert!(!p.pending());
        let alt = Mods { alt: true, ..Mods::NONE };
        assert_eq!(parse(b"\x1b\x1b[A"), vec![km(Key::Up, alt)], "ESC before a key sequence is Alt (rxvt)");
        assert_eq!(parse(b"\x1b\x1bx"), vec![k(Key::Escape), km(Key::Char('x'), alt)]);
        let mut out = Vec::new();
        p.feed(b"\x1b\x1b", &mut out);
        assert_eq!(p.pending_timeout(Duration::from_millis(25)), Some(Duration::from_millis(25)));
        p.flush(&mut out);
        assert_eq!(out, vec![k(Key::Escape), k(Key::Escape)]);
    }

    /// Alt+] and friends start a sequence in the legacy encoding; after the timeout they
    /// are the Alt key, not Escape (the default quit key).
    #[test]
    fn alt_with_a_sequence_introducer_is_flushed_as_alt() {
        let alt = Mods { alt: true, ..Mods::NONE };
        for c in [b']', b'[', b'O', b'P', b'_', b'^'] {
            let mut p = Parser::new();
            let mut out = Vec::new();
            p.feed(&[0x1b, c], &mut out);
            assert!(out.is_empty());
            p.flush(&mut out);
            assert_eq!(out, vec![km(Key::Char(c as char), alt)], "ESC {}", c as char);
        }
    }

    /// A report split on the way waits longer than a key and is dropped whole if the rest
    /// never comes; before, it became Escape (quit) plus text typed into the app.
    #[test]
    fn sequence_cut_short_is_dropped_not_escape() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(b"\x1b[<0;120;4", &mut out);
        assert_eq!(p.pending_timeout(Duration::from_millis(25)), Some(SEQUENCE_TIMEOUT));
        p.feed(b"0M", &mut out);
        assert!(matches!(out[..], [Event::Mouse(MouseEvent { x: 120, y: 40, .. })]));
        out.clear();
        for partial in [&b"\x1b[<0;120;4"[..], b"\x1b]11;rgb:0/0", b"\x1bP>|WezT", "あ".as_bytes().split_at(2).0] {
            p.feed(partial, &mut out);
            p.flush(&mut out);
            assert!(out.is_empty() && !p.pending(), "{partial:?}: {out:?}");
        }
    }

    #[test]
    fn overlong_csi_and_x10_mouse_are_skipped_whole() {
        let mut long = b"\x1b[".to_vec();
        long.extend(std::iter::repeat_n(b'1', MAX_CSI + 10));
        long.extend_from_slice(b"Mab");
        assert_eq!(parse(&long), vec![k(Key::Char('a')), k(Key::Char('b'))]);
        assert_eq!(parse(b"\x1b[M #!x"), vec![k(Key::Char('x'))], "X10 report payload is not typed");
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(b"\x1b[M ", &mut out);
        assert!(out.is_empty() && p.pending());
    }

    #[test]
    fn incomplete_csi_waits() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(b"\x1b[1;", &mut out);
        assert!(out.is_empty());
        p.feed(b"5A", &mut out);
        assert_eq!(out, vec![km(Key::Up, Mods { ctrl: true, ..Mods::NONE })]);
    }

    #[test]
    fn sgr_mouse() {
        let ev = |action, x, y, mods| Event::Mouse(MouseEvent { action, x, y, mods });
        assert_eq!(
            parse(b"\x1b[<0;10;5M\x1b[<0;10;5m\x1b[<32;11;5M\x1b[<35;12;6M\x1b[<64;1;1M\x1b[<65;1;1M\x1b[<18;3;4M\x1b[<128;1;1M"),
            vec![
                ev(MouseAction::Press(MouseButton::Left), 10, 5, Mods::NONE),
                ev(MouseAction::Release(MouseButton::Left), 10, 5, Mods::NONE),
                ev(MouseAction::Motion, 11, 5, Mods::NONE),
                ev(MouseAction::Motion, 12, 6, Mods::NONE),
                ev(MouseAction::Wheel { dx: 0, dy: -1 }, 1, 1, Mods::NONE),
                ev(MouseAction::Wheel { dx: 0, dy: 1 }, 1, 1, Mods::NONE),
                ev(MouseAction::Press(MouseButton::Right), 3, 4, Mods { ctrl: true, ..Mods::NONE }),
                ev(MouseAction::Press(MouseButton::Back), 1, 1, Mods::NONE),
            ]
        );
        assert!(parse(b"\x1b[<64;1;1m").is_empty(), "wheel release is ignored");
        assert_eq!(
            parse(b"\x1b[<0;-3;-12m"),
            vec![ev(MouseAction::Release(MouseButton::Left), 0, 0, Mods::NONE)],
            "a release outside the window is clamped, not dropped"
        );
    }

    #[test]
    fn terminal_replies() {
        assert_eq!(
            parse(b"\x1b[6;21;9t\x1b[4;1260;2538t\x1b[8;60;282t\x1b[?62;22c\x1b[?1016;2$y\x1b_Gi=31;OK\x1b\\\x1b_Gi=31;ENOTSUP:no\x1b\\"),
            vec![
                Event::Reply(Reply::CellSize { width: 9, height: 21 }),
                Event::Reply(Reply::WindowSize { width: 2538, height: 1260 }),
                Event::Reply(Reply::TextAreaChars { cols: 282, rows: 60 }),
                Event::Reply(Reply::DeviceAttributes),
                Event::Reply(Reply::Mode { mode: 1016, state: 2 }),
                Event::Reply(Reply::Graphics { id: 31, ok: true, message: "OK".into() }),
                Event::Reply(Reply::Graphics { id: 31, ok: false, message: "ENOTSUP:no".into() }),
            ]
        );
    }

    #[test]
    fn malformed_replies_are_reported() {
        assert!(matches!(parse(b"\x1b[6:21:9t")[..], [Event::Reply(Reply::Malformed(_))]));
        assert!(matches!(parse(b"\x1b[6;21;9;1t")[..], [Event::Reply(Reply::Malformed(_))]));
        assert!(matches!(parse(b"\x1b[6;21t")[..], [Event::Reply(Reply::Malformed(_))]));
        assert!(matches!(parse(b"\x1b_Gfoo\x1b\\")[..], [Event::Reply(Reply::Malformed(_))]));
    }

    #[test]
    fn replies_interleaved_with_keys() {
        assert_eq!(
            parse(b"a\x1b[6;21;9tb"),
            vec![k(Key::Char('a')), Event::Reply(Reply::CellSize { width: 9, height: 21 }), k(Key::Char('b'))]
        );
    }

    #[test]
    fn unknown_sequences_are_dropped() {
        assert_eq!(parse(b"\x1b]11;rgb:0/0/0\x07\x1bP1$r0m\x1b\\\x1b[?2004hx"), vec![k(Key::Char('x'))]);
    }

    #[test]
    fn xtversion_reply() {
        assert_eq!(
            parse(b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\x"),
            vec![Event::Reply(Reply::TerminalName("WezTerm 20240203-110809-5046fc22".into())), k(Key::Char('x'))]
        );
    }

    #[test]
    fn oversized_garbage_is_discarded() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        let mut junk = b"\x1b_".to_vec();
        junk.extend(std::iter::repeat_n(b'x', MAX_SEQUENCE + 10));
        p.feed(&junk, &mut out);
        assert!(!p.pending());
    }
}
