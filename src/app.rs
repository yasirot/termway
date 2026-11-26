//! Main loop: terminal <-> renderer <-> compositor.

use crate::cli::{MouseMode, Options, QuitKey};
use crate::compositor::{self, Compositor, CompositorError, OutputMode};
use crate::frame::{FrameBuf, FrameError};
use crate::geom::{CellGeom, DirtyMap, Rect, TileGrid};
use crate::input::{Event, Key, KeyEvent, Mods, MouseAction, MouseButton, Parser, Reply};
use crate::keymap::{Keymap, DYN_SLOTS};
use crate::render::{self, damage_to_dirty, Compat, Phase, RenderConfig, Renderer};
use crate::signals::{self, SignalPipe};
use crate::stats::{Extra, Stats};
use crate::term::{self, HandshakeOptions, RawMode, TermError};
use crate::vt::{self, Vt};
use crate::wl::{CaptureStatus, Wayland, WlError};
use crate::writer::Writer;
use rustix::event::{poll, PollFd, PollFlags};
use rustix::fd::BorrowedFd;
use std::io;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
/// A lone ESC is the Escape key once nothing follows for this long.
const ESC_TIMEOUT: Duration = Duration::from_millis(25);
/// Longer when ESC quits: an ESC that merely starts a report split on the way must not
/// end the session, and quitting a little later costs nothing.
const QUIT_ESC_TIMEOUT: Duration = Duration::from_millis(150);
/// Waiting at exit for the terminal to confirm mouse reporting is off.
const EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(300);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);
const MAX_CAPTURE_FAILURES: u32 = 20;
const MAX_OUTPUT_PX: u32 = 16384;
const MIN_REFRESH_HZ: u32 = 60;
/// Without an app of termway's own, a config that starts none leaves the screen black.
const NO_WINDOW_WARNING: Duration = Duration::from_secs(5);

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

#[derive(Debug)]
pub enum AppError {
    Term(TermError),
    Wayland(WlError),
    Compositor(CompositorError),
    Frame(FrameError),
    Io(io::Error),
    Config(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Term(e) => e.fmt(f),
            Self::Wayland(e) => e.fmt(f),
            Self::Compositor(e) => e.fmt(f),
            Self::Frame(e) => e.fmt(f),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Config(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for AppError {}

macro_rules! from_err {
    ($t:ty, $v:ident) => {
        impl From<$t> for AppError {
            fn from(e: $t) -> Self {
                Self::$v(e)
            }
        }
    };
}
from_err!(TermError, Term);
from_err!(WlError, Wayland);
from_err!(CompositorError, Compositor);
from_err!(FrameError, Frame);
from_err!(io::Error, Io);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    QuitKey,
    Signal(i32),
    AppExited,
    TerminalClosed,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Self::QuitKey | Self::AppExited | Self::TerminalClosed => 0,
            Self::Signal(s) => (128 + s) as u8,
        }
    }
}

/// Terminal-side state that must be undone on every exit path (also on panic).
struct Terminal {
    out: BorrowedFd<'static>,
    raw: RawMode,
    writer: Option<Writer>,
    active: bool,
    compat: Compat,
}

/// Frees every image (hidden cached ones too) and undoes [`Terminal::setup`].
fn restore_sequence(compat: Compat) -> Vec<u8> {
    let mut s = b"\x1b[?2026l\x1b_Ga=d,d=A,q=2\x1b\\".to_vec();
    // WezTerm's d=A already frees hidden images and it does not implement d=R.
    if compat == Compat::Kitty {
        s.extend_from_slice(b"\x1b_Ga=d,d=R,x=1,y=4294967295,q=2\x1b\\");
    }
    s.extend_from_slice(
        b"\x1b[0m\x1b[2J\x1b[?1016l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?7h\x1b[?25h\x1b[?1049l",
    );
    // Its answer comes after any mouse report sent before tracking was turned off.
    s.extend_from_slice(b"\x1b[c");
    s
}

