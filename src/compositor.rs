//! Launches a headless wlroots compositor (sway or cage) in a private runtime directory
//! so its socket name is predictable and it cannot clash with other sessions.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sway,
    Cage,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sway" => Some(Self::Sway),
            "cage" => Some(Self::Cage),
            _ => None,
        }
    }

    pub fn binary(self) -> &'static str {
        match self {
            Self::Sway => "sway",
            Self::Cage => "cage",
        }
    }
}

#[derive(Debug)]
pub enum CompositorError {
    NoRuntimeDir,
    Spawn { binary: &'static str, err: io::Error },
    Io(io::Error),
    Exited { status: ExitStatus, log_tail: String },
    NoSocket { log_tail: String },
    SwayConfig { path: PathBuf, err: io::Error },
    MissingProgram { program: &'static str, hint: &'static str },
}

impl std::fmt::Display for CompositorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRuntimeDir => write!(f, "XDG_RUNTIME_DIR is not set"),
            Self::Spawn { binary, err } => {
                write!(f, "cannot start {binary}: {err} (is it installed in this container?)")
            }
            Self::Io(e) => write!(f, "compositor setup failed: {e}"),
            Self::Exited { status, log_tail } => {
                write!(f, "compositor exited during startup ({status}); log:\n{log_tail}")
            }
            Self::NoSocket { log_tail } => write!(f, "compositor did not create a Wayland socket; log:\n{log_tail}"),
            Self::SwayConfig { path, err } => write!(f, "cannot read --sway-config {}: {err}", path.display()),
            Self::MissingProgram { program, hint } => write!(f, "{program} is not in PATH ({hint})"),
        }
    }
}

impl std::error::Error for CompositorError {}

impl From<io::Error> for CompositorError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// POSIX shell single-quoting.
pub fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Environment for the headless compositor only; the app gets termway's own values back
/// (a nested sway, for one, must not start headless).
const COMPOSITOR_ENV: [(&str, &str); 3] =
    [("WLR_BACKENDS", "headless"), ("WLR_RENDERER", "pixman"), ("WLR_LIBINPUT_NO_DEVICES", "1")];

/// `--x11`: the app runs as an X11 session on a rootful Xwayland.
#[derive(Clone, Debug, PartialEq)]
pub struct X11 {
    /// Reported to X clients, so Xft/pango text follows the output scale.
    pub dpi: u32,
    /// Holds the cookie for Xwayland (`-auth`) and its clients (`XAUTHORITY`).
    pub xauthority: PathBuf,
    /// Xwayland writes its display number here (`-displayfd`).
    pub display_fifo: PathBuf,
}

