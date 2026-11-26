//! Wayland client side: wlr-screencopy capture into shm, virtual keyboard / pointer,
//! output resizing through wlr-output-management, and counting the app's windows
//! through wlr-foreign-toplevel-management.

use crate::compositor::OutputMode;
use crate::frame::PixelFormat;
use crate::geom::Rect;
use crate::term::timespec;
use rustix::event::{poll, PollFd, PollFlags};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use wayland_client::backend::ReadEventsGuard;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
};
use wayland_client::{delegate_noop, event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Debug)]
pub enum WlError {
    Connect(String),
    MissingProtocol(&'static str),
    NoOutput(String),
    Protocol(String),
    Io(std::io::Error),
    Timeout(&'static str),
    ModeSet(String),
    UnsupportedFormat(Vec<u32>),
    CaptureFailed,
}

impl std::fmt::Display for WlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "cannot connect to the Wayland compositor: {e}"),
            Self::MissingProtocol(p) => write!(f, "compositor does not offer {p}"),
            Self::NoOutput(o) => write!(f, "no Wayland output {o}"),
            Self::Protocol(e) => write!(f, "Wayland protocol error: {e}"),
            Self::Io(e) => write!(f, "Wayland I/O error: {e}"),
            Self::Timeout(what) => write!(f, "timed out waiting for the compositor ({what})"),
            Self::ModeSet(e) => write!(f, "failed to set output mode: {e}"),
            Self::UnsupportedFormat(f_) => write!(f, "screencopy offers no supported shm format (got {f_:x?})"),
            Self::CaptureFailed => write!(f, "screencopy failed repeatedly"),
        }
    }
}

impl std::error::Error for WlError {}

impl From<std::io::Error> for WlError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<wayland_client::backend::WaylandError> for WlError {
    fn from(e: wayland_client::backend::WaylandError) -> Self {
        match e {
            wayland_client::backend::WaylandError::Io(e) => Self::Io(e),
            e => Self::Protocol(e.to_string()),
        }
    }
}

impl From<wayland_client::DispatchError> for WlError {
    fn from(e: wayland_client::DispatchError) -> Self {
        match e {
            wayland_client::DispatchError::Backend(e) => e.into(),
            e => Self::Protocol(e.to_string()),
        }
    }
}

impl WlError {
    /// The connection is gone, typically because the compositor exited.
    pub fn is_disconnect(&self) -> bool {
        use std::io::ErrorKind::{BrokenPipe, ConnectionReset};
        matches!(self, Self::Io(e) if matches!(e.kind(), BrokenPipe | ConnectionReset))
    }
}

/// Protocol availability, reported at startup.
#[derive(Clone, Debug, Default)]
pub struct Protocols {
    pub screencopy: u32,
    pub image_copy_capture: bool,
    pub virtual_keyboard: bool,
    pub virtual_pointer: u32,
    pub output_management: u32,
    pub foreign_toplevel: u32,
}

impl std::fmt::Display for Protocols {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = |n: u32| if n == 0 { "missing".to_string() } else { format!("v{n}") };
        write!(
            f,
            "zwlr_screencopy_manager_v1 {}, ext_image_copy_capture_manager_v1 {}, zwp_virtual_keyboard_manager_v1 {}, \
             zwlr_virtual_pointer_manager_v1 {}, zwlr_output_manager_v1 {}, zwlr_foreign_toplevel_manager_v1 {}",
            v(self.screencopy),
            if self.image_copy_capture { "present (unused)" } else { "missing" },
            if self.virtual_keyboard { "present" } else { "missing" },
            v(self.virtual_pointer),
            v(self.output_management),
            v(self.foreign_toplevel),
        )
    }
}