impl Terminal {
    fn setup(&mut self, mouse: bool, pixels: bool) -> io::Result<()> {
        let mut s = b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[0m\x1b[2J".to_vec();
        if mouse {
            s.extend_from_slice(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
            if pixels {
                s.extend_from_slice(b"\x1b[?1016h");
            }
        }
        self.active = true;
        term::write_all(self.out, &s)?;
        self.writer = Some(Writer::spawn(self.out)?);
        Ok(())
    }

    fn writer(&mut self) -> &mut Writer {
        self.writer.as_mut().expect("writer runs while the session is active")
    }

    fn restore(&mut self) -> io::Result<()> {
        // Frames are written whole, so once the writer is drained the terminal is at a
        // sequence boundary and the cleanup cannot land inside an image upload.
        let drained = self.writer.take().is_none_or(|mut w| w.finish(Duration::from_secs(3)));
        let mut result = Ok(());
        if self.active {
            self.active = false;
            if drained {
                result = term::write_all(self.out, &restore_sequence(self.compat));
                if result.is_ok() {
                    // Reports still in flight would reach the shell as text.
                    self.raw.discard_input_until_da1(EXIT_DRAIN_TIMEOUT);
                }
            } else {
                result = Err(io::Error::other("terminal stopped reading; screen not restored"));
            }
        }
        self.raw.restore().and(result)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            crate::log_error!("terminal restore: {e}");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    cols: u32,
    rows: u32,
    cell_w: u32,
    cell_h: u32,
    draw_rows: u32,
}

impl Geometry {
    fn new(cols: u32, rows: u32, cell: (u32, u32), status_line: bool) -> Result<Self, AppError> {
        let draw_rows = rows.saturating_sub(status_line as u32);
        if cols == 0 || draw_rows == 0 {
            return Err(AppError::Config(format!("terminal too small ({cols}x{rows} cells)")));
        }
        if cell.0 == 0 || cell.1 == 0 {
            return Err(AppError::Config(format!("invalid cell size {}x{}", cell.0, cell.1)));
        }
        let g = Self { cols, rows, cell_w: cell.0, cell_h: cell.1, draw_rows };
        match (cols.checked_mul(cell.0), draw_rows.checked_mul(cell.1)) {
            (Some(w), Some(h)) if w <= MAX_OUTPUT_PX && h <= MAX_OUTPUT_PX => Ok(g),
            _ => Err(AppError::Config(format!(
                "output {cols}x{draw_rows} cells of {}x{} px exceeds {MAX_OUTPUT_PX} px",
                cell.0, cell.1
            ))),
        }
    }

    fn output_px(&self) -> (u32, u32) {
        (self.cols * self.cell_w, self.draw_rows * self.cell_h)
    }

    fn cells(&self) -> CellGeom {
        CellGeom { cell_w: self.cell_w, cell_h: self.cell_h, cols: self.cols, rows: self.draw_rows }
    }
}

#[derive(Clone, Copy, Debug)]
enum Resize {
    Debounce(Instant),
    AwaitCell { deadline: Instant, cols: u32, rows: u32 },
}

struct App {
    // Field order is drop order: restore the terminal before stopping the compositor.
    term: Terminal,
    wl: Wayland,
    compositor: Option<Compositor>,
    signals: SignalPipe,
    opts: Options,
    parser: Parser,
    keymap: Keymap,
    renderer: Renderer,
    vt: Vt,
    fb: FrameBuf,
    dirty: DirtyMap,
    stats: Stats,
    geom: Geometry,
    compat: Compat,
    large_png: bool,
    pixel_mouse: bool,
    full_next: bool,
    next_capture: Instant,
    capture_started: Instant,
    frame_interval: Duration,
    capture_failures: u32,
    esc_deadline: Option<Instant>,
    resize: Option<Resize>,
    /// When to warn if the compositor still has no window (only when termway started no app).
    no_window_check: Option<Instant>,
    events: Vec<Event>,
}

fn render_config(o: &Options, compat: Compat, large_png: bool) -> RenderConfig {
    RenderConfig {
        phase: o.phase,
        compat,
        large_png,
        cache_bytes: o.cache_mb << 20,
        zlib_level: o.zlib_level,
        tile_zlib_level: o.tile_zlib_level,
        solid_fill: o.solid_fill,
        lowres_threshold: o.lowres_threshold,
        refine_delay: o.refine_delay,
        first_image_id: 1,
        threads: o.threads,
        scroll_copy: o.scroll_copy,
    }
}

/// Compositor refresh rate: at least 60 Hz so input feels immediate, and never below
/// the capture rate.
fn refresh_hz(o: &Options) -> u32 {
    o.max_fps.max(MIN_REFRESH_HZ)
}

fn output_mode(o: &Options, g: &Geometry) -> OutputMode {
    let (width, height) = g.output_px();
    OutputMode { width, height, refresh_hz: refresh_hz(o), scale: o.scale.resolve(g.cell_h) }
}

fn stdin_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 0 stays open for the whole process lifetime.
    unsafe { BorrowedFd::borrow_raw(0) }
}

fn stdout_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 1 stays open for the whole process lifetime.
    unsafe { BorrowedFd::borrow_raw(1) }
}

/// Runs a session; returns how it ended plus a summary line.
pub fn run(mut opts: Options) -> Result<(Exit, String), AppError> {
    let (stdin, stdout) = (stdin_fd(), stdout_fd());
    if !rustix::termios::isatty(stdout) {
        return Err(TermError::NotATty("stdout").into());
    }
    let signals = SignalPipe::install()?;
    let raw = RawMode::enter(stdin)?;
    let mut terminal = Terminal { out: stdout, raw, writer: None, active: false, compat: Compat::Kitty };
    let caps = term::handshake(
        stdin,
        stdout,
        HANDSHAKE_TIMEOUT,
        HandshakeOptions { cell_override: opts.cell_size, skip_graphics_check: opts.skip_graphics_check },
    )?;
    log_info!("terminal: {:?}, graphics {}, SGR-Pixels {}", caps.size, caps.graphics, caps.pixel_mouse);
    let compat = opts.compat.unwrap_or_else(|| Compat::detect(caps.terminal_name.as_deref()));
    log_info!(
        "terminal name {:?}, graphics compat {} ({})",
        caps.terminal_name.as_deref().unwrap_or("unknown"),
        compat.name(),
        if opts.compat.is_some() { "--graphics-compat" } else { "detected" }
    );
    terminal.compat = compat;
    let large_png = opts.large_png.unwrap_or_else(|| render::needs_png(caps.terminal_name.as_deref()));
    log_info!(
        "images over {} KiB sent as {} ({})",
        render::PNG_MIN_BYTES >> 10,
        if large_png { "PNG" } else { "zlib" },
        if opts.large_png.is_some() { "--large-images" } else { "detected" }
    );
    if opts.compat.is_none() && caps.terminal_name.is_none() && opts.scroll_copy && opts.phase == Phase::Progressive {
        // kitty and Ghostty answer XTVERSION; silence often means WezTerm behind ssh.exe,
        // where re-placing the scroll pieces makes WezTerm duplicate them.
        log_warn!(
            "terminal did not answer XTVERSION: scroll copy disabled; \
             for WezTerm pass --graphics-compat wezterm, otherwise --graphics-compat kitty"
        );
        opts.scroll_copy = false;
    }
    let geom = Geometry::new(caps.size.cols, caps.size.rows, (caps.size.cell_w, caps.size.cell_h), opts.status_line)?;
    let mode = output_mode(&opts, &geom);

    let (mut compositor, socket) = match &opts.attach {
        Some(d) => (None, compositor::resolve_display(d)?),
        None => {
            let launch = compositor::Launch {
                command: &opts.command,
                mode,
                log: opts.compositor_log.as_deref(),
                sway_config: opts.sway_config.as_deref(),
                x11: opts.x11,
            };
            let mut c = Compositor::spawn(opts.compositor, &launch)?;
            let s = c.wait_for_socket(Duration::from_secs(10))?;
            log_info!("{} started (pid {:?}), socket {}", opts.compositor.binary(), c.pid(), s.display());
            if let Some(p) = &opts.sway_config {
                log_info!("including sway config {}", p.display());
            }
            if opts.x11 {
                log_info!("running {} as an X11 session on rootful Xwayland", opts.command.join(" "));
            }
            (Some(c), s)
        }
    };
    let stream = UnixStream::connect(&socket).map_err(|e| WlError::Connect(format!("{}: {e}", socket.display())))?;
    let mut wl = Wayland::connect(stream, opts.output.as_deref(), opts.attach.is_none())?;
    log_info!("protocols: {}", wl.protocols());
    log_info!("capturing output {}", wl.output_name());
    for e in compositor.iter().flat_map(Compositor::config_errors) {
        log_warn!("sway config: {e}");
    }
    if opts.resize_output {
        if !wl.can_resize() {
            log_warn!("compositor lacks wlr-output-management; the frame is clipped/padded instead of resized");
        } else if let Err(e) = wl.set_output_mode(mode) {
            if e.is_disconnect() {
                // The event loop reports why the session ended.
                log_debug!("output mode not set: {e}");
            } else {
                log_warn!("{e}; the frame is clipped/padded instead");
            }
        } else {
            log_info!("output mode set to {}x{} at scale {}", mode.width, mode.height, mode.scale);
        }
    }
    let keymap = Keymap::new();
    if wl.has_keyboard() {
        wl.upload_keymap(&keymap.text())?;
    } else {
        log_warn!("no virtual keyboard: keyboard input is disabled");
    }
    let mouse = opts.mouse != MouseMode::Off && wl.has_pointer();
    if opts.mouse != MouseMode::Off && !wl.has_pointer() {
        log_warn!("no virtual pointer: mouse input is disabled");
    }
    let pixel_mouse = match opts.mouse {
        MouseMode::Pixels => true,
        MouseMode::Auto => caps.pixel_mouse,
        _ => false,
    };
    terminal.setup(mouse, mouse && pixel_mouse)?;

    let grid = TileGrid::new(geom.cells(), opts.tile.0, opts.tile.1);
    let now = Instant::now();
    let stats = Stats::new(opts.stats.as_deref(), compositor.as_ref().and_then(Compositor::pid))?;
    let no_window_check = (compositor.is_some() && opts.command.is_empty()).then(|| now + NO_WINDOW_WARNING);
    let mut app = App {
        term: terminal,
        renderer: Renderer::new(render_config(&opts, compat, large_png), grid),
        dirty: DirtyMap::new(grid.tiles_x(), grid.tiles_y()),
        frame_interval: Duration::from_secs(1) / opts.max_fps,
        wl,
        compositor: compositor.take(),
        signals,
        opts,
        parser: Parser::new(),
        keymap,
        vt: Vt::new(),
        fb: FrameBuf::new(0, 0),
        stats,
        geom,
        compat,
        large_png,
        pixel_mouse,
        full_next: true,
        next_capture: now,
        capture_started: now,
        capture_failures: 0,
        esc_deadline: None,
        resize: None,
        no_window_check,
        events: Vec::new(),
    };
    if let Some(n) = app.renderer.take_note() {
        log_warn!("{n}");
    }
    app.renderer.reset(&mut app.vt);
    app.flush_vt()?;
    let result = app.event_loop();
    if let Some(path) = &app.opts.dump_frame {
        if let Err(e) = app.fb.write_ppm(path) {
            log_warn!("--dump-frame {}: {e}", path.display());
        }
    }
    let summary = app.stats.summary();
    let restore = app.term.restore();
    drop(app);
    let exit = result?;
    restore?;
    Ok((exit, summary))
}

impl App {
    fn flush_vt(&mut self) -> Result<(), AppError> {
        if !self.vt.is_empty() {
            let buf = self.vt.take();
            self.stats.record_output(buf.len());
            self.term.writer().send(buf)?;
        }
        Ok(())
    }

    fn event_loop(&mut self) -> Result<Exit, AppError> {
        let stdin = stdin_fd();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let now = Instant::now();
            if let Some(exit) = self.timers(now)? {
                return Ok(exit);
            }
            if self.resize.is_none() && self.wl.capture_idle() && !self.term.writer().busy() && now >= self.next_capture
            {
                let with_damage = !self.full_next && self.opts.phase != Phase::Full;
                self.wl.start_capture(with_damage, self.opts.overlay_cursor);
                self.capture_started = now;
                log_debug!("capture requested (damage {with_damage})");
            }
            if let Err(e) = self.wl.flush().and_then(|_| self.wl.dispatch_pending()) {
                return self.wayland_gone(e);
            }
            self.handle_capture()?;

            // Requests issued while dispatching (e.g. screencopy `copy` after
            // `buffer_done`) must reach the compositor before we sleep.
            if let Err(e) = self.wl.flush() {
                return self.wayland_gone(e);
            }
            let timeout = self.poll_timeout(Instant::now());
            let (stdin_ev, sig_ev, done_ev) = {
                let Some(guard) = self.wl.prepare_read() else { continue };
                let writer = self.term.writer.as_ref().expect("writer active");
                // A full socket leaves requests queued; wake up when it has room again.
                let wl_flags = if self.wl.flush_pending() { PollFlags::IN | PollFlags::OUT } else { PollFlags::IN };
                let mut fds = [
                    PollFd::from_borrowed_fd(guard.connection_fd(), wl_flags),
                    PollFd::from_borrowed_fd(stdin, PollFlags::IN),
                    PollFd::new(self.signals.fd(), PollFlags::IN),
                    PollFd::from_borrowed_fd(writer.done_fd(), PollFlags::IN),
                ];
                match poll(&mut fds, Some(&term::timespec(timeout))) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(e) => return Err(io::Error::from(e).into()),
                }
                let rev: Vec<PollFlags> = fds.iter().map(PollFd::revents).collect();
                if !rev[0].is_empty() {
                    match guard.read() {
                        Ok(_) => {}
                        Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(e) => return self.wayland_gone(e.into()),
                    }
                }
                (rev[1], rev[2], rev[3])
            };
            if !done_ev.is_empty() {
                self.term.writer().on_done()?;
                log_debug!("writer done, busy {}", self.term.writer().busy());
            }
            if !sig_ev.is_empty() {
                for sig in self.signals.drain() {
                    if sig == libc::SIGWINCH {
                        self.resize = Some(Resize::Debounce(Instant::now() + RESIZE_DEBOUNCE));
                    } else {
                        log_info!("received {}", signals::name(sig));
                        return Ok(Exit::Signal(sig));
                    }
                }
            }
            if stdin_ev.contains(PollFlags::IN) {
                let n = match rustix::io::read(stdin, &mut buf) {
                    Ok(0) => return Ok(Exit::TerminalClosed),
                    Ok(n) => n,
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => 0,
                    Err(e) => return Err(io::Error::from(e).into()),
                };
                let mut events = std::mem::take(&mut self.events);
                self.parser.feed(&buf[..n], &mut events);
                let lone_esc = if self.opts.quit_key == QuitKey::Escape { QUIT_ESC_TIMEOUT } else { ESC_TIMEOUT };
                self.esc_deadline = self.parser.pending_timeout(lone_esc).map(|t| Instant::now() + t);
                let exit = self.handle_events(&mut events);
                self.events = events;
                match exit {
                    Ok(Some(exit)) => return Ok(exit),
                    Ok(None) => {}
                    Err(AppError::Wayland(e)) => return self.wayland_gone(e),
                    Err(e) => return Err(e),
                }
            } else if stdin_ev.intersects(PollFlags::HUP | PollFlags::ERR) {
                return Ok(Exit::TerminalClosed);
            }
        }
    }