/// Starts Xwayland and runs `cmd` on it, leaving the app's status in `$status`.
///
/// Rootful but not fullscreen: fullscreen keeps the X screen at a fixed size (640x480
/// unless `-geometry`) scaled to the output, while a tiled window makes Xwayland follow
/// the window size, so the X screen tracks terminal resizes. `-hidpi` sizes it in
/// output pixels instead of scaling up a logical-size screen. `-shm`: sway renders with
/// pixman, so GPU buffers could not be shown.
fn x11_session(x: &X11, cmd: &str) -> String {
    let (auth, fifo) = (shell_quote(&x.xauthority.to_string_lossy()), shell_quote(&x.display_fifo.to_string_lossy()));
    format!(
        "rm -f {fifo}
         mkfifo -m 600 {fifo}
         Xwayland -hidpi -dpi {dpi} -shm -nolisten tcp -auth {auth} -displayfd 3 3>{fifo} &
         xwayland=$!
         read -r display <{fifo}
         rm -f {fifo}
         if [ -n \"$display\" ]; then
         \t(unset WAYLAND_DISPLAY SWAYSOCK I3SOCK; export DISPLAY=\":$display\" XAUTHORITY={auth} XDG_SESSION_TYPE=x11; exec {cmd})
         \tstatus=$?
         else
         \techo 'termway: Xwayland did not start' >&2
         \tstatus=127
         fi
         kill \"$xwayland\" 2>/dev/null
         wait \"$xwayland\" 2>/dev/null
",
        dpi = x.dpi
    )
}

/// One MIT-MAGIC-COOKIE-1 entry for any address and display (FamilyWild), in the
/// Xauthority file format (big-endian lengths).
pub fn xauthority_entry(cookie: &[u8; 16]) -> Vec<u8> {
    let mut v = 0xffffu16.to_be_bytes().to_vec();
    for field in [&b""[..], b"", b"MIT-MAGIC-COOKIE-1", cookie] {
        v.extend_from_slice(&(field.len() as u16).to_be_bytes());
        v.extend_from_slice(field);
    }
    v
}

/// Without a cookie any local user could connect to the X server, read the screen and
/// inject input.
fn write_xauthority(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut cookie = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut cookie)?;
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    io::Write::write_all(&mut f, &xauthority_entry(&cookie))
}

fn find_program(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// `app_env` holds termway's values of [`COMPOSITOR_ENV`] (`None`: unset). The app's exit
/// status goes to `status_file`, since sway does not report it.
pub fn launch_script(
    kind: Kind,
    command: &[String],
    app_env: &[(&str, Option<String>)],
    status_file: &Path,
    x11: Option<&X11>,
) -> String {
    let cmd: Vec<String> = command.iter().map(|a| shell_quote(a)).collect();
    let cmd = cmd.join(" ");
    let run = match x11 {
        Some(x) => x11_session(x, &cmd),
        None => format!("{cmd}\nstatus=$?\n"),
    };
    let mut env = String::new();
    for (name, value) in app_env {
        match value {
            Some(v) => env += &format!("export {name}={}\n", shell_quote(v)),
            None => env += &format!("unset {name}\n"),
        }
    }
    let status_file = shell_quote(&status_file.to_string_lossy());
    let end_session = match kind {
        // sway keeps running after its client exits; end the session with the app.
        Kind::Sway => "swaymsg exit >/dev/null 2>&1\n",
        Kind::Cage => "",
    };
    format!("#!/bin/sh\n# generated by termway\n{env}{run}echo \"$status\" >{status_file}\n{end_session}exit $status\n")
}

fn termway_env() -> Vec<(&'static str, Option<String>)> {
    COMPOSITOR_ENV.iter().map(|(k, _)| (*k, std::env::var(k).ok())).collect()
}

/// Initial output configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub scale: f64,
}

/// Headless outputs have no cursor plane, so sway always paints its pointer into the
/// captured frame; hiding it when idle or typing avoids a second cursor next to the
/// terminal's own and the damage it causes.
///
/// `include` (the user's config) comes after these defaults so its settings win; the
/// output mode and scale it sets are replaced once termway connects.
pub fn sway_config(mode: OutputMode, launch: Option<&Path>, include: Option<&Path>) -> String {
    let OutputMode { width, height, refresh_hz, scale } = mode;
    let mut s = format!(
        "# generated by termway\n\
         xwayland disable\n\
         default_border none\n\
         default_floating_border none\n\
         focus_follows_mouse yes\n\
         seat * hide_cursor 1000\n\
         seat * hide_cursor when-typing enable\n\
         output * mode --custom {width}x{height}@{refresh_hz}Hz scale {scale}\n"
    );
    if let Some(p) = include {
        s += &format!("include {}\n", p.display());
    }
    if let Some(p) = launch {
        s += &format!("exec {}\n", p.display());
    }
    s
}

/// Validates `--sway-config` before anything is started.
fn check_user_config(path: &Path) -> Result<PathBuf, CompositorError> {
    let err = |err| CompositorError::SwayConfig { path: path.to_path_buf(), err };
    let real = fs::canonicalize(path).map_err(err)?;
    if !real.is_file() {
        return Err(err(io::Error::other("not a regular file")));
    }
    File::open(&real).map_err(err)?;
    Ok(real)
}

/// sway's config errors ("Error on line N '...': reason (file)") from its log.
fn config_errors(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("[sway/config.c:"))
        .filter_map(|l| l.find("Error on line").map(|i| l[i..].to_string()))
        .collect()
}

pub struct Compositor {
    child: Option<Child>,
    runtime_dir: PathBuf,
    log_path: PathBuf,
    kind: Kind,
    /// `None`: no app, the session lasts as long as the compositor.
    app: Option<String>,
    started: Instant,
}

const APP_STATUS_FILE: &str = "app.status";
/// In the runtime dir; see [`Compositor::spawn`] for why sway includes this link.
const USER_CONFIG_LINK: &str = "user.conf";
const EXIT_LOG_LINES: usize = 10;

