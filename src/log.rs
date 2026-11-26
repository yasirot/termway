//! Minimal logger. stdout belongs to the terminal image stream and stderr usually is the
//! same terminal, so without `--log` messages are buffered and printed after the
//! terminal has been restored.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

enum Sink {
    File(File),
    Stderr,
    Buffer(Vec<String>),
}

struct Logger {
    sink: Sink,
    min: Level,
    start: Instant,
}

static LOGGER: Mutex<Option<Logger>> = Mutex::new(None);
const BUFFER_LINES: usize = 500;

/// Routes messages of dependencies using the `log` facade (wayland-backend) into our sink.
struct Facade;

impl log::Log for Facade {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }

    fn log(&self, r: &log::Record) {
        let level = match r.level() {
            // wayland-backend logs every connection error it returns; termway reports those
            // itself, and a broken pipe is how a session normally ends.
            _ if r.target().starts_with("wayland_backend::rs::client_impl") => Level::Debug,
            log::Level::Error => Level::Error,
            log::Level::Warn => Level::Warn,
            log::Level::Info => Level::Info,
            _ => Level::Debug,
        };
        log(level, format_args!("{}: {}", r.target(), r.args()));
    }

    fn flush(&self) {}
}

static FACADE: Facade = Facade;

pub fn init(path: Option<&Path>, stderr_is_tty: bool, verbose: bool) -> io::Result<()> {
    let sink = match path {
        Some(p) => Sink::File(File::create(p)?),
        None if stderr_is_tty => Sink::Buffer(Vec::new()),
        None => Sink::Stderr,
    };
    let min = if verbose { Level::Debug } else { Level::Info };
    *LOGGER.lock().unwrap_or_else(|e| e.into_inner()) = Some(Logger { sink, min, start: Instant::now() });
    if log::set_logger(&FACADE).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    Ok(())
}

thread_local! {
    /// Set while this thread holds the logger, so a panic raised meanwhile (whose hook
    /// logs) returns instead of deadlocking on the lock it already holds.
    static IN_LOGGER: Cell<bool> = const { Cell::new(false) };
}

struct InLogger;

impl InLogger {
    fn enter() -> Option<Self> {
        (!IN_LOGGER.with(|f| f.replace(true))).then_some(Self)
    }
}

impl Drop for InLogger {
    fn drop(&mut self) {
        IN_LOGGER.with(|f| f.set(false));
    }
}

/// Writes a line to stderr, ignoring errors: `eprintln!` panics when the terminal is
/// gone (EIO after an SSH disconnect), which must not take the process down with it.
pub fn stderr_line(line: &str) {
    let _ = writeln!(io::stderr().lock(), "{line}");
}

pub fn log(level: Level, msg: std::fmt::Arguments) {
    let Some(_in_logger) = InLogger::enter() else { return };
    let mut guard = LOGGER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = guard.as_mut() else { return };
    if level < l.min {
        return;
    }
    let line = format!("[{:9.3}] {:5} {}", l.start.elapsed().as_secs_f64(), format!("{level:?}").to_uppercase(), msg);
    match &mut l.sink {
        Sink::File(f) => {
            let _ = writeln!(f, "{line}");
        }
        Sink::Stderr => stderr_line(&line),
        Sink::Buffer(v) => {
            if v.len() >= BUFFER_LINES {
                // Keep warnings and errors; drop the oldest informational line.
                if let Some(i) = v.iter().position(|s| s.contains(" INFO ") || s.contains(" DEBUG ")) {
                    v.remove(i);
                } else {
                    v.remove(0);
                }
            }
            v.push(line);
        }
    }
}

/// Prints buffered messages (call after the terminal is restored).
pub fn flush_buffered() {
    let lines = {
        let Some(_in_logger) = InLogger::enter() else { return };
        let mut guard = LOGGER.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(Logger { sink: Sink::Buffer(v), .. }) => std::mem::take(v),
            _ => return,
        }
    };
    for line in lines.iter().filter(|l| !l.contains(" DEBUG ")) {
        stderr_line(line);
    }
}

#[macro_export]
macro_rules! log_debug { ($($t:tt)*) => { $crate::log::log($crate::log::Level::Debug, format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_info { ($($t:tt)*) => { $crate::log::log($crate::log::Level::Info, format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_warn { ($($t:tt)*) => { $crate::log::log($crate::log::Level::Warn, format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_error { ($($t:tt)*) => { $crate::log::log($crate::log::Level::Error, format_args!($($t)*)) } }

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic inside the logger runs the panic hook, which logs on the same thread.
    #[test]
    fn logging_from_inside_the_logger_returns() {
        let _held = InLogger::enter().expect("not nested");
        assert!(InLogger::enter().is_none());
        log(Level::Error, format_args!("from a panic hook"));
        flush_buffered();
    }
}