struct ShmBuffer {
    _fd: OwnedFd,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    ptr: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

impl ShmBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<WlState>,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> Result<Self, WlError> {
        let len = stride as usize * height as usize;
        if len == 0 || len > i32::MAX as usize {
            return Err(WlError::Protocol(format!("invalid screencopy buffer {width}x{height} stride {stride}")));
        }
        let fd =
            rustix::fs::memfd_create("termway-frame", rustix::fs::MemfdFlags::CLOEXEC).map_err(std::io::Error::from)?;
        rustix::fs::ftruncate(&fd, len as u64).map_err(std::io::Error::from)?;
        // SAFETY: fresh shared mapping of a memfd we own, unmapped in Drop.
        let ptr =
            unsafe { mmap(std::ptr::null_mut(), len, ProtFlags::READ | ProtFlags::WRITE, MapFlags::SHARED, &fd, 0) }
                .map_err(std::io::Error::from)? as *mut u8;
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let wl_format = WEnum::<wl_shm::Format>::from(format);
        let WEnum::Value(fmt) = wl_format else {
            return Err(WlError::UnsupportedFormat(vec![format]));
        };
        let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, fmt, qh, ());
        Ok(Self { _fd: fd, pool, buffer, ptr, len, width, height, stride, format })
    }

    fn data(&self) -> &[u8] {
        // SAFETY: the mapping lives as long as self; the compositor only writes into it
        // between a copy request and the ready event, and we only read after ready.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: ptr/len come from the mmap in `new`.
        unsafe {
            let _ = munmap(self.ptr as *mut _, self.len);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct OfferedBuffer {
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Default)]
enum CaptureState {
    #[default]
    Idle,
    /// Waiting for buffer parameters.
    Negotiating {
        frame: ZwlrScreencopyFrameV1,
        with_damage: bool,
        offers: Vec<OfferedBuffer>,
    },
    Copying {
        frame: ZwlrScreencopyFrameV1,
    },
    Ready,
    Failed,
}

pub enum CaptureStatus {
    Pending,
    Ready(CapturedFrame),
    Failed,
}

/// Metadata of a completed capture; pixels are in [`Wayland::frame_data`].
#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub y_invert: bool,
    pub damage: Vec<Rect>,
}

