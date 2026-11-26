//! Terminal setup: raw mode, size detection and the startup query handshake.

use crate::input::{Event, Parser, Reply};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::termios::{self, OptionalActions, Termios};
use std::io;
use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

/// Id used for the Kitty graphics support query.
const GRAPHICS_QUERY_ID: u32 = 31;

#[derive(Debug)]
pub enum TermError {
    NotATty(&'static str),
    ZeroSize {
        cols: u16,
        rows: u16,
    },
    /// `timeout` is `None` when the terminal answered the final DA1 query but not this one.
    NoResponse {
        query: &'static str,
        timeout: Option<Duration>,
    },
    InvalidResponse(String),
    NoGraphics(String),
    Io(io::Error),
}

impl std::fmt::Display for TermError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotATty(which) => write!(f, "{which} is not a terminal"),
            Self::ZeroSize { cols, rows } => write!(f, "terminal reports a zero size ({cols}x{rows} cells)"),
            Self::NoResponse { query, timeout } => {
                match timeout {
                    Some(t) => write!(f, "terminal did not answer {query} within {} ms", t.as_millis())?,
                    None => write!(f, "terminal does not support {query} (it answered the other queries)")?,
                }
                write!(f, " and TIOCGWINSZ has no pixel size; pass --cell-size WxH to override")
            }
            Self::InvalidResponse(r) => write!(f, "invalid terminal response: {r}"),
            Self::NoGraphics(why) => write!(
                f,
                "terminal does not support the Kitty graphics protocol ({why}); \
                 use --skip-graphics-check to try anyway"
            ),
            Self::Io(e) => write!(f, "terminal I/O error: {e}"),
        }
    }
}

impl std::error::Error for TermError {}

impl From<io::Error> for TermError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rustix::io::Errno> for TermError {
    fn from(e: rustix::io::Errno) -> Self {
        Self::Io(e.into())
    }
}

/// Restores the saved terminal attributes on drop.
pub struct RawMode {
    fd: BorrowedFd<'static>,
    saved: Termios,
    restored: bool,
}

impl RawMode {
    pub fn enter(fd: BorrowedFd<'static>) -> Result<Self, TermError> {
        if !termios::isatty(fd) {
            return Err(TermError::NotATty("stdin"));
        }
        let saved = termios::tcgetattr(fd)?;
        let mut raw = saved.clone();
        raw.make_raw();
        termios::tcsetattr(fd, OptionalActions::Flush, &raw)?;
        Ok(Self { fd, saved, restored: false })
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if !self.restored {
            self.restored = true;
            // Unread replies and reports would otherwise be echoed into the shell.
            let _ = termios::tcflush(self.fd, termios::QueueSelector::IFlush);
            termios::tcsetattr(self.fd, OptionalActions::Now, &self.saved)?;
        }
        Ok(())
    }