    fn wayland_gone(&mut self, e: WlError) -> Result<Exit, AppError> {
        // Give the compositor a moment to finish exiting so we can tell a normal
        // application exit from a crash.
        let deadline = Instant::now() + Duration::from_millis(500);
        while let Some(c) = &mut self.compositor {
            if let Some(status) = c.try_wait() {
                let report = c.exit_report(status, self.wl.windows_seen());
                log_info!("{}", report.info);
                if let Some(w) = report.warning {
                    log_warn!("{w}");
                }
                return Ok(Exit::AppExited);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(e.into())
    }

    fn poll_timeout(&self, now: Instant) -> Duration {
        let mut next = self.stats.next_tick();
        let writer_idle = !self.term.writer.as_ref().is_some_and(Writer::busy);
        if writer_idle && self.wl.capture_idle() && self.resize.is_none() {
            next = next.min(self.next_capture);
        }
        if let Some(t) = self.static_refine_at() {
            next = next.min(t);
        }
        for t in [self.esc_deadline, self.resize_deadline(), self.no_window_check].into_iter().flatten() {
            next = next.min(t);
        }
        next.saturating_duration_since(now).min(Duration::from_secs(1))
    }

    /// Phase 4: once the compositor has sent no damage for the refine delay (our
    /// capture request is still waiting) and the link is idle, refine what is left.
    fn static_refine_at(&self) -> Option<Instant> {
        let writer_idle = !self.term.writer.as_ref().is_some_and(Writer::busy);
        if !writer_idle || !self.wl.capture_waiting() {
            return None;
        }
        let quiet_since = self.capture_started + self.opts.refine_delay;
        self.renderer.next_refine_at().map(|t| t.max(quiet_since))
    }

    fn resize_deadline(&self) -> Option<Instant> {
        match self.resize? {
            Resize::Debounce(t) => Some(t),
            Resize::AwaitCell { deadline, .. } => Some(deadline),
        }
    }

    fn timers(&mut self, now: Instant) -> Result<Option<Exit>, AppError> {
        if self.esc_deadline.is_some_and(|t| now >= t) {
            self.esc_deadline = None;
            let mut events = std::mem::take(&mut self.events);
            self.parser.flush(&mut events);
            let r = self.handle_events(&mut events);
            self.events = events;
            if let Some(exit) = r? {
                return Ok(Some(exit));
            }
        }
        match self.resize {
            Some(Resize::Debounce(t)) if now >= t => self.begin_resize()?,
            Some(Resize::AwaitCell { deadline, cols, rows }) if now >= deadline => {
                log_warn!(
                    "no reply to CSI 16 t after resize; keeping cell size {}x{}",
                    self.geom.cell_w,
                    self.geom.cell_h
                );
                self.apply_resize(cols, rows, (self.geom.cell_w, self.geom.cell_h))?;
            }
            _ => {}
        }
        if self.static_refine_at().is_some_and(|t| now >= t) {
            let st = self.render_with(false, |r, fb, vt| r.refine(fb, now, vt))?;
            log_debug!("refined {} tiles", st.refined_tiles);
        }
        if self.no_window_check.is_some_and(|t| now >= t) {
            self.no_window_check = None;
            if self.wl.windows_seen() == Some(0) {
                log_warn!(
                    "no window has opened: without a COMMAND termway starts no app, so the --sway-config file \
                     must start one (e.g. exec foot); sway does not read /etc/sway/config or ~/.config/sway/config then"
                );
            }
        }
        if now >= self.stats.next_tick() {
            let extra = Extra {
                cached_tiles: self.renderer.cached_tiles(),
                cache_bytes: self.renderer.cache_cost(),
                live_ephemerals: self.renderer.live_ephemerals(),
            };
            self.stats.tick(now, &extra)?;
            if self.opts.status_line {
                self.draw_status();
                self.flush_vt()?;
            }
        }
        Ok(None)
    }

    fn draw_status(&mut self) {
        let text: String = self.stats.last_line().chars().take(self.geom.cols.saturating_sub(1) as usize).collect();
        self.vt.move_to(self.geom.rows - 1, 0);
        self.vt.reset_sgr();
        self.vt.raw(b"\x1b[7m");
        self.vt.raw(text.as_bytes());
        self.vt.raw(b"\x1b[0m\x1b[K");
        self.vt.forget_state();
    }

    /// Runs a render step wrapped in synchronized output and queues the bytes.
    fn render_with(
        &mut self,
        captured: bool,
        f: impl FnOnce(&mut Renderer, &FrameBuf, &mut Vt) -> crate::render::FrameStats,
    ) -> Result<crate::render::FrameStats, AppError> {
        let t0 = Instant::now();
        debug_assert!(self.vt.is_empty());
        if self.opts.sync_output {
            self.vt.raw(vt::SYNC_BEGIN);
        }
        let before = self.vt.len();
        let st = f(&mut self.renderer, &self.fb, &mut self.vt);
        let produced = self.vt.len() > before;
        if self.opts.sync_output {
            self.vt.raw(vt::SYNC_END);
        }
        let bytes = if produced {
            let buf = self.vt.take();
            let n = buf.len();
            self.term.writer().send(buf)?;
            n
        } else {
            self.vt.buf.clear();
            0
        };
        self.stats.record_frame(bytes, &st, t0.elapsed(), captured);
        if let Some(n) = self.renderer.take_note() {
            log_warn!("{n}");
        }
        Ok(st)
    }

    fn handle_capture(&mut self) -> Result<(), AppError> {
        match self.wl.take_capture() {
            CaptureStatus::Pending => Ok(()),
            CaptureStatus::Failed => {
                self.capture_failures += 1;
                if self.capture_failures == 1 || self.capture_failures % 5 == 0 {
                    log_warn!("screencopy failed ({} in a row)", self.capture_failures);
                }
                if self.capture_failures > MAX_CAPTURE_FAILURES {
                    return Err(WlError::CaptureFailed.into());
                }
                self.full_next = true;
                // Back off (50 ms doubling to 1.6 s): an output switched off for a while
                // must not use up the failures within a second.
                let backoff = Duration::from_millis(50) * (1 << (self.capture_failures - 1).min(5));
                self.next_capture = Instant::now() + backoff;
                Ok(())
            }
            CaptureStatus::Ready(meta) => {
                let waited = self.capture_started.elapsed();
                self.capture_failures = 0;
                self.stats.record_capture();
                let mut full = self.full_next || self.opts.phase == Phase::Full;
                if (meta.width, meta.height) != (self.fb.width(), self.fb.height()) {
                    log_info!("frame size {}x{}", meta.width, meta.height);
                    self.fb = FrameBuf::new(meta.width, meta.height);
                    full = true;
                }
                let damage: Vec<Rect> = if full {
                    vec![self.fb.bounds()]
                } else if meta.y_invert {
                    meta.damage
                        .iter()
                        .map(|r| Rect::new(r.x, meta.height.saturating_sub(r.y.saturating_add(r.h)), r.w, r.h))
                        .collect()
                } else {
                    meta.damage.clone()
                };
                for r in &damage {
                    let (data, threads) = (self.wl.frame_data(), self.opts.threads);
                    self.fb.update_from_shm(data, meta.stride, meta.format, meta.y_invert, *r, threads)?;
                }
                self.dirty.clear();
                if full {
                    self.dirty.mark_all();
                } else {
                    damage_to_dirty(self.renderer.grid(), &damage, &mut self.dirty);
                }
                self.full_next = false;
                self.next_capture = self.capture_started + self.frame_interval;
                let now = Instant::now();
                let dirty = std::mem::replace(&mut self.dirty, DirtyMap::new(0, 0));
                let r = self.render_with(true, |r, fb, vt| r.render(fb, &dirty, now, vt));
                log_debug!(
                    "frame: waited {:.1} ms, encoded in {:.1} ms, {} damage rects, {} dirty tiles",
                    waited.as_secs_f64() * 1000.0,
                    now.elapsed().as_secs_f64() * 1000.0,
                    meta.damage.len(),
                    dirty.count()
                );
                self.dirty = dirty;
                r.map(|_| ())
            }
        }
    }

    /// Handles and clears `events` (the vector is kept for its allocation).
    fn handle_events(&mut self, events: &mut Vec<Event>) -> Result<Option<Exit>, AppError> {
        let r = self.handle_batch(events);
        events.clear();
        r
    }

    fn handle_batch(&mut self, events: &[Event]) -> Result<Option<Exit>, AppError> {
        let mut start = 0;
        while start < events.len() {
            // The keymap holds DYN_SLOTS characters outside the US layout; a paste may
            // bring more, so the keymap is switched between runs that fit.
            let mut dynamic: Vec<char> = Vec::new();
            let mut end = start;
            while end < events.len() {
                if let Event::Key(KeyEvent { key: Key::Char(c), .. }) = events[end] {
                    if Keymap::needs_dynamic(c) && !dynamic.contains(&c) {
                        if dynamic.len() == DYN_SLOTS {
                            break;
                        }
                        dynamic.push(c);
                    }
                }
                end += 1;
            }
            if self.keymap.prepare(dynamic) && self.wl.has_keyboard() {
                self.wl.upload_keymap(&self.keymap.text())?;
            }
            if let Some(exit) = self.handle_run(&events[start..end])? {
                return Ok(Some(exit));
            }
            start = end;
        }
        self.wl.flush()?;
        Ok(None)
    }

    fn handle_run(&mut self, events: &[Event]) -> Result<Option<Exit>, AppError> {
        for ev in events.iter().cloned() {
            match ev {
                Event::Key(k) => {
                    let quit = match self.opts.quit_key {
                        QuitKey::Escape => k.key == Key::Escape && k.mods == Mods::NONE,
                        QuitKey::CtrlBracket => k.key == Key::Char(']') && k.mods == Mods { ctrl: true, ..Mods::NONE },
                        QuitKey::None => false,
                    };
                    if quit {
                        return Ok(Some(Exit::QuitKey));
                    }
                    match self.keymap.stroke(&k) {
                        Some(s) => self.wl.tap_key(s.code, s.mods),
                        None => log_debug!("unmapped key {k:?}"),
                    }
                }
                Event::Mouse(m) => self.mouse(m),
                Event::Reply(Reply::CellSize { width, height }) if width > 0 && height > 0 => match self.resize {
                    Some(Resize::AwaitCell { cols, rows, .. }) => self.apply_resize(cols, rows, (width, height))?,
                    // A late answer (the query queued behind a slow frame) still counts.
                    None if (width, height) != (self.geom.cell_w, self.geom.cell_h)
                        && self.opts.cell_size.is_none() =>
                    {
                        log_info!("late CSI 16 t answer: cells are {width}x{height} px");
                        self.apply_resize(self.geom.cols, self.geom.rows, (width, height))?;
                    }
                    _ => {}
                },
                Event::Reply(r) => log_debug!("terminal reply {r:?}"),
            }
        }
        Ok(None)
    }

    fn mouse(&mut self, m: crate::input::MouseEvent) {
        let (ow, oh) = if self.fb.width() > 0 { (self.fb.width(), self.fb.height()) } else { self.geom.output_px() };
        let (x, y) = if self.pixel_mouse {
            (m.x.saturating_sub(1), m.y.saturating_sub(1))
        } else {
            let g = &self.geom;
            (m.x.saturating_sub(1) * g.cell_w + g.cell_w / 2, m.y.saturating_sub(1) * g.cell_h + g.cell_h / 2)
        };
        self.wl.pointer_motion(x.min(ow.saturating_sub(1)), y.min(oh.saturating_sub(1)), (ow, oh));
        let button = |b| match b {
            MouseButton::Left => BTN_LEFT,
            MouseButton::Right => BTN_RIGHT,
            MouseButton::Middle => BTN_MIDDLE,
            MouseButton::Back => BTN_SIDE,
            MouseButton::Forward => BTN_EXTRA,
        };
        match m.action {
            MouseAction::Press(b) => self.wl.pointer_button(button(b), true),
            MouseAction::Release(b) => self.wl.pointer_button(button(b), false),
            MouseAction::Wheel { dx, dy } => self.wl.pointer_scroll(dx, dy),
            MouseAction::Motion => {}
        }
    }

    fn begin_resize(&mut self) -> Result<(), AppError> {
        self.resize = None;
        let ws = match term::grid_size(stdout_fd()) {
            Ok(ws) => ws,
            Err(e) => {
                log_warn!("ignoring resize: {e}");
                return Ok(());
            }
        };
        let (cols, rows) = (ws.ws_col as u32, ws.ws_row as u32);
        if let Some(cell) = self.opts.cell_size {
            self.apply_resize(cols, rows, cell)
        } else if ws.ws_xpixel > 0 && ws.ws_ypixel > 0 {
            self.apply_resize(cols, rows, (ws.ws_xpixel as u32 / cols, ws.ws_ypixel as u32 / rows))
        } else {
            self.vt.raw(b"\x1b[16t");
            self.flush_vt()?;
            self.resize = Some(Resize::AwaitCell { deadline: Instant::now() + HANDSHAKE_TIMEOUT, cols, rows });
            Ok(())
        }
    }

    fn apply_resize(&mut self, cols: u32, rows: u32, cell: (u32, u32)) -> Result<(), AppError> {
        self.resize = None;
        let geom = match Geometry::new(cols, rows, cell, self.opts.status_line) {
            Ok(g) => g,
            Err(e) => {
                log_warn!("ignoring resize: {e}");
                return Ok(());
            }
        };
        log_info!(
            "resize to {cols}x{rows} cells of {}x{} px, scale {}",
            cell.0,
            cell.1,
            self.opts.scale.resolve(cell.1)
        );
        // Terminals may reflow or drop placements on resize: start from a clean slate.
        self.renderer.reset(&mut self.vt);
        let grid = TileGrid::new(geom.cells(), self.opts.tile.0, self.opts.tile.1);
        self.renderer = Renderer::new(render_config(&self.opts, self.compat, self.large_png), grid);
        self.dirty = DirtyMap::new(grid.tiles_x(), grid.tiles_y());
        self.flush_vt()?;
        self.geom = geom;
        if self.opts.resize_output && self.wl.can_resize() {
            self.wl.cancel_capture();
            if let Err(e) = self.wl.set_output_mode(output_mode(&self.opts, &geom)) {
                log_warn!("{e}");
            }
        }
        self.full_next = true;
        self.next_capture = Instant::now();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fd::{AsRawFd, OwnedFd};
    use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
    use rustix::termios::{self, LocalModes};

    fn pty() -> (OwnedFd, OwnedFd) {
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
        (master, slave)
    }

    fn read_all(master: &OwnedFd) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let mut fds = [PollFd::new(master, PollFlags::IN)];
            if poll(&mut fds, Some(&term::timespec(Duration::from_millis(200)))).unwrap() == 0 {
                return out;
            }
            let n = rustix::io::read(master, &mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn panic_restores_terminal() {
        let (master, slave) = pty();
        // SAFETY: `slave` outlives every use of the borrowed fd in this test.
        let fd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(slave.as_raw_fd()) };
        let mut t =
            Terminal { out: fd, raw: RawMode::enter(fd).unwrap(), writer: None, active: false, compat: Compat::Kitty };
        t.setup(true, true).unwrap();
        t.writer().send(b"\x1b_Ga=T,f=24,s=1,v=1,i=5,q=2,C=1;AAAA\x1b\\".to_vec()).unwrap();
        assert!(!termios::tcgetattr(&slave).unwrap().local_modes.contains(LocalModes::ICANON));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _t = t;
            panic!("simulated failure");
        }));
        assert!(r.is_err());
        let out = read_all(&master);
        assert!(out.starts_with(b"\x1b[?1049h"), "setup first");
        assert!(out.ends_with(&restore_sequence(Compat::Kitty)), "restore sequence after the queued frame");
        let mut sim = crate::sim::SimTerm::new(80, 24, 9, 21);
        sim.feed(&out);
        assert!(sim.errors.is_empty(), "{:?}", sim.errors);
        assert!(!sim.alt_screen);
        let modes = termios::tcgetattr(&slave).unwrap().local_modes;
        assert!(modes.contains(LocalModes::ICANON | LocalModes::ECHO), "termios restored");
    }

