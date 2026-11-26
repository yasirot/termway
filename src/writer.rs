//! Output thread: writes whole frames to the terminal so a slow link never blocks
//! input handling, and reports completion through a pipe the main loop polls.

use rustix::event::{poll, PollFd, PollFlags};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::pipe::{pipe_with, PipeFlags};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bytes per write, so progress is visible while a large frame crawls over a slow link.
const WRITE_CHUNK: usize = 64 * 1024;
/// Upper bound for [`Writer::finish`] however steadily the terminal keeps reading.
const FINISH_CAP: Duration = Duration::from_secs(60);

pub struct Writer {
    tx: Option<Sender<Vec<u8>>>,
    handle: Option<JoinHandle<()>>,
    done: OwnedFd,
    in_flight: usize,
    error: Arc<Mutex<Option<io::Error>>>,
    written: Arc<AtomicU64>,
    /// Makes the thread skip buffers it has not started (each one is self-contained).
    cancel: Arc<AtomicBool>,
}

impl Writer {
    pub fn spawn(out: BorrowedFd<'static>) -> io::Result<Self> {
        let (done_r, done_w) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
        let (tx, rx) = channel::<Vec<u8>>();
        let error = Arc::new(Mutex::new(None));
        let written = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (err, progress, cancelled) = (Arc::clone(&error), Arc::clone(&written), Arc::clone(&cancel));
        let handle = std::thread::Builder::new().name("termway-writer".into()).spawn(move || {
            for buf in rx {
                let mut r = Ok(());
                if !cancelled.load(Ordering::Relaxed) {
                    for chunk in buf.chunks(WRITE_CHUNK) {
                        r = crate::term::write_all(out, chunk);
                        if r.is_err() {
                            break;
                        }
                        progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    }
                }
                let failed = r.is_err();
                if let Err(e) = r {
                    *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                }
                let _ = rustix::io::write(&done_w, &[1]);
                if failed {
                    break;
                }
            }
            // Dropping done_w signals EOF to the main loop.
        })?;
        Ok(Self { tx: Some(tx), handle: Some(handle), done: done_r, in_flight: 0, error, written, cancel })
    }

    pub fn done_fd(&self) -> BorrowedFd<'_> {
        self.done.as_fd()
    }

    pub fn busy(&self) -> bool {
        self.in_flight > 0
    }

    pub fn send(&mut self, buf: Vec<u8>) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.check()?;
        let tx = self.tx.as_ref().ok_or_else(|| io::Error::other("writer closed"))?;
        tx.send(buf).map_err(|_| io::Error::other("writer thread exited"))?;
        self.in_flight += 1;
        Ok(())
    }

    fn check(&self) -> io::Result<()> {
        match self.error.lock().unwrap_or_else(|p| p.into_inner()).take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Consumes completion notifications; call when the done fd is readable.
    pub fn on_done(&mut self) -> io::Result<()> {
        let mut buf = [0u8; 64];
        while let Ok(n) = rustix::io::read(&self.done, &mut buf) {
            if n == 0 {
                break;
            }
            self.in_flight = self.in_flight.saturating_sub(n);
        }
        self.check()
    }

    /// Waits until everything queued is written. Returns false once no byte was written
    /// for `stall`, or after `cap` in total.
    pub fn drain(&mut self, stall: Duration, cap: Duration) -> io::Result<bool> {
        let end = Instant::now() + cap;
        let mut written = self.written.load(Ordering::Relaxed);
        let mut stalled_at = Instant::now() + stall;
        while self.in_flight > 0 {
            let now = Instant::now();
            let deadline = stalled_at.min(end);
            if now >= deadline {
                return Ok(false);
            }
            let mut fds = [PollFd::new(&self.done, PollFlags::IN)];
            let tick = deadline.saturating_duration_since(now).min(Duration::from_millis(100));
            match poll(&mut fds, Some(&crate::term::timespec(tick))) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            self.on_done()?;
            if self.handle.as_ref().is_some_and(|h| h.is_finished()) && self.in_flight > 0 {
                self.check()?;
                return Err(io::Error::other("writer thread exited with frames queued"));
            }
            let now_written = self.written.load(Ordering::Relaxed);
            if now_written != written {
                written = now_written;
                stalled_at = Instant::now() + stall;
            }
        }
        Ok(true)
    }

    /// Stops the thread after it finished the buffer in progress; buffers not started are
    /// dropped. Waits while the terminal keeps reading, up to [`FINISH_CAP`]. Returns
    /// false if the terminal stopped reading for `stall`, in which case the thread is leaked.
    pub fn finish(&mut self, stall: Duration) -> bool {
        self.cancel.store(true, Ordering::Relaxed);
        let drained = self.drain(stall, FINISH_CAP).unwrap_or(false);
        self.tx = None;
        if drained {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
        drained
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.finish(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_in_order_and_reports_completion() {
        let (r, w) = rustix::pipe::pipe().unwrap();
        // SAFETY: `w` outlives the writer, which is finished before it is dropped.
        let wfd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(std::os::fd::AsRawFd::as_raw_fd(&w)) };
        let mut wr = Writer::spawn(wfd).unwrap();
        wr.send(b"hello ".to_vec()).unwrap();
        wr.send(b"world".to_vec()).unwrap();
        wr.send(Vec::new()).unwrap();
        assert!(wr.drain(Duration::from_secs(2), Duration::from_secs(5)).unwrap());
        assert!(!wr.busy());
        assert!(wr.finish(Duration::from_secs(1)));
        let mut buf = [0u8; 32];
        let n = rustix::io::read(&r, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello world");
    }

    /// A large frame over a slow link: finishing waits while bytes keep moving, not for a
    /// fixed time (which used to leave the terminal unrestored on slow links).
    #[test]
    fn drain_waits_while_the_terminal_keeps_reading() {
        let (r, w) = rustix::pipe::pipe().unwrap();
        // SAFETY: `w` outlives the writer, which is finished before it is dropped.
        let wfd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(std::os::fd::AsRawFd::as_raw_fd(&w)) };
        let reader = std::thread::spawn(move || {
            let (mut buf, mut total) = (vec![0u8; 16 * 1024], 0);
            while total < 1 << 20 {
                std::thread::sleep(Duration::from_millis(20));
                match rustix::io::read(&r, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total += n,
                }
            }
            total
        });
        let mut wr = Writer::spawn(wfd).unwrap();
        wr.send(vec![7u8; 1 << 20]).unwrap();
        // About 16 KiB per 20 ms: over a second in total, far beyond the stall limit.
        assert!(wr.drain(Duration::from_millis(300), Duration::from_secs(20)).unwrap());
        assert!(wr.finish(Duration::from_secs(1)));
        assert_eq!(reader.join().unwrap(), 1 << 20);
    }

    #[test]
    fn write_errors_surface() {
        let (r, w) = rustix::pipe::pipe().unwrap();
        drop(r);
        // SAFETY: as above.
        let wfd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(std::os::fd::AsRawFd::as_raw_fd(&w)) };
        let mut wr = Writer::spawn(wfd).unwrap();
        wr.send(b"x".to_vec()).unwrap();
        let r = wr.drain(Duration::from_secs(2), Duration::from_secs(5));
        assert!(r.is_err(), "EPIPE must be reported: {r:?}");
        assert!(wr.send(b"y".to_vec()).is_err());
    }
}
