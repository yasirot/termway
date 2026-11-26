//! Self-pipe signal delivery so signals can be handled in the poll loop.

use rustix::fd::{AsRawFd, OwnedFd};
use rustix::pipe::{pipe_with, PipeFlags};
use std::io;
use std::sync::atomic::{AtomicI32, Ordering};

static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        // SAFETY: write(2) is async-signal-safe; a full pipe just drops the byte.
        unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
    }
}

pub struct SignalPipe {
    read: OwnedFd,
    _write: OwnedFd,
}

pub const HANDLED: [libc::c_int; 5] = [libc::SIGWINCH, libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

impl SignalPipe {
    pub fn install() -> io::Result<Self> {
        let (read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
        WRITE_FD.store(write.as_raw_fd(), Ordering::Relaxed);
        for sig in HANDLED {
            // SAFETY: installing a handler that only calls write(2).
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                sa.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut sa.sa_mask);
                if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(Self { read, _write: write })
    }

    pub fn fd(&self) -> &OwnedFd {
        &self.read
    }

    /// Signals received since the last call.
    pub fn drain(&self) -> Vec<libc::c_int> {
        let mut buf = [0u8; 64];
        let mut out = Vec::new();
        while let Ok(n) = rustix::io::read(&self.read, &mut buf) {
            if n == 0 {
                break;
            }
            out.extend(buf[..n].iter().map(|&b| b as libc::c_int));
        }
        out
    }
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        WRITE_FD.store(-1, Ordering::Relaxed);
        for sig in HANDLED {
            // SAFETY: restoring default dispositions.
            unsafe { libc::signal(sig, libc::SIG_DFL) };
        }
    }
}

pub fn name(sig: libc::c_int) -> &'static str {
    match sig {
        libc::SIGWINCH => "SIGWINCH",
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        libc::SIGHUP => "SIGHUP",
        libc::SIGQUIT => "SIGQUIT",
        _ => "signal",
    }
}