    /// Reads and drops input until the answer to a DA1 query sent just before (which
    /// the terminal sends after everything it queued earlier), EOF or `timeout`.
    pub fn discard_input_until_da1(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let (mut parser, mut events, mut buf) = (Parser::new(), Vec::new(), [0u8; 4096]);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            let mut fds = [PollFd::new(&self.fd, PollFlags::IN)];
            match poll(&mut fds, Some(&timespec(left))) {
                Ok(0) => return,
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(_) => return,
            }
            match rustix::io::read(self.fd, &mut buf) {
                Ok(0) => return,
                Ok(n) => parser.feed(&buf[..n], &mut events),
                Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                Err(_) => return,
            }
            if events.drain(..).any(|e| e == Event::Reply(Reply::DeviceAttributes)) {
                return;
            }
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            crate::log::stderr_line(&format!("termway: failed to restore terminal attributes: {e}"));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellSource {
    Override,
    CsiReply,
    Ioctl,
    WindowSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u32,
    pub rows: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    pub source: CellSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub size: TermSize,
    pub graphics: bool,
    pub pixel_mouse: bool,
    /// XTVERSION answer, e.g. "WezTerm 20240203-110809-5046fc22".
    pub terminal_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HandshakeOptions {
    pub cell_override: Option<(u32, u32)>,
    pub skip_graphics_check: bool,
}

pub fn timespec(d: Duration) -> Timespec {
    Timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ }
}

pub fn grid_size(fd: BorrowedFd) -> Result<termios::Winsize, TermError> {
    let ws = termios::tcgetwinsize(fd)?;
    if ws.ws_col == 0 || ws.ws_row == 0 {
        return Err(TermError::ZeroSize { cols: ws.ws_col, rows: ws.ws_row });
    }
    Ok(ws)
}

pub fn write_all(fd: BorrowedFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match rustix::io::write(fd, data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(rustix::io::Errno::INTR) => {}
            // The tty may have been left non-blocking by an earlier program.
            Err(rustix::io::Errno::AGAIN) => {
                let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
                match poll(&mut fds, None) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Collected answers to the startup queries.
#[derive(Debug, Default)]
struct Answers {
    cell: Option<Result<(u32, u32), String>>,
    window: Option<(u32, u32)>,
    graphics: Option<Result<(), String>>,
    pixel_mouse: bool,
    terminal_name: Option<String>,
    done: bool,
}

impl Answers {
    fn take(&mut self, ev: Event) {
        match ev {
            Event::Reply(Reply::CellSize { width, height }) => {
                self.cell = Some(if width == 0 || height == 0 {
                    Err(format!("CSI 16 t reported a {width}x{height} cell"))
                } else {
                    Ok((width, height))
                });
            }
            Event::Reply(Reply::WindowSize { width, height }) if width > 0 && height > 0 => {
                self.window = Some((width, height))
            }
            Event::Reply(Reply::Graphics { id: GRAPHICS_QUERY_ID, ok, message }) => {
                self.graphics = Some(if ok { Ok(()) } else { Err(message) });
            }
            // 1 set, 2 reset, 3 permanently set; 4 (permanently reset) cannot be enabled.
            Event::Reply(Reply::Mode { mode: 1016, state }) => self.pixel_mouse = matches!(state, 1..=3),
            Event::Reply(Reply::TerminalName(name)) => self.terminal_name = Some(name),
            Event::Reply(Reply::DeviceAttributes) => self.done = true,
            Event::Reply(Reply::Malformed(m)) if m.starts_with("CSI 6;") || m.starts_with("CSI 6 ") => {
                self.cell = Some(Err(m))
            }
            // Keystrokes typed during the handshake are dropped.
            _ => {}
        }
    }
}

/// Queries cell size, graphics and SGR-Pixels support and the terminal's name. Replies are collected until the
/// primary device attributes answer (which every terminal sends, in order) or `timeout`.
pub fn handshake(
    input: BorrowedFd,
    output: BorrowedFd,
    timeout: Duration,
    opts: HandshakeOptions,
) -> Result<Capabilities, TermError> {
    let ws = grid_size(output)?;
    let mut q = Vec::new();
    q.extend_from_slice(format!("\x1b_Gi={GRAPHICS_QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\").as_bytes());
    q.extend_from_slice(b"\x1b[16t\x1b[14t\x1b[?1016$p\x1b[>0q\x1b[c");
    write_all(output, &q)?;

    let mut parser = Parser::new();
    let mut answers = Answers::default();
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    let mut events = Vec::new();
    while !answers.done {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let ts = timespec(left);
        let mut fds = [PollFd::new(&input, PollFlags::IN)];
        match poll(&mut fds, Some(&ts)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let n = match rustix::io::read(input, &mut buf) {
            Ok(0) => return Err(TermError::Io(io::ErrorKind::UnexpectedEof.into())),
            Ok(n) => n,
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => continue,
            Err(e) => return Err(e.into()),
        };
        parser.feed(&buf[..n], &mut events);
        events.drain(..).for_each(|e| answers.take(e));
    }

    let (cols, rows) = (ws.ws_col as u32, ws.ws_row as u32);
    let (cell_w, cell_h, source) = if let Some((w, h)) = opts.cell_override {
        (w, h, CellSource::Override)
    } else {
        match answers.cell {
            Some(Ok((w, h))) => (w, h, CellSource::CsiReply),
            Some(Err(m)) => return Err(TermError::InvalidResponse(m)),
            None if ws.ws_xpixel > 0 && ws.ws_ypixel > 0 => {
                (ws.ws_xpixel as u32 / cols, ws.ws_ypixel as u32 / rows, CellSource::Ioctl)
            }
            None => match answers.window {
                Some((w, h)) if w >= cols && h >= rows => (w / cols, h / rows, CellSource::WindowSize),
                _ => {
                    let timeout = (!answers.done).then_some(timeout);
                    return Err(TermError::NoResponse { query: "CSI 16 t (cell size)", timeout });
                }
            },
        }
    };
    if cell_w == 0 || cell_h == 0 {
        return Err(TermError::InvalidResponse(format!("cell size {cell_w}x{cell_h}")));
    }
    let graphics = match answers.graphics {
        Some(Ok(())) => true,
        _ if opts.skip_graphics_check => false,
        Some(Err(m)) => return Err(TermError::NoGraphics(format!("query answered {m:?}"))),
        None => return Err(TermError::NoGraphics("no reply to the a=q query".into())),
    };
    Ok(Capabilities {
        size: TermSize { cols, rows, cell_w, cell_h, source },
        graphics,
        pixel_mouse: answers.pixel_mouse,
        terminal_name: answers.terminal_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fd::{AsFd, OwnedFd};
    use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
    use std::thread;

    struct Pty {
        master: OwnedFd,
        slave: OwnedFd,
    }

    fn pty(cols: u16, rows: u16, xpix: u16, ypix: u16) -> Pty {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = rustix::fs::open(
            name.as_c_str(),
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let mut t = termios::tcgetattr(&slave).unwrap();
        t.make_raw();
        termios::tcsetattr(&slave, OptionalActions::Now, &t).unwrap();
        termios::tcsetwinsize(
            &slave,
            termios::Winsize { ws_col: cols, ws_row: rows, ws_xpixel: xpix, ws_ypixel: ypix },
        )
        .unwrap();
        Pty { master, slave }
    }

    /// Runs the handshake with a fake terminal that answers with `reply` once it saw DA1.
    fn run(
        p: Pty,
        reply: &'static [u8],
        timeout: Duration,
        opts: HandshakeOptions,
    ) -> (Result<Capabilities, TermError>, Duration) {
        let Pty { master, slave } = p;
        let term = thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            while !seen.ends_with(b"\x1b[c") {
                match rustix::io::read(&master, &mut buf) {
                    Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                    _ => return seen,
                }
            }
            write_all(master.as_fd(), reply).unwrap();
            thread::sleep(Duration::from_millis(1500));
            drop(master);
            seen
        });
        let start = Instant::now();
        let r = handshake(slave.as_fd(), slave.as_fd(), timeout, opts);
        let took = start.elapsed();
        drop(term);
        (r, took)
    }

    const FULL: &[u8] =
        b"\x1b_Gi=31;OK\x1b\\\x1b[6;21;9t\x1b[4;1260;2538t\x1b[?1016;2$y\x1bP>|ghostty 1.1.3\x1b\\\x1b[?62;22c";

    #[test]
    fn sgr_pixels_needs_a_mode_that_can_be_set() {
        for (state, ok) in [(0, false), (1, true), (2, true), (3, true), (4, false)] {
            let mut a = Answers::default();
            a.take(Event::Reply(Reply::Mode { mode: 1016, state }));
            assert_eq!(a.pixel_mouse, ok, "DECRPM state {state}");
        }
    }

    /// At exit, mouse reports still in flight are read away up to the DA1 answer.
    #[test]
    fn exit_drain_stops_at_the_da1_answer() {
        let Pty { master, slave } = pty(80, 24, 0, 0);
        // SAFETY: `slave` outlives the raw mode guard, which is dropped first.
        let fd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(std::os::fd::AsRawFd::as_raw_fd(&slave)) };
        let raw = RawMode::enter(fd).unwrap();
        write_all(master.as_fd(), b"\x1b[<35;87;21M\x1b[<35;88;21M\x1b[?62;22c").unwrap();
        let start = Instant::now();
        raw.discard_input_until_da1(Duration::from_secs(2));
        assert!(start.elapsed() < Duration::from_millis(500), "stops at DA1");
        let start = Instant::now();
        raw.discard_input_until_da1(Duration::from_millis(200));
        assert!(start.elapsed() >= Duration::from_millis(200), "no answer: waits for the timeout");
        drop(raw);
    }

    #[test]
    fn full_answers() {
        let (r, took) = run(pty(282, 60, 0, 0), FULL, Duration::from_secs(1), Default::default());
        let caps = r.unwrap();
        assert_eq!(caps.size, TermSize { cols: 282, rows: 60, cell_w: 9, cell_h: 21, source: CellSource::CsiReply });
        assert!(caps.graphics && caps.pixel_mouse);
        assert_eq!(caps.terminal_name.as_deref(), Some("ghostty 1.1.3"));
        assert!(took < Duration::from_millis(500), "DA1 ends the wait early");
    }

    #[test]
    fn no_response_times_out() {
        let (r, took) = run(pty(80, 24, 0, 0), b"", Duration::from_millis(300), Default::default());
        assert!(matches!(r, Err(TermError::NoResponse { .. })), "{r:?}");
        assert!(took >= Duration::from_millis(300) && took < Duration::from_millis(900), "{took:?}");
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("CSI 16 t") && msg.contains("within 300 ms"), "{msg}");
    }

    #[test]
    fn da1_without_cell_size_fails_fast() {
        let (r, took) =
            run(pty(80, 24, 0, 0), b"\x1b_Gi=31;OK\x1b\\\x1b[?62c", Duration::from_secs(1), Default::default());
        assert!(matches!(r, Err(TermError::NoResponse { timeout: None, .. })), "{r:?}");
        assert!(took < Duration::from_millis(500));
        assert!(r.unwrap_err().to_string().contains("does not support CSI 16 t"));
    }

    #[test]
    fn falls_back_to_ioctl_then_window_size() {
        let (r, _) =
            run(pty(100, 50, 1000, 1000), b"\x1b_Gi=31;OK\x1b\\\x1b[?62c", Duration::from_secs(1), Default::default());
        let s = r.unwrap().size;
        assert_eq!((s.cell_w, s.cell_h, s.source), (10, 20, CellSource::Ioctl));
        let (r, _) = run(
            pty(282, 60, 0, 0),
            b"\x1b_Gi=31;OK\x1b\\\x1b[4;1260;2538t\x1b[?62c",
            Duration::from_secs(1),
            Default::default(),
        );
        let s = r.unwrap().size;
        assert_eq!((s.cell_w, s.cell_h, s.source), (9, 21, CellSource::WindowSize));
    }

    #[test]
    fn zero_cell_reply_is_invalid() {
        let (r, _) = run(
            pty(80, 24, 0, 0),
            b"\x1b_Gi=31;OK\x1b\\\x1b[6;0;0t\x1b[?62c",
            Duration::from_secs(1),
            Default::default(),
        );
        assert!(matches!(r, Err(TermError::InvalidResponse(_))), "{r:?}");
    }

    #[test]
    fn malformed_cell_reply_is_invalid() {
        let (r, _) = run(
            pty(80, 24, 0, 0),
            b"\x1b_Gi=31;OK\x1b\\\x1b[6;21t\x1b[?62c",
            Duration::from_secs(1),
            Default::default(),
        );
        assert!(matches!(r, Err(TermError::InvalidResponse(_))), "{r:?}");
    }

    #[test]
    fn zero_grid_is_rejected() {
        let (r, _) = run(pty(0, 0, 0, 0), FULL, Duration::from_millis(100), Default::default());
        assert!(matches!(r, Err(TermError::ZeroSize { .. })), "{r:?}");
    }

    #[test]
    fn missing_graphics_support() {
        let (r, _) = run(pty(80, 24, 0, 0), b"\x1b[6;21;9t\x1b[?62c", Duration::from_secs(1), Default::default());
        assert!(matches!(r, Err(TermError::NoGraphics(_))), "{r:?}");
        let opts = HandshakeOptions { skip_graphics_check: true, ..Default::default() };
        let (r, _) = run(pty(80, 24, 0, 0), b"\x1b[6;21;9t\x1b[?62c", Duration::from_secs(1), opts);
        assert!(!r.unwrap().graphics);
    }

    #[test]
    fn override_skips_cell_queries() {
        let opts = HandshakeOptions { cell_override: Some((8, 16)), ..Default::default() };
        let (r, _) = run(pty(80, 24, 0, 0), b"\x1b_Gi=31;OK\x1b\\\x1b[?62c", Duration::from_secs(1), opts);
        assert_eq!(r.unwrap().size.source, CellSource::Override);
    }

    #[test]
    fn keystrokes_during_handshake_are_ignored() {
        let (r, _) = run(
            pty(282, 60, 0, 0),
            b"ab\x1b[A\x1b_Gi=31;OK\x1b\\\x1b[6;21;9t\x1b[?62c",
            Duration::from_secs(1),
            Default::default(),
        );
        assert_eq!(r.unwrap().size.cell_w, 9);
    }

    #[test]
    fn raw_mode_restores_attributes() {
        let p = pty(80, 24, 0, 0);
        let mut t = termios::tcgetattr(&p.slave).unwrap();
        t.local_modes |= termios::LocalModes::ECHO | termios::LocalModes::ICANON;
        termios::tcsetattr(&p.slave, OptionalActions::Now, &t).unwrap();
        // SAFETY: the fd outlives the RawMode value within this test.
        let fd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(std::os::fd::AsRawFd::as_raw_fd(&p.slave)) };
        {
            let _raw = RawMode::enter(fd).unwrap();
            let now = termios::tcgetattr(&p.slave).unwrap();
            assert!(!now.local_modes.contains(termios::LocalModes::ECHO));
        }
        let after = termios::tcgetattr(&p.slave).unwrap();
        assert!(after.local_modes.contains(termios::LocalModes::ECHO | termios::LocalModes::ICANON));
    }

    #[test]
    fn raw_mode_rejects_non_tty() {
        let (r, _w) = rustix::pipe::pipe().unwrap();
        // SAFETY: the pipe fd lives until the end of the test.
        let fd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(std::os::fd::AsRawFd::as_raw_fd(&r)) };
        assert!(matches!(RawMode::enter(fd), Err(TermError::NotATty(_))));
    }
}