/// How the session ended, for the log.
#[derive(Debug, PartialEq)]
pub struct ExitReport {
    pub info: String,
    /// Set when the end looks unintended.
    pub warning: Option<String>,
}

/// `app` is `None` when no app was started; `app_status` is `None` when the compositor
/// ended before the app; `windows` is `None` when the compositor cannot list them. The
/// app's output ends up in the compositor log.
fn exit_report(
    kind: Kind,
    app: Option<&str>,
    app_status: Option<i32>,
    compositor: ExitStatus,
    windows: Option<u32>,
    after: Duration,
    log_tail: &str,
) -> ExitReport {
    let log = if log_tail.trim().is_empty() {
        String::new()
    } else {
        format!("\nlast lines of the session log:\n{log_tail}")
    };
    let abnormal = || (!compositor.success()).then(|| format!("{} ended abnormally{log}", kind.binary()));
    let Some(app) = app else {
        return ExitReport { info: format!("{} exited ({compositor})", kind.binary()), warning: abnormal() };
    };
    let Some(status) = app_status else {
        let info = format!("{} exited ({compositor}) before {app} finished", kind.binary());
        return ExitReport { info, warning: abnormal() };
    };
    let info = format!("{app} exited with status {status} after {:.1} s", after.as_secs_f64());
    let warning = match (status, windows) {
        // Typical for single-instance apps: the request went to a running instance.
        (0, Some(0)) => Some(format!(
            "{app} exited without opening a window. Single-instance apps such as Firefox hand the request \
             to an instance that is already running (another termway session shares the D-Bus session bus) \
             and exit; start a separate instance instead, e.g. firefox --new-instance --profile DIR{log}"
        )),
        (0, _) => None,
        (_, Some(0)) => Some(format!("{app} failed without opening a window{log}")),
        _ => Some(format!("{app} failed{log}")),
    };
    ExitReport { info, warning }
}

static INSTANCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What the compositor runs, and how.
#[derive(Clone, Copy, Debug)]
pub struct Launch<'a> {
    /// Empty: no app (sway only).
    pub command: &'a [String],
    pub mode: OutputMode,
    pub log: Option<&'a Path>,
    /// `--sway-config`.
    pub sway_config: Option<&'a Path>,
    /// Run `command` as an X11 session on rootful Xwayland (sway only).
    pub x11: bool,
}

impl Compositor {
    /// The sway config is included through a link in the runtime dir: sway word-expands
    /// include paths, so spaces or quotes in the user's path would split it, and sway
    /// resolves the link, so includes inside the user's file stay relative to that file.
    pub fn spawn(kind: Kind, launch: &Launch) -> Result<Self, CompositorError> {
        let base = std::env::var_os("XDG_RUNTIME_DIR").ok_or(CompositorError::NoRuntimeDir)?;
        Self::spawn_in(Path::new(&base), kind.binary(), kind, launch)
    }