struct Head {
    head: ZwlrOutputHeadV1,
    name: Option<String>,
    enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigResult {
    Succeeded,
    Failed,
    Cancelled,
}

pub struct WlState {
    shm: wl_shm::WlShm,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    output_sizes: Vec<(i32, i32)>,
    heads: Vec<Head>,
    manager_serial: Option<u32>,
    /// The output configuration awaiting its result; answers for others are ignored.
    config: Option<ZwlrOutputConfigurationV1>,
    config_result: Option<ConfigResult>,
    capture: CaptureState,
    buffer: Option<ShmBuffer>,
    manager_version: u32,
    y_invert: bool,
    damage: Vec<Rect>,
    synced: bool,
    error: Option<WlError>,
    /// Toplevels announced so far, closed ones included.
    windows_seen: u32,
}

pub struct Wayland {
    conn: Connection,
    queue: EventQueue<WlState>,
    qh: QueueHandle<WlState>,
    state: WlState,
    output: wl_output::WlOutput,
    output_name: String,
    screencopy: ZwlrScreencopyManagerV1,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    pointer: Option<ZwlrVirtualPointerV1>,
    output_manager: Option<ZwlrOutputManagerV1>,
    toplevel_manager: Option<ZwlrForeignToplevelManagerV1>,
    protocols: Protocols,
    start: Instant,
    /// Requests are still queued because the socket was full.
    flush_pending: std::cell::Cell<bool>,
}

const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

impl Wayland {
    /// `count_windows`: track the windows the app opens (only for a compositor we started).
    pub fn connect(stream: UnixStream, want_output: Option<&str>, count_windows: bool) -> Result<Self, WlError> {
        let conn = Connection::from_socket(stream).map_err(|e| WlError::Connect(e.to_string()))?;
        let (globals, queue) = registry_queue_init::<WlState>(&conn).map_err(|e| WlError::Connect(e.to_string()))?;
        let qh = queue.handle();
        let list = globals.contents().clone_list();
        let version = |iface: &str| list.iter().filter(|g| g.interface == iface).map(|g| g.version).max().unwrap_or(0);
        let mut protocols = Protocols {
            screencopy: version(ZwlrScreencopyManagerV1::interface().name).min(3),
            image_copy_capture: version("ext_image_copy_capture_manager_v1") > 0,
            virtual_keyboard: version(ZwpVirtualKeyboardManagerV1::interface().name) > 0,
            virtual_pointer: version(ZwlrVirtualPointerManagerV1::interface().name).min(2),
            output_management: version(ZwlrOutputManagerV1::interface().name).min(4),
            foreign_toplevel: version(ZwlrForeignToplevelManagerV1::interface().name).min(3),
        };
        if protocols.screencopy < 2 {
            return Err(WlError::MissingProtocol("zwlr_screencopy_manager_v1 version >= 2 (copy_with_damage)"));
        }
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|_| WlError::MissingProtocol("wl_shm"))?;
        let screencopy: ZwlrScreencopyManagerV1 =
            globals.bind(&qh, 2..=3, ()).map_err(|_| WlError::MissingProtocol("zwlr_screencopy_manager_v1"))?;
        let seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=7, ()).ok();
        let outputs: Vec<(wl_output::WlOutput, Option<String>)> = list
            .iter()
            .filter(|g| g.interface == wl_output::WlOutput::interface().name)
            .map(|g| (globals.registry().bind::<wl_output::WlOutput, _, _>(g.name, g.version.min(4), &qh, ()), None))
            .collect();
        let output_manager: Option<ZwlrOutputManagerV1> =
            if protocols.output_management > 0 { globals.bind(&qh, 1..=4, ()).ok() } else { None };
        let toplevel_manager: Option<ZwlrForeignToplevelManagerV1> =
            if count_windows && protocols.foreign_toplevel > 0 { globals.bind(&qh, 1..=3, ()).ok() } else { None };
        let mut state = WlState {
            shm,
            output_sizes: vec![(0, 0); outputs.len()],
            outputs,
            heads: Vec::new(),
            manager_serial: None,
            config: None,
            config_result: None,
            capture: CaptureState::Idle,
            buffer: None,
            manager_version: screencopy.version(),
            y_invert: false,
            damage: Vec::new(),
            synced: false,
            error: None,
            windows_seen: 0,
        };
        let mut this_queue = queue;
        sync(&conn, &mut this_queue, &qh, &mut state)?;
        // Output names arrive after binding; a second sync covers late head/mode events.
        sync(&conn, &mut this_queue, &qh, &mut state)?;

        let idx = match want_output {
            Some(name) => state
                .outputs
                .iter()
                .position(|(_, n)| n.as_deref() == Some(name))
                .ok_or_else(|| WlError::NoOutput(name.to_string()))?,
            None if state.outputs.is_empty() => return Err(WlError::NoOutput("(none advertised)".into())),
            None => 0,
        };
        let (output, name) = state.outputs[idx].clone();
        let output_name = name.unwrap_or_else(|| format!("output-{idx}"));