    #[test]
    fn geometry_limits() {
        assert!(Geometry::new(282, 60, (9, 21), false).is_ok());
        assert_eq!(Geometry::new(282, 60, (9, 21), true).unwrap().output_px(), (2538, 1239));
        assert!(matches!(Geometry::new(80, 1, (9, 21), true), Err(AppError::Config(_))));
        assert!(matches!(Geometry::new(2000, 60, (9, 21), false), Err(AppError::Config(_))));
        assert!(matches!(Geometry::new(80, 24, (0, 21), false), Err(AppError::Config(_))));
        // 80 * 53687092 wraps to 64 in u32: the overflow must be caught, not wrapped.
        assert!(matches!(Geometry::new(80, 24, (53_687_092, 21), false), Err(AppError::Config(_))));
    }

    #[test]
    fn output_mode_scales_with_hidpi_cells() {
        let o = Options::default();
        let m = output_mode(&o, &Geometry::new(120, 30, (20, 42), false).unwrap());
        assert_eq!(m, OutputMode { width: 2400, height: 1260, refresh_hz: 60, scale: 2.0 });
        let o = Options { scale: crate::cli::Scale::Fixed(1.0), ..o };
        assert_eq!(output_mode(&o, &Geometry::new(120, 30, (20, 42), false).unwrap()).scale, 1.0);
    }
}