    fn spawn_in(base: &Path, program: &'static str, kind: Kind, launch: &Launch) -> Result<Self, CompositorError> {
        let &Launch { command, mode, log, sway_config: conf_path, x11 } = launch;
        debug_assert!(kind == Kind::Sway || (conf_path.is_none() && !x11 && !command.is_empty()));
        debug_assert!(!x11 || !command.is_empty());
        let user_conf = conf_path.map(check_user_config).transpose()?;
        if x11 && find_program("Xwayland", std::env::var_os("PATH").as_deref()).is_none() {
            let hint = "--x11 runs the session on Xwayland; install the xwayland package";
            return Err(CompositorError::MissingProgram { program: "Xwayland", hint });
        }
        let n = INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let runtime_dir = base.join(format!("termway-{}-{n}", std::process::id()));
        if runtime_dir.exists() {
            fs::remove_dir_all(&runtime_dir)?;
        }
        fs::DirBuilder::new().mode(0o700).create(&runtime_dir)?;
        // Keep the session bus reachable for apps that need it.
        let bus = base.join("bus");
        if bus.exists() {
            let _ = std::os::unix::fs::symlink(&bus, runtime_dir.join("bus"));
        }
        let app =
            command.first().map(|a| Path::new(a).file_name().map_or(a.clone(), |n| n.to_string_lossy().into_owned()));
        let mut this = Self {
            child: None,
            log_path: log.map_or_else(|| runtime_dir.join("compositor.log"), Path::to_path_buf),
            runtime_dir,
            kind,
            app,
            started: Instant::now(),
        };
        let launch = this.runtime_dir.join("launch.sh");
        if !command.is_empty() {
            let x11 = if x11 {
                let x = X11 {
                    dpi: (96.0 * mode.scale).round() as u32,
                    xauthority: this.runtime_dir.join("xauthority"),
                    display_fifo: this.runtime_dir.join("x11-display"),
                };
                write_xauthority(&x.xauthority)?;
                Some(x)
            } else {
                None
            };
            let status_file = this.runtime_dir.join(APP_STATUS_FILE);
            fs::write(&launch, launch_script(kind, command, &termway_env(), &status_file, x11.as_ref()))?;
            fs::set_permissions(&launch, fs::Permissions::from_mode(0o700))?;
        }
        let include = match &user_conf {
            Some(real) => {
                let link = this.runtime_dir.join(USER_CONFIG_LINK);
                std::os::unix::fs::symlink(real, &link)?;
                Some(link)
            }
            None => None,
        };
        let log_file = File::create(&this.log_path)?;
        let mut cmd = Command::new(program);
        match kind {
            Kind::Sway => {
                let config = this.runtime_dir.join("sway.conf");
                let launch = (!command.is_empty()).then_some(launch.as_path());
                fs::write(&config, sway_config(mode, launch, include.as_deref()))?;
                cmd.arg("-c").arg(config);
            }
            Kind::Cage => {
                cmd.arg("--").arg(&launch);
            }
        }
        cmd.envs(COMPOSITOR_ENV)
            .env("XDG_RUNTIME_DIR", &this.runtime_dir)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("SWAYSOCK")
            .env_remove("I3SOCK")
            .stdin(Stdio::null())
            .stdout(log_file.try_clone()?)
            .stderr(log_file)
            // Own process group: terminal signals do not reach it and we can stop its children.
            .process_group(0);
        // SAFETY: prctl is async-signal-safe; it asks the kernel to SIGTERM the child if we die.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|err| CompositorError::Spawn { binary: program, err })?;
        this.child = Some(child);
        Ok(this)
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.as_mut().and_then(|c| c.try_wait().ok().flatten())
    }

    /// Waits for the compositor's `wayland-N` socket to appear.
    pub fn wait_for_socket(&mut self, timeout: Duration) -> Result<PathBuf, CompositorError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait() {
                return Err(CompositorError::Exited { status, log_tail: self.log_tail(20) });
            }
            if let Some(sock) = find_socket(&self.runtime_dir)? {
                return Ok(sock);
            }
            if Instant::now() >= deadline {
                return Err(CompositorError::NoSocket { log_tail: self.log_tail(20) });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The app's exit status, once the launch script has recorded it.
    fn app_status(&self) -> Option<i32> {
        fs::read_to_string(self.runtime_dir.join(APP_STATUS_FILE)).ok()?.trim().parse().ok()
    }

    /// Call once the compositor has exited with `status`; `windows` is the number of
    /// toplevels the app opened (`None`: unknown).
    pub fn exit_report(&self, status: ExitStatus, windows: Option<u32>) -> ExitReport {
        let tail = self.log_tail(EXIT_LOG_LINES);
        exit_report(self.kind, self.app.as_deref(), self.app_status(), status, windows, self.started.elapsed(), &tail)
    }

    /// Errors in sway's configuration. sway reads it before it answers any client, so
    /// once termway is connected they are in the log.
    pub fn config_errors(&self) -> Vec<String> {
        let mut raw = Vec::new();
        if let Ok(f) = File::open(&self.log_path) {
            let _ = f.take(256 * 1024).read_to_end(&mut raw);
        }
        config_errors(&String::from_utf8_lossy(&raw))
    }

    pub fn log_tail(&self, lines: usize) -> String {
        let Ok(mut f) = File::open(&self.log_path) else { return String::from("(no log)") };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(16 * 1024);
        let _ = f.seek(SeekFrom::Start(start));
        let mut raw = Vec::new();
        let _ = f.read_to_end(&mut raw);
        // Lossy: the cut can split a character, and apps may print any bytes.
        let s = String::from_utf8_lossy(&raw);
        let mut v: Vec<&str> = s.lines().collect();
        if start > 0 && !v.is_empty() {
            v.remove(0);
        }
        v[v.len().saturating_sub(lines)..].join("\n")
    }

    /// Stops the compositor and everything it started, then removes the runtime dir.
    pub fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                let pgid = -(child.id() as i32);
                // SAFETY: signalling our own child's process group.
                unsafe { libc::kill(pgid, libc::SIGTERM) };
                let deadline = Instant::now() + Duration::from_secs(2);
                while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if child.try_wait().ok().flatten().is_none() {
                    // SAFETY: as above.
                    unsafe { libc::kill(pgid, libc::SIGKILL) };
                    let _ = child.wait();
                }
            }
            // Whatever is left in sway's own group.
            // SAFETY: as above; ESRCH when the group is already gone is fine.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        }
        stop_session_processes(&self.runtime_dir);
        if self.runtime_dir.exists() {
            let _ = fs::remove_dir_all(&self.runtime_dir);
        }
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Processes started inside the session. sway's `exec` gives each app its own session,
/// so they are not in sway's process group, but all inherit its private runtime dir.
fn session_processes(runtime_dir: &Path) -> Vec<i32> {
    use std::os::unix::ffi::OsStrExt;
    let needle = [b"XDG_RUNTIME_DIR=", runtime_dir.as_os_str().as_bytes()].concat();
    let me = std::process::id() as i32;
    let Ok(dir) = fs::read_dir("/proc") else { return Vec::new() };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&pid| pid != me)
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| env.split(|&b| b == 0).any(|v| v == needle))
        })
        .collect()
}