        let keyboard = match (&seat, protocols.virtual_keyboard) {
            (Some(seat), true) => {
                let m: ZwpVirtualKeyboardManagerV1 =
                    globals.bind(&qh, 1..=1, ()).map_err(|_| WlError::MissingProtocol("vk"))?;
                Some(m.create_virtual_keyboard(seat, &qh, ()))
            }
            _ => None,
        };
        let pointer = match protocols.virtual_pointer {
            0 => None,
            v => {
                let m: ZwlrVirtualPointerManagerV1 =
                    globals.bind(&qh, 1..=2, ()).map_err(|_| WlError::MissingProtocol("vp"))?;
                Some(if v >= 2 {
                    m.create_virtual_pointer_with_output(seat.as_ref(), Some(&output), &qh, ())
                } else {
                    m.create_virtual_pointer(seat.as_ref(), &qh, ())
                })
            }
        };
        if seat.is_none() {
            protocols.virtual_keyboard = false;
        }
        Ok(Self {
            conn,
            queue: this_queue,
            qh,
            state,
            output,
            output_name,
            screencopy,
            keyboard,
            pointer,
            output_manager,
            toplevel_manager,
            protocols,
            start: Instant::now(),
            flush_pending: std::cell::Cell::new(false),
        })
    }

    pub fn protocols(&self) -> &Protocols {
        &self.protocols
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub fn has_keyboard(&self) -> bool {
        self.keyboard.is_some()
    }

    pub fn has_pointer(&self) -> bool {
        self.pointer.is_some()
    }

    pub fn can_resize(&self) -> bool {
        self.output_manager.is_some()
    }

    /// Windows opened in the compositor so far (`None`: it cannot list them).
    pub fn windows_seen(&self) -> Option<u32> {
        self.toplevel_manager.as_ref().map(|_| self.state.windows_seen)
    }

    pub fn flush(&self) -> Result<(), WlError> {
        let pending = flush_nonblocking(&self.conn)?;
        self.flush_pending.set(pending);
        Ok(())
    }

    /// Requests are waiting for room in the socket: poll it for writing too.
    pub fn flush_pending(&self) -> bool {
        self.flush_pending.get()
    }

    pub fn prepare_read(&self) -> Option<ReadEventsGuard> {
        self.queue.prepare_read()
    }

    pub fn dispatch_pending(&mut self) -> Result<(), WlError> {
        self.queue.dispatch_pending(&mut self.state)?;
        match self.state.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn time_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    pub fn capture_idle(&self) -> bool {
        matches!(self.state.capture, CaptureState::Idle)
    }

    /// A capture request is outstanding (with damage tracking: waiting for a change).
    pub fn capture_waiting(&self) -> bool {
        matches!(self.state.capture, CaptureState::Negotiating { .. } | CaptureState::Copying { .. })
    }

    /// Requests the next frame; `with_damage` waits until the output changed.
    pub fn start_capture(&mut self, with_damage: bool, overlay_cursor: bool) {
        debug_assert!(self.capture_idle());
        let frame = self.screencopy.capture_output(overlay_cursor as i32, &self.output, &self.qh, ());
        self.state.damage.clear();
        self.state.y_invert = false;
        self.state.capture = CaptureState::Negotiating { frame, with_damage, offers: Vec::new() };
    }

    /// Drops an outstanding capture. wlroots fails captures that span a mode change,
    /// so cancelling first keeps that expected failure out of the failure count.
    pub fn cancel_capture(&mut self) {
        match std::mem::take(&mut self.state.capture) {
            CaptureState::Negotiating { frame, .. } | CaptureState::Copying { frame } => frame.destroy(),
            CaptureState::Idle | CaptureState::Ready | CaptureState::Failed => {}
        }
    }

    pub fn take_capture(&mut self) -> CaptureStatus {
        match self.state.capture {
            CaptureState::Ready => {
                self.state.capture = CaptureState::Idle;
                let b = self.state.buffer.as_ref().expect("Ready is only entered with a buffer");
                let format = PixelFormat::from_wl_shm(b.format).expect("negotiated format is supported");
                CaptureStatus::Ready(CapturedFrame {
                    width: b.width,
                    height: b.height,
                    stride: b.stride as usize,
                    format,
                    y_invert: self.state.y_invert,
                    damage: std::mem::take(&mut self.state.damage),
                })
            }
            CaptureState::Failed => {
                self.state.capture = CaptureState::Idle;
                CaptureStatus::Failed
            }
            _ => CaptureStatus::Pending,
        }
    }

    pub fn frame_data(&self) -> &[u8] {
        self.state.buffer.as_ref().map_or(&[], |b| b.data())
    }

    /// Current mode size of the captured output as last reported by wl_output.
    pub fn output_size(&self) -> Option<(u32, u32)> {
        let idx = self.state.outputs.iter().position(|(o, _)| o == &self.output)?;
        let (w, h) = self.state.output_sizes[idx];
        (w > 0 && h > 0).then_some((w as u32, h as u32))
    }

    /// Sets a custom mode and scale on the captured output and waits for the result.
    pub fn set_output_mode(&mut self, mode: OutputMode) -> Result<(), WlError> {
        let OutputMode { width, height, refresh_hz, scale } = mode;
        let Some(manager) = self.output_manager.clone() else {
            return Err(WlError::MissingProtocol("zwlr_output_manager_v1"));
        };
        for _attempt in 0..3 {
            let serial = self.state.manager_serial.ok_or(WlError::ModeSet("no output configuration serial".into()))?;
            let target = self
                .state
                .heads
                .iter()
                .position(|h| h.name.as_deref() == Some(self.output_name.as_str()))
                .or_else(|| self.state.heads.iter().position(|h| h.enabled))
                .ok_or_else(|| WlError::ModeSet("no output head".into()))?;
            let cfg = manager.create_configuration(serial, &self.qh, ());
            for (i, h) in self.state.heads.iter().enumerate() {
                if h.enabled || i == target {
                    let ch = cfg.enable_head(&h.head, &self.qh, ());
                    if i == target {
                        ch.set_custom_mode(width as i32, height as i32, refresh_hz as i32 * 1000);
                        ch.set_scale(scale);
                    }
                }
            }
            cfg.apply();
            self.state.config_result = None;
            self.state.config = Some(cfg.clone());
            let waited = self.wait_until(|s| s.config_result.is_some(), "output configuration");
            self.state.config = None;
            cfg.destroy();
            waited?;
            match self.state.config_result.take() {
                Some(ConfigResult::Succeeded) => {
                    self.sync()?;
                    return Ok(());
                }
                Some(ConfigResult::Failed) => {
                    return Err(WlError::ModeSet(format!("compositor rejected {width}x{height} at scale {scale}")))
                }
                // The configuration changed under us; retry with the new serial.
                _ => self.sync()?,
            }
        }
        Err(WlError::ModeSet("configuration kept being cancelled".into()))
    }

    fn sync(&mut self) -> Result<(), WlError> {
        sync(&self.conn, &mut self.queue, &self.qh, &mut self.state)
    }

    fn wait_until(&mut self, pred: impl Fn(&WlState) -> bool, what: &'static str) -> Result<(), WlError> {
        wait_until(&self.conn, &mut self.queue, &mut self.state, pred, what)
    }

    pub fn upload_keymap(&mut self, text: &str) -> Result<(), WlError> {
        let Some(kb) = &self.keyboard else { return Ok(()) };
        let fd = rustix::fs::memfd_create("termway-keymap", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(std::io::Error::from)?;
        let mut data = text.as_bytes().to_vec();
        data.push(0);
        crate::term::write_all(fd.as_fd(), &data)?;
        kb.keymap(wayland_client::protocol::wl_keyboard::KeymapFormat::XkbV1.into(), fd.as_fd(), data.len() as u32);
        Ok(())
    }

    /// Presses and releases a key with the given modifier mask held.
    pub fn tap_key(&mut self, code: u32, mods: u32) {
        let t = self.time_ms();
        let Some(kb) = &self.keyboard else { return };
        if mods != 0 {
            kb.modifiers(mods, 0, 0, 0);
        }
        kb.key(t, code, 1);
        kb.key(t, code, 0);
        if mods != 0 {
            kb.modifiers(0, 0, 0, 0);
        }
    }

    pub fn pointer_motion(&mut self, x: u32, y: u32, extent: (u32, u32)) {
        let t = self.time_ms();
        if let Some(p) = &self.pointer {
            p.motion_absolute(
                t,
                x.min(extent.0.saturating_sub(1)),
                y.min(extent.1.saturating_sub(1)),
                extent.0,
                extent.1,
            );
            p.frame();
        }
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        let t = self.time_ms();
        if let Some(p) = &self.pointer {
            let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
            p.button(t, button, state);
            p.frame();
        }
    }

    pub fn pointer_scroll(&mut self, dx: i32, dy: i32) {
        let t = self.time_ms();
        let Some(p) = &self.pointer else { return };
        p.axis_source(wl_pointer::AxisSource::Wheel);
        if dy != 0 {
            p.axis_discrete(t, wl_pointer::Axis::VerticalScroll, 15.0 * dy as f64, dy);
        }
        if dx != 0 {
            p.axis_discrete(t, wl_pointer::Axis::HorizontalScroll, 15.0 * dx as f64, dx);
        }
        p.frame();
    }
}

fn sync(
    conn: &Connection,
    queue: &mut EventQueue<WlState>,
    qh: &QueueHandle<WlState>,
    state: &mut WlState,
) -> Result<(), WlError> {
    state.synced = false;
    conn.display().sync(qh, ());
    wait_until(conn, queue, state, |s| s.synced, "sync")
}

/// Dispatches events until `pred` holds, with a timeout so a hung compositor is reported.
/// Flushes queued requests; returns true if some are left because the socket is full.
fn flush_nonblocking(conn: &Connection) -> Result<bool, WlError> {
    match conn.flush() {
        Ok(()) => Ok(false),
        Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(e) => Err(e.into()),
    }
}

fn wait_until(
    conn: &Connection,
    queue: &mut EventQueue<WlState>,
    state: &mut WlState,
    pred: impl Fn(&WlState) -> bool,
    what: &'static str,
) -> Result<(), WlError> {
    let deadline = Instant::now() + SYNC_TIMEOUT;
    loop {
        queue.dispatch_pending(state)?;
        if let Some(e) = state.error.take() {
            return Err(e);
        }
        if pred(state) {
            return Ok(());
        }
        let unsent = flush_nonblocking(conn)?;
        let Some(guard) = queue.prepare_read() else { continue };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(WlError::Timeout(what));
        }
        let ready = {
            let fd: BorrowedFd = guard.connection_fd();
            let flags = if unsent { PollFlags::IN | PollFlags::OUT } else { PollFlags::IN };
            let mut fds = [PollFd::from_borrowed_fd(fd, flags)];
            match poll(&mut fds, Some(&timespec(left))) {
                Ok(n) => n > 0,
                Err(rustix::io::Errno::INTR) => false,
                Err(e) => return Err(WlError::Io(e.into())),
            }
        };
        if ready {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WlState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        ev: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = ev {
            state.synced = true;
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for WlState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        ev: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(idx) = state.outputs.iter().position(|(o, _)| o == output) else { return };
        match ev {
            wl_output::Event::Name { name } => state.outputs[idx].1 = Some(name),
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. }
                if f.contains(wl_output::Mode::Current) =>
            {
                state.output_sizes[idx] = (width, height)
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for WlState {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        ev: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event as E;
        let current = match &state.capture {
            CaptureState::Negotiating { frame: f, .. } | CaptureState::Copying { frame: f } => f == frame,
            _ => false,
        };
        if !current {
            // A cancelled capture (e.g. around a mode change): its late events must not
            // decide the fate of the current one.
            if matches!(ev, E::Ready { .. } | E::Failed) {
                frame.destroy();
            }
            return;
        }
        match ev {
            E::Buffer { format, width, height, stride } => {
                let format: u32 = format.into();
                if let CaptureState::Negotiating { offers, .. } = &mut state.capture {
                    offers.push(OfferedBuffer { format, width, height, stride });
                }
                if state.manager_version < 3 {
                    state.begin_copy(qh);
                }
            }
            E::BufferDone => state.begin_copy(qh),
            E::Flags { flags: WEnum::Value(f) } => {
                state.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert)
            }
            E::Damage { x, y, width, height } => state.damage.push(Rect::new(x, y, width, height)),
            E::Ready { .. } => {
                frame.destroy();
                // `ready` before we sent `copy` would be a compositor bug; treat it as a failure.
                let copying = matches!(state.capture, CaptureState::Copying { .. }) && state.buffer.is_some();
                state.capture = if copying { CaptureState::Ready } else { CaptureState::Failed };
            }
            E::Failed => {
                frame.destroy();
                state.capture = CaptureState::Failed;
            }
            _ => {}
        }
    }
}

impl WlState {
    fn begin_copy(&mut self, qh: &QueueHandle<Self>) {
        let CaptureState::Negotiating { frame, with_damage, offers } = std::mem::take(&mut self.capture) else {
            return;
        };
        let Some(offer) = offers.iter().find(|o| PixelFormat::from_wl_shm(o.format).is_some()).copied() else {
            frame.destroy();
            self.error = Some(WlError::UnsupportedFormat(offers.iter().map(|o| o.format).collect()));
            self.capture = CaptureState::Failed;
            return;
        };
        let reuse = self.buffer.as_ref().is_some_and(|b| {
            (b.width, b.height, b.stride, b.format) == (offer.width, offer.height, offer.stride, offer.format)
        });
        if !reuse {
            self.buffer = None;
            match ShmBuffer::new(&self.shm, qh, offer.width, offer.height, offer.stride, offer.format) {
                Ok(b) => self.buffer = Some(b),
                Err(e) => {
                    frame.destroy();
                    self.error = Some(e);
                    self.capture = CaptureState::Failed;
                    return;
                }
            }
        }
        let buffer = &self.buffer.as_ref().expect("buffer allocated above").buffer;
        if with_damage {
            frame.copy_with_damage(buffer);
        } else {
            frame.copy(buffer);
        }
        self.capture = CaptureState::Copying { frame };
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        ev: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push(Head { head, name: None, enabled: false }),
            zwlr_output_manager_v1::Event::Done { serial } => state.manager_serial = Some(serial),
            _ => {}
        }
    }

    event_created_child!(WlState, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for WlState {
    fn event(
        state: &mut Self,
        head: &ZwlrOutputHeadV1,
        ev: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(h) = state.heads.iter_mut().find(|h| &h.head == head) else { return };
        match ev {
            zwlr_output_head_v1::Event::Name { name } => h.name = Some(name),
            zwlr_output_head_v1::Event::Enabled { enabled } => h.enabled = enabled != 0,
            zwlr_output_head_v1::Event::Finished => {
                if head.version() >= 3 {
                    head.release();
                }
                state.heads.retain(|h| &h.head != head);
            }
            _ => {}
        }
    }

    event_created_child!(WlState, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for WlState {
    fn event(
        _: &mut Self,
        mode: &ZwlrOutputModeV1,
        ev: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let (zwlr_output_mode_v1::Event::Finished, 3..) = (ev, mode.version()) {
            mode.release();
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        ev: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { .. } = ev {
            state.windows_seen += 1;
        }
    }

    event_created_child!(WlState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for WlState {
    fn event(
        _: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        ev: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_handle_v1::Event::Closed = ev {
            handle.destroy();
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for WlState {
    fn event(
        state: &mut Self,
        cfg: &ZwlrOutputConfigurationV1,
        ev: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.config.as_ref() != Some(cfg) {
            // A configuration abandoned after a timeout.
            return;
        }
        state.config_result = Some(match ev {
            zwlr_output_configuration_v1::Event::Succeeded => ConfigResult::Succeeded,
            zwlr_output_configuration_v1::Event::Failed => ConfigResult::Failed,
            _ => ConfigResult::Cancelled,
        });
    }
}

delegate_noop!(WlState: ignore wl_shm::WlShm);
delegate_noop!(WlState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WlState: ignore wl_buffer::WlBuffer);
delegate_noop!(WlState: ignore wl_seat::WlSeat);
delegate_noop!(WlState: ignore ZwlrScreencopyManagerV1);
delegate_noop!(WlState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(WlState: ignore ZwpVirtualKeyboardV1);
delegate_noop!(WlState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(WlState: ignore ZwlrVirtualPointerV1);
delegate_noop!(WlState: ignore ZwlrOutputConfigurationHeadV1);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use wayland_client::backend::WaylandError;

    #[test]
    fn lost_connection_is_a_disconnect() {
        let e = WlError::from(WaylandError::Io(ErrorKind::BrokenPipe.into()));
        assert!(e.is_disconnect(), "{e}");
        assert_eq!(e.to_string(), "Wayland I/O error: broken pipe");
        let e =
            WlError::from(wayland_client::DispatchError::Backend(WaylandError::Io(ErrorKind::ConnectionReset.into())));
        assert!(e.is_disconnect(), "{e}");
        assert!(!WlError::Io(ErrorKind::OutOfMemory.into()).is_disconnect());
        assert!(!WlError::Timeout("output configuration").is_disconnect());
    }
}
