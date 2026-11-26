//! Per-second statistics written as JSON lines (`--stats`), plus a final summary.

use crate::render::FrameStats;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub bytes: u64,
    /// Frames that produced output.
    pub frames: u64,
    pub captures: u64,
    pub render: FrameStats,
    pub encode: Duration,
}

impl Counters {
    fn add(&mut self, o: &Counters) {
        self.bytes += o.bytes;
        self.frames += o.frames;
        self.captures += o.captures;
        self.render.add(&o.render);
        self.encode += o.encode;
    }

    /// Share of tile lookups served from the cache (tile path only).
    pub fn hit_rate(&self) -> Option<f64> {
        let n = self.render.cache_hits + self.render.tiles_sent;
        (n > 0).then(|| self.render.cache_hits as f64 / n as f64)
    }
}

/// CPU time of a process in seconds, from /proc/<pid>/stat (utime + stime).
pub fn cpu_seconds(pid: Option<u32>) -> Option<f64> {
    let path = match pid {
        Some(p) => format!("/proc/{p}/stat"),
        None => "/proc/self/stat".to_string(),
    };
    let s = std::fs::read_to_string(path).ok()?;
    // Fields after the parenthesised command name; utime/stime are fields 14/15.
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    Some(ticks as f64 / if hz > 0 { hz as f64 } else { 100.0 })
}

pub struct Stats {
    out: Option<BufWriter<File>>,
    start: Instant,
    last: Instant,
    interval: Counters,
    total: Counters,
    cpu_last: (Option<f64>, Option<f64>),
    cpu_total_start: (Option<f64>, Option<f64>),
    compositor: Option<u32>,
    last_line: String,
}

pub struct Extra {
    pub cached_tiles: usize,
    pub cache_bytes: u64,
    pub live_ephemerals: usize,
}

impl Stats {
    pub fn new(path: Option<&Path>, compositor: Option<u32>) -> io::Result<Self> {
        let out = path.map(File::create).transpose()?.map(BufWriter::new);
        let now = Instant::now();
        let cpu = (cpu_seconds(None), compositor.and_then(|p| cpu_seconds(Some(p))));
        Ok(Self {
            out,
            start: now,
            last: now,
            interval: Counters::default(),
            total: Counters::default(),
            cpu_last: cpu,
            cpu_total_start: cpu,
            compositor,
            last_line: String::new(),
        })
    }

    pub fn record_output(&mut self, bytes: usize) {
        self.interval.bytes += bytes as u64;
    }

    pub fn record_capture(&mut self) {
        self.interval.captures += 1;
    }

    /// Accounts one render step; `captured` is false for Phase 4 refinement passes,
    /// which are not frames from the compositor.
    pub fn record_frame(&mut self, bytes: usize, st: &FrameStats, encode: Duration, captured: bool) {
        self.interval.bytes += bytes as u64;
        self.interval.frames += (captured && bytes > 0) as u64;
        self.interval.render.add(st);
        self.interval.encode += encode;
    }

    pub fn next_tick(&self) -> Instant {
        self.last + Duration::from_secs(1)
    }

    pub fn total(&self) -> Counters {
        let mut t = self.total;
        t.add(&self.interval);
        t
    }

    /// Short human-readable line for the status bar.
    pub fn last_line(&self) -> &str {
        &self.last_line
    }