/// Stops apps that outlive the compositor, e.g. a nested headless sway or a daemon.
fn stop_session_processes(runtime_dir: &Path) {
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        let pids = session_processes(runtime_dir);
        if pids.is_empty() {
            return;
        }
        crate::log_debug!("sending signal {sig} to {} leftover session processes", pids.len());
        for pid in pids {
            // SAFETY: plain kill(2); the pid carries our private runtime dir.
            unsafe { libc::kill(pid, sig) };
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && !session_processes(runtime_dir).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn find_socket(dir: &Path) -> io::Result<Option<PathBuf>> {
    use std::os::unix::fs::FileTypeExt;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("wayland-") && !name.ends_with(".lock") && entry.file_type()?.is_socket() {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

/// Resolves `--attach NAME` like libwayland does for WAYLAND_DISPLAY.
pub fn resolve_display(name: &str) -> Result<PathBuf, CompositorError> {
    resolve_display_in(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), name)
}

fn resolve_display_in(runtime_dir: Option<&std::ffi::OsStr>, name: &str) -> Result<PathBuf, CompositorError> {
    let p = Path::new(name);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    Ok(PathBuf::from(runtime_dir.ok_or(CompositorError::NoRuntimeDir)?).join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_the_shell() {
        let args: Vec<String> =
            ["printf", "%s|", "a b", "it's", "$HOME", "`x`", ";"].iter().map(|s| s.to_string()).collect();
        let script = launch_script(Kind::Cage, &args, &[], Path::new("/dev/null"), None);
        let out = Command::new("sh").arg("-c").arg(&script).output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "a b|it's|$HOME|`x`|;|");
    }

    #[test]
    fn sway_script_ends_session_with_app() {
        let s = launch_script(Kind::Sway, &["foot".into()], &[], Path::new("/r/app.status"), None);
        assert!(s.contains("'foot'\nstatus=$?\necho \"$status\" >'/r/app.status'\nswaymsg exit"));
        let mode = OutputMode { width: 2538, height: 1239, refresh_hz: 60, scale: 1.5 };
        let c = sway_config(mode, Some(Path::new("/run/user/1000/termway-1/launch.sh")), None);
        assert!(c.contains("output * mode --custom 2538x1239@60Hz scale 1.5\n"));
        assert!(c.ends_with("\nexec /run/user/1000/termway-1/launch.sh\n"));
        let c = sway_config(OutputMode { scale: 2.0, ..mode }, Some(Path::new("/l")), None);
        assert!(c.contains("@60Hz scale 2\n"), "integral scales print without a fraction");
        assert!(c.contains("xwayland disable"));
        assert!(!c.contains("include"));
    }

    #[test]
    fn user_sway_config_comes_after_the_defaults() {
        let mode = OutputMode { width: 800, height: 600, refresh_hz: 60, scale: 1.0 };
        let c = sway_config(mode, None, Some(Path::new("/r/user.conf")));
        let (defaults, rest) = c.split_once("include /r/user.conf\n").expect("include line");
        assert!(defaults.contains("default_border none\n") && defaults.contains("output * mode"));
        assert_eq!(rest, "", "no app to exec");
    }

    #[test]
    fn user_sway_config_is_linked_into_the_runtime_dir() {
        let base = std::env::temp_dir().join(format!("termway-userconf-{}", std::process::id()));
        let dir = base.join("my configs");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("it's sway.conf");
        fs::write(&file, "gaps outer 4\n").unwrap();
        let mode = OutputMode { width: 10, height: 10, refresh_hz: 60, scale: 1.0 };
        // `true` stands in for sway: it ignores `-c` and exits.
        let launch = Launch { command: &[], mode, log: None, sway_config: Some(&file), x11: false };
        let c = Compositor::spawn_in(&base, "true", Kind::Sway, &launch).unwrap();
        let link = c.runtime_dir.join(USER_CONFIG_LINK);
        assert_eq!(fs::read_link(&link).unwrap(), fs::canonicalize(&file).unwrap());
        let conf = fs::read_to_string(c.runtime_dir.join("sway.conf")).unwrap();
        assert!(conf.ends_with(&format!("include {}\n", link.display())), "{conf}");
        assert!(!c.runtime_dir.join("launch.sh").exists(), "no app, no launch script");
        let dir_of_session = c.runtime_dir.clone();
        drop(c);
        assert!(!dir_of_session.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unreadable_sway_config_fails_before_starting() {
        let base = std::env::temp_dir().join(format!("termway-noconf-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let mode = OutputMode { width: 10, height: 10, refresh_hz: 60, scale: 1.0 };
        for path in [base.join("missing.conf"), base.clone()] {
            let launch = Launch { command: &[], mode, log: None, sway_config: Some(&path), x11: false };
            let r = Compositor::spawn_in(&base, "true", Kind::Sway, &launch);
            let err = r.err().expect("must fail");
            assert!(matches!(err, CompositorError::SwayConfig { .. }), "{err:?}");
            assert!(err.to_string().starts_with(&format!("cannot read --sway-config {}: ", path.display())), "{err}");
        }
        assert_eq!(fs::read_dir(&base).unwrap().count(), 0, "no runtime dir created");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn config_errors_are_found_in_the_log() {
        let log = "00:00:00.001 [INFO] [sway/config.c:800] Loading config from /r/sway.conf\n\
                   00:00:00.007 [ERROR] [sway/config.c:814] Error on line 3 'output * bg #285577 solid_color # x': \
                   Invalid output subcommand: #. (/home/u/termway.conf)\n\
                   00:00:00.008 [ERROR] [sway/config.c:440] Error(s) loading config!\n\
                   foot: Error on line 1 of something else\n";
        assert_eq!(
            config_errors(log),
            ["Error on line 3 'output * bg #285577 solid_color # x': Invalid output subcommand: #. (/home/u/termway.conf)"]
        );
        assert!(config_errors("00:00:00.001 [INFO] [sway/config.c:800] Loading config\n").is_empty());
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("termway-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_exe(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs a launch script with `stub_dir` first in PATH; returns (stdout, stderr).
    fn run_x11_script(dir: &Path, xwayland: &str) -> (String, String) {
        let stub_dir = dir.join("bin");
        fs::create_dir_all(&stub_dir).unwrap();
        write_exe(&stub_dir.join("Xwayland"), xwayland);
        let x = X11 { dpi: 192, xauthority: dir.join("xauthority"), display_fifo: dir.join("x11-display") };
        let cmd = [
            "sh".to_string(),
            "-c".into(),
            "echo \"$DISPLAY|$XAUTHORITY|${WAYLAND_DISPLAY-unset}|${I3SOCK-unset}|$XDG_SESSION_TYPE\"".into(),
        ];
        let script = launch_script(Kind::Cage, &cmd, &[], &dir.join("app.status"), Some(&x));
        let path = format!("{}:{}", stub_dir.display(), std::env::var("PATH").unwrap_or_default());
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env("PATH", path)
            .env("WAYLAND_DISPLAY", "wayland-1")
            .env("I3SOCK", "/run/sway.sock")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                child.kill().unwrap();
                panic!("launch script hung (Xwayland not stopped?)");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = child.wait_with_output().unwrap();
        (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    #[test]
    fn x11_session_runs_on_xwaylands_display_and_stops_it() {
        let dir = temp_dir("x11");
        // The stub records its arguments, reports display 7 and would stay for 30 s.
        let stub = format!("#!/bin/sh\necho \"$@\" >{}/args\necho 7 >&3\nexec sleep 30\n", dir.display());
        let (out, _) = run_x11_script(&dir, &stub);
        let auth = dir.join("xauthority");
        assert_eq!(out, format!(":7|{}|unset|unset|x11\n", auth.display()), "X11 env, Wayland and sway IPC removed");
        assert_eq!(fs::read_to_string(dir.join("app.status")).unwrap(), "0\n");
        let args = fs::read_to_string(dir.join("args")).unwrap();
        assert!(
            args.starts_with(&format!("-hidpi -dpi 192 -shm -nolisten tcp -auth {} -displayfd 3", auth.display())),
            "{args}"
        );
        assert!(!args.contains("-fullscreen"), "fullscreen would pin the X screen size");
        assert!(!dir.join("x11-display").exists(), "fifo removed");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn x11_session_reports_an_xwayland_that_does_not_start() {
        let dir = temp_dir("x11fail");
        let (out, err) = run_x11_script(&dir, "#!/bin/sh\necho 'cannot open display' >&2\nexit 1\n");
        assert_eq!(out, "", "the app is not started");
        assert!(err.contains("termway: Xwayland did not start"), "{err}");
        assert_eq!(fs::read_to_string(dir.join("app.status")).unwrap(), "127\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn xauthority_entry_matches_any_display() {
        let cookie: [u8; 16] = std::array::from_fn(|i| i as u8);
        let e = xauthority_entry(&cookie);
        let mut want = vec![0xff, 0xff, 0, 0, 0, 0, 0, 18];
        want.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
        want.extend_from_slice(&[0, 16]);
        want.extend_from_slice(&cookie);
        assert_eq!(e, want, "FamilyWild, empty address and display number");
    }

    #[test]
    fn programs_are_found_only_when_executable() {
        let dir = temp_dir("path");
        write_exe(&dir.join("Xwayland"), "#!/bin/sh\n");
        fs::write(dir.join("plain"), "").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), &dir]).unwrap();
        assert_eq!(find_program("Xwayland", Some(&path)), Some(dir.join("Xwayland")));
        assert_eq!(find_program("plain", Some(&path)), None);
        assert_eq!(find_program("Xwayland", None), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn app_gets_termways_environment_back() {
        let env = [("WLR_BACKENDS", None), ("WLR_RENDERER", Some("gles2 'x'".to_string()))];
        let script = launch_script(
            Kind::Cage,
            &["sh".into(), "-c".into(), "echo \"${WLR_BACKENDS-unset}|$WLR_RENDERER\"".into()],
            &env,
            Path::new("/dev/null"),
            None,
        );
        let out = Command::new("sh").arg("-c").arg(&script).env("WLR_BACKENDS", "headless").output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "unset|gles2 'x'\n");
    }

    #[test]
    fn app_exit_is_reported_with_its_output() {
        let base = std::env::temp_dir().join(format!("termway-exit-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let mode = OutputMode { width: 10, height: 10, refresh_hz: 60, scale: 1.0 };
        let cmd = ["/bin/sh".into(), "-c".into(), "echo boom >&2; exit 3".into()];
        // `env -- launch.sh` stands in for `cage -- launch.sh`.
        let launch = Launch { command: &cmd, mode, log: None, sway_config: None, x11: false };
        let mut c = Compositor::spawn_in(&base, "env", Kind::Cage, &launch).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(s) = c.try_wait() {
                break s;
            }
            assert!(Instant::now() < deadline, "launch script did not finish");
            std::thread::sleep(Duration::from_millis(10));
        };
        let r = c.exit_report(status, Some(0));
        assert!(r.info.starts_with("sh exited with status 3 after "), "{}", r.info);
        let warning = r.warning.expect("a failure is a warning");
        assert!(warning.starts_with("sh failed without opening a window\n"), "{warning}");
        assert!(warning.ends_with("\nboom"), "{warning}");
        drop(c);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn exit_reports() {
        use std::os::unix::process::ExitStatusExt;
        let ok = ExitStatus::from_raw(0);
        let after = Duration::from_millis(3960);
        let report = |app_status, compositor, windows, tail| {
            exit_report(Kind::Sway, Some("firefox"), app_status, compositor, windows, after, tail)
        };

        let r = report(Some(0), ok, Some(0), "");
        assert_eq!(r.info, "firefox exited with status 0 after 4.0 s");
        let w = r.warning.expect("no window is suspicious");
        assert!(w.starts_with("firefox exited without opening a window."), "{w}");
        assert!(w.contains("--new-instance") && !w.contains('\n'), "{w}");

        assert_eq!(report(Some(0), ok, Some(2), "noise").warning, None, "a normal exit");
        assert_eq!(report(Some(0), ok, None, "").warning, None, "window count unknown");
        let w = report(Some(1), ok, Some(1), "Error: x\n").warning.unwrap();
        assert_eq!(w, "firefox failed\nlast lines of the session log:\nError: x\n");

        let r = report(None, ok, None, "");
        assert_eq!(r.info, "sway exited (exit status: 0) before firefox finished");
        assert_eq!(r.warning, None);
        let r = report(None, ExitStatus::from_raw(libc::SIGSEGV), Some(1), "wlr: oops");
        assert_eq!(r.warning.unwrap(), "sway ended abnormally\nlast lines of the session log:\nwlr: oops");

        // No app (--sway-config without a command): the session is the compositor's.
        let r = exit_report(Kind::Sway, None, None, ok, Some(3), after, "noise");
        assert_eq!(r, ExitReport { info: "sway exited (exit status: 0)".into(), warning: None });
        let r = exit_report(Kind::Sway, None, None, ExitStatus::from_raw(libc::SIGSEGV), None, after, "wlr: oops");
        assert_eq!(r.warning.unwrap(), "sway ended abnormally\nlast lines of the session log:\nwlr: oops");
    }

    #[test]
    fn leftover_session_processes_are_stopped() {
        let rt = std::env::temp_dir().join(format!("termway-sweep-{}", std::process::id()));
        // setsid: like sway's exec, the app leaves our process group.
        let mut inside = Command::new("setsid").args(["sleep", "30"]).env("XDG_RUNTIME_DIR", &rt).spawn().unwrap();
        let mut outside = Command::new("sleep").arg("30").env("XDG_RUNTIME_DIR", "/nonexistent").spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while session_processes(&rt).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(session_processes(&rt), [inside.id() as i32]);
        stop_session_processes(&rt);
        assert!(inside.wait().unwrap().code().is_none(), "killed by a signal");
        assert!(outside.try_wait().unwrap().is_none(), "unrelated processes survive");
        outside.kill().unwrap();
        outside.wait().unwrap();
    }

    #[test]
    fn resolve_relative_display() {
        let rt = Some(std::ffi::OsStr::new("/tmp/rt-test"));
        assert_eq!(resolve_display_in(rt, "wayland-3").unwrap(), PathBuf::from("/tmp/rt-test/wayland-3"));
        assert_eq!(resolve_display_in(None, "/x/y").unwrap(), PathBuf::from("/x/y"));
        assert!(matches!(resolve_display_in(None, "wayland-0"), Err(CompositorError::NoRuntimeDir)));
    }

    #[test]
    fn missing_binary_is_reported_and_cleaned_up() {
        let base = std::env::temp_dir().join(format!("termway-test-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let mode = OutputMode { width: 10, height: 10, refresh_hz: 60, scale: 1.0 };
        let cmd = ["true".to_string()];
        let launch = Launch { command: &cmd, mode, log: None, sway_config: None, x11: false };
        let r = Compositor::spawn_in(&base, "/nonexistent/sway", Kind::Sway, &launch);
        let err = r.err().expect("spawn must fail");
        assert!(matches!(err, CompositorError::Spawn { .. }), "{err:?}");
        assert!(err.to_string().contains("/nonexistent/sway"));
        assert_eq!(fs::read_dir(&base).unwrap().count(), 0, "runtime dir removed on failure");
        fs::remove_dir_all(&base).unwrap();
    }
}