    /// Emits one JSON line if a second has passed.
    pub fn tick(&mut self, now: Instant, extra: &Extra) -> io::Result<bool> {
        if now < self.next_tick() {
            return Ok(false);
        }
        let dt = now.duration_since(self.last).as_secs_f64();
        let cpu = (cpu_seconds(None), self.compositor.and_then(|p| cpu_seconds(Some(p))));
        let pct = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => format!("{:.1}", (a - b) / dt * 100.0),
            _ => "null".into(),
        };
        let c = &self.interval;
        let r = &c.render;
        let hit = c.hit_rate().map_or("null".into(), |h| format!("{h:.4}"));
        let line = format!(
            "{{\"t\":{:.3},\"bytes\":{},\"bytes_per_sec\":{:.0},\"frames\":{},\"fps\":{:.2},\"captures\":{},\
             \"dirty_tiles\":{},\"changed_tiles\":{},\"images_sent\":{},\"tiles_sent\":{},\"cache_hits\":{},\
             \"hit_rate\":{},\"solid_fills\":{},\"lowres_images\":{},\"refined_tiles\":{},\"evictions\":{},\
             \"payload_bytes\":{},\"encode_ms\":{:.2},\"cpu_pct\":{},\"compositor_cpu_pct\":{},\
             \"cached_tiles\":{},\"cache_mib\":{:.1},\"live_ephemerals\":{},\"scroll_shifts\":{}}}",
            now.duration_since(self.start).as_secs_f64(),
            c.bytes,
            c.bytes as f64 / dt,
            c.frames,
            c.frames as f64 / dt,
            c.captures,
            r.dirty_tiles,
            r.changed_tiles,
            r.images_sent,
            r.tiles_sent,
            r.cache_hits,
            hit,
            r.solid_fills,
            r.lowres_images,
            r.refined_tiles,
            r.evictions,
            r.payload_bytes,
            c.encode.as_secs_f64() * 1000.0,
            pct(cpu.0, self.cpu_last.0),
            pct(cpu.1, self.cpu_last.1),
            extra.cached_tiles,
            extra.cache_bytes as f64 / (1 << 20) as f64,
            extra.live_ephemerals,
            r.scroll_shifts,
        );
        if let Some(out) = &mut self.out {
            if let Err(e) = writeln!(out, "{line}").and_then(|_| out.flush()) {
                // Statistics are not worth ending the session over (a full disk, say).
                crate::log_warn!("--stats: {e}; statistics are no longer written");
                self.out = None;
            }
        }
        self.last_line = format!(
            "{:>7.1} KiB/s {:>5.1} fps  tiles {:>5} sent {:>5} hit {}  cpu {}%",
            c.bytes as f64 / dt / 1024.0,
            c.frames as f64 / dt,
            r.changed_tiles,
            r.tiles_sent,
            c.hit_rate().map_or("  -  ".into(), |h| format!("{:>4.0}%", h * 100.0)),
            pct(cpu.0, self.cpu_last.0),
        );
        self.total.add(&self.interval);
        self.interval = Counters::default();
        self.cpu_last = cpu;
        self.last = now;
        Ok(true)
    }

    pub fn summary(&self) -> String {
        let t = self.total();
        let secs = self.start.elapsed().as_secs_f64().max(1e-9);
        let cpu = cpu_seconds(None).zip(self.cpu_total_start.0).map(|(a, b)| (a - b) / secs * 100.0);
        format!(
            "termway: {:.1} s, {} bytes ({:.1} KiB/s), {} frames ({:.1} fps), {} tiles sent, {} cache hits ({}), cpu {}",
            secs,
            t.bytes,
            t.bytes as f64 / secs / 1024.0,
            t.frames,
            t.frames as f64 / secs,
            t.render.tiles_sent,
            t.render.cache_hits,
            t.hit_rate().map_or("n/a".into(), |h| format!("{:.1}%", h * 100.0)),
            cpu.map_or("n/a".into(), |c| format!("{c:.1}%")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_cpu_time() {
        assert!(cpu_seconds(None).is_some());
        assert!(cpu_seconds(Some(u32::MAX)).is_none());
    }

    #[test]
    fn hit_rate() {
        let mut c = Counters::default();
        assert_eq!(c.hit_rate(), None);
        c.render.cache_hits = 3;
        c.render.tiles_sent = 1;
        assert_eq!(c.hit_rate(), Some(0.75));
    }

    #[test]
    fn writes_json_lines() {
        let path = std::env::temp_dir().join(format!("termway-stats-{}.jsonl", std::process::id()));
        let mut s = Stats::new(Some(&path), None).unwrap();
        s.record_frame(
            100,
            &FrameStats { cache_hits: 1, tiles_sent: 1, ..Default::default() },
            Duration::from_millis(2),
            true,
        );
        s.record_frame(50, &FrameStats::default(), Duration::ZERO, false);
        let extra = Extra { cached_tiles: 1, cache_bytes: 1024, live_ephemerals: 0 };
        assert!(!s.tick(Instant::now(), &extra).unwrap());
        assert!(s.tick(Instant::now() + Duration::from_secs(1), &extra).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(
            text.contains("\"bytes\":150") && text.contains("\"frames\":1,") && text.contains("\"hit_rate\":0.5000"),
            "{text}"
        );
        assert_eq!(s.total().bytes, 150);
    }
}
