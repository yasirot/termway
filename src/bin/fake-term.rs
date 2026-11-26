//! Benchmark / end-to-end harness: a fake terminal on a pty.
//!
//! Starts `termway` on the slave side, answers its startup queries like Ghostty would
//! (optionally with errors), types scripted input, counts the bytes termway writes and
//! can decode the Kitty graphics stream to check what a terminal would show.

use rustix::event::{poll, PollFd, PollFlags};
use rustix::fd::{AsFd, OwnedFd};
use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
use rustix::termios::{self, Winsize};
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use termway::sim::{Flavor, Query, SimTerm};
use termway::term::{timespec, write_all};

const STREAMING_GRACE_S: f64 = 2.0;

const USAGE: &str = "\
Usage: fake-term [OPTIONS] -- TERMWAY [ARGS...]
       fake-term --compare A.ppm B.ppm [--rows-px N]

  --cols N --rows N --cell WxH     terminal geometry (default 282x60, 9x21)
  --ioctl-pixels                   report pixel size in TIOCGWINSZ (default: 0 like distrobox)
  --no-cell-reply --no-window-reply --no-da1 --no-graphics --no-version-reply --bad-cell-reply
  --emulate kitty|wezterm          image model and XTVERSION answer (default kitty: answers
                                   like Ghostty)
  --duration S                     total run time before quitting (default 10)
  --warmup S                       seconds excluded from the measurement (default 2)
  --send-at S:TEXT                 type TEXT at time S (escapes: \\r \\n \\e \\t \\xNN)
  --type RATE:TEXT                 type TEXT cyclically at RATE chars/s after the warmup
  --resize-at S:COLSxROWS          change the pty size and send SIGWINCH
  --throttle-kbps N                read at most N KiB/s (simulated link)
  --decode                         model the terminal and report protocol errors
  --snapshot FILE                  with --decode: write the modelled screen (PPM) when idle at the end,
                                   or between two synchronized frames if the output never pauses
  --quit-key BYTES                 bytes that make termway quit (default \\e)
  --report FILE                    JSON summary
  --dump-output FILE               save every byte termway wrote
  --signal-at S:SIG                send INT, TERM or HUP to termway at time S instead of quitting
  --label TEXT                     label stored in the report
  --stats FILE                     termway --stats file to merge into the report
";

struct Opts {
    cols: u16,
    rows: u16,
    cell: (u16, u16),
    ioctl_pixels: bool,
    reply_cell: bool,
    reply_window: bool,
    reply_da1: bool,
    reply_version: bool,
    reply_graphics: bool,
    bad_cell: bool,
    flavor: Flavor,
    duration: f64,
    warmup: f64,
    sends: Vec<(f64, Vec<u8>)>,
    typing: Option<(f64, Vec<u8>)>,
    resizes: Vec<(f64, u16, u16)>,
    throttle: Option<u64>,
    decode: bool,
    snapshot: Option<PathBuf>,
    quit: Vec<u8>,
    report: Option<PathBuf>,
    label: String,
    stats: Option<PathBuf>,
    dump: Option<PathBuf>,
    signal: Option<(f64, i32)>,
    child: Vec<String>,
}

fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'r' => out.push(b'\r'),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'e' => out.push(0x1b),
                b'\\' => out.push(b'\\'),
                b'x' if i + 2 < b.len() => {
                    let v = u8::from_str_radix(&s[i + 1..i + 3], 16).expect("bad \\x escape");
                    out.push(v);
                    i += 2;
                }
                c => out.extend_from_slice(&[b'\\', c]),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    out
}

fn split_at_colon(s: &str) -> (f64, &str) {
    let (t, rest) = s.split_once(':').expect("expected S:VALUE");
    (t.parse().expect("bad time"), rest)
}

fn parse_args() -> Result<Opts, String> {
    let mut o = Opts {
        cols: 282,
        rows: 60,
        cell: (9, 21),
        ioctl_pixels: false,
        reply_cell: true,
        reply_window: true,
        reply_da1: true,
        reply_version: true,
        reply_graphics: true,
        bad_cell: false,
        flavor: Flavor::Kitty,
        duration: 10.0,
        warmup: 2.0,
        sends: Vec::new(),
        typing: None,
        resizes: Vec::new(),
        throttle: None,
        decode: false,
        snapshot: None,
        quit: vec![0x1b],
        report: None,
        label: String::new(),
        stats: None,
        dump: None,
        signal: None,
        child: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut v = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--cols" => o.cols = v()?.parse().map_err(|_| "bad --cols")?,
            "--rows" => o.rows = v()?.parse().map_err(|_| "bad --rows")?,
            "--cell" => {
                let s = v()?;
                let (w, h) = s.split_once('x').ok_or("bad --cell")?;
                o.cell = (w.parse().map_err(|_| "bad --cell")?, h.parse().map_err(|_| "bad --cell")?);
            }
            "--ioctl-pixels" => o.ioctl_pixels = true,
            "--no-cell-reply" => o.reply_cell = false,
            "--no-window-reply" => o.reply_window = false,
            "--no-da1" => o.reply_da1 = false,
            "--no-version-reply" => o.reply_version = false,
            "--no-graphics" => o.reply_graphics = false,
            "--bad-cell-reply" => o.bad_cell = true,
            "--emulate" => {
                o.flavor = match v()?.as_str() {
                    "kitty" => Flavor::Kitty,
                    "wezterm" => Flavor::WezTerm,
                    _ => return Err("bad --emulate (kitty or wezterm)".into()),
                }
            }
            "--duration" => o.duration = v()?.parse().map_err(|_| "bad --duration")?,
            "--warmup" => o.warmup = v()?.parse().map_err(|_| "bad --warmup")?,
            "--send-at" => {
                let s = v()?;
                let (t, text) = split_at_colon(&s);
                o.sends.push((t, unescape(text)));
            }
            "--type" => {
                let s = v()?;
                let (rate, text) = split_at_colon(&s);
                o.typing = Some((rate, unescape(text)));
            }
            "--resize-at" => {
                let s = v()?;
                let (t, size) = split_at_colon(&s);
                let (c, r) = size.split_once('x').ok_or("bad --resize-at")?;
                o.resizes.push((t, c.parse().map_err(|_| "bad cols")?, r.parse().map_err(|_| "bad rows")?));
            }
            "--throttle-kbps" => o.throttle = Some(v()?.parse().map_err(|_| "bad --throttle-kbps")?),
            "--decode" => o.decode = true,
            "--snapshot" => o.snapshot = Some(v()?.into()),
            "--quit-key" => o.quit = unescape(&v()?),
            "--report" => o.report = Some(v()?.into()),
            "--label" => o.label = v()?,
            "--stats" => o.stats = Some(v()?.into()),
            "--dump-output" => o.dump = Some(v()?.into()),
            "--signal-at" => {
                let s = v()?;
                let (t, sig) = split_at_colon(&s);
                let sig = match sig {
                    "INT" => libc::SIGINT,
                    "TERM" => libc::SIGTERM,
                    "HUP" => libc::SIGHUP,
                    other => return Err(format!("unsupported signal {other}")),
                };
                o.signal = Some((t, sig));
            }
            "--" => {
                o.child = it.by_ref().collect();
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    if o.child.is_empty() {
        return Err("missing -- TERMWAY command".into());
    }
    Ok(o)
}

/// Finds startup queries without full decoding (keeps a tail for split sequences).
struct QueryScanner {
    tail: Vec<u8>,
}

impl QueryScanner {
    fn scan(&mut self, data: &[u8], out: &mut Vec<Query>) {
        const PATTERNS: [(&[u8], Query); 5] = [
            (b"\x1b[16t", Query::CellSize),
            (b"\x1b[14t", Query::WindowSize),
            (b"\x1b[?1016$p", Query::Mode(1016)),
            (b"\x1b[>0q", Query::Version),
            (b"\x1b[c", Query::PrimaryDeviceAttributes),
        ];
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(data);
        let mut i = 0;
        while i < buf.len() {
            if buf[i] == 0x1b {
                if let Some((_, q)) = PATTERNS.iter().find(|(p, _)| buf[i..].starts_with(p)) {
                    out.push(q.clone());
                } else if buf[i..].starts_with(b"\x1b_Gi=31,") && buf[i..].windows(4).take(64).any(|w| w == b"a=q,") {
                    out.push(Query::Graphics { id: 31 });
                }
            }
            i += 1;
        }
        let keep = buf.len().saturating_sub(64);
        self.tail = buf[keep..].to_vec();
    }
}

fn ppm(path: &PathBuf, w: u32, h: u32, rgb: &[u8]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(fs::File::create(path)?);
    write!(f, "P6\n{w} {h}\n255\n")?;
    f.write_all(rgb)?;
    f.flush()
}

fn read_ppm(path: &str) -> (u32, u32, Vec<u8>) {
    let data = fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while data[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..i]).into_owned());
    }
    assert_eq!(fields[0], "P6", "{path}: not a binary PPM");
    (fields[1].parse().unwrap(), fields[2].parse().unwrap(), data[i + 1..].to_vec())
}

fn compare(args: &[String]) -> i32 {
    let (aw, ah, a) = read_ppm(&args[0]);
    let (bw, _bh, b) = read_ppm(&args[1]);
    let rows = args.iter().position(|s| s == "--rows-px").map(|i| args[i + 1].parse::<u32>().unwrap()).unwrap_or(ah);
    assert_eq!(aw, bw, "widths differ");
    let n = (aw * rows) as usize * 3;
    let diff = a[..n].chunks(3).zip(b[..n].chunks(3)).filter(|(x, y)| x != y).count();
    println!("compared {aw}x{rows}: {diff} differing pixels");
    (diff != 0) as i32
}

struct Buckets {
    start: Instant,
    per_sec: Vec<u64>,
}

impl Buckets {
    fn add(&mut self, n: usize) {
        let s = self.start.elapsed().as_secs() as usize;
        if self.per_sec.len() <= s {
            self.per_sec.resize(s + 1, 0);
        }
        self.per_sec[s] += n as u64;
    }
}

fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// Averages selected fields of termway's JSONL stats within the measurement window.
fn merge_stats(path: &PathBuf, from: f64, to: f64) -> String {
    let Ok(text) = fs::read_to_string(path) else { return "null".into() };
    let field = |line: &str, key: &str| -> Option<f64> {
        let pat = format!("\"{key}\":");
        let i = line.find(&pat)? + pat.len();
        let rest = &line[i..];
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        rest[..end].parse().ok()
    };
    let lines: Vec<&str> =
        text.lines().filter(|l| field(l, "t").is_some_and(|t| t > from + 0.5 && t <= to + 0.5)).collect();
    let keys = [
        "bytes_per_sec",
        "fps",
        "captures",
        "dirty_tiles",
        "changed_tiles",
        "images_sent",
        "tiles_sent",
        "cache_hits",
        "solid_fills",
        "lowres_images",
        "refined_tiles",
        "evictions",
        "cpu_pct",
        "compositor_cpu_pct",
        "encode_ms",
    ];
    let mut parts = vec![format!("\"seconds\":{}", lines.len())];
    for k in keys {
        let vals: Vec<f64> = lines.iter().filter_map(|l| field(l, k)).collect();
        parts.push(format!("\"{k}\":{}", mean(&vals).map_or("null".into(), |m| format!("{m:.3}"))));
    }
    let hits: f64 = lines.iter().filter_map(|l| field(l, "cache_hits")).sum();
    let sent: f64 = lines.iter().filter_map(|l| field(l, "tiles_sent")).sum();
    let rate = if hits + sent > 0.0 { format!("{:.4}", hits / (hits + sent)) } else { "null".into() };
    parts.push(format!("\"hit_rate\":{rate}"));
    format!("{{{}}}", parts.join(","))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--compare") {
        std::process::exit(compare(&args[1..]));
    }
    let o = parse_args().unwrap_or_else(|e| {
        eprintln!("fake-term: {e}\n{USAGE}");
        std::process::exit(2);
    });

    // CLOEXEC: leaked into termway, these fds would reach everything it starts and keep
    // the pty open after termway exits.
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).expect("openpt");
    grantpt(&master).expect("grantpt");
    unlockpt(&master).expect("unlockpt");
    let name = ptsname(&master, Vec::new()).expect("ptsname");
    let slave: OwnedFd = rustix::fs::open(
        name.as_c_str(),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .expect("open pty slave");
    let winsize = |cols: u16, rows: u16| Winsize {
        ws_col: cols,
        ws_row: rows,
        ws_xpixel: if o.ioctl_pixels { cols * o.cell.0 } else { 0 },
        ws_ypixel: if o.ioctl_pixels { rows * o.cell.1 } else { 0 },
    };
    termios::tcsetwinsize(&slave, winsize(o.cols, o.rows)).expect("TIOCSWINSZ");

    let stdin = Stdio::from(std::fs::File::from(slave.try_clone().unwrap()));
    let stdout = Stdio::from(std::fs::File::from(slave.try_clone().unwrap()));
    let mut child = Command::new(&o.child[0])
        .args(&o.child[1..])
        .stdin(stdin)
        .stdout(stdout)
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| panic!("cannot start {}: {e}", o.child[0]));
    drop(slave);

    let start = Instant::now();
    let mut buckets = Buckets { start, per_sec: Vec::new() };
    let mut scanner = QueryScanner { tail: Vec::new() };
    let new_sim = |cols: u16, rows: u16| {
        let mut s = SimTerm::new(cols as u32, rows as u32, o.cell.0 as u32, o.cell.1 as u32);
        s.flavor = o.flavor;
        // Answering as Ghostty 1.3 also means modelling its zlib inflater's limit.
        if o.flavor == Flavor::Kitty && o.reply_version {
            s.zlib_limit = Some(termway::render::PNG_MIN_BYTES);
        }
        s
    };
    let mut sim = o.decode.then(|| new_sim(o.cols, o.rows));
    let mut queries = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut sends = o.sends.clone();
    sends.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut resizes = o.resizes.clone();
    let mut typed = 0usize;
    let mut quit_sent: Option<Instant> = None;
    let mut last_output = Instant::now();
    let mut snapshot_done = o.snapshot.is_none();
    let mut total_bytes = 0u64;
    let mut window_bytes = 0u64;
    // Token bucket for --throttle-kbps, allowing bursts of 20 ms worth of data.
    let mut tokens = 0.0f64;
    let mut tokens_at = Instant::now();
    let exit_status;
    let mut grid = (o.cols, o.rows);
    let mut dump = o.dump.as_ref().map(|p| std::io::BufWriter::new(fs::File::create(p).expect("create --dump-output")));

    loop {
        let t = start.elapsed().as_secs_f64();
        if let Some(status) = child.try_wait().expect("wait") {
            exit_status = Some(status);
            // Drain what is left.
            while let Ok(n) = rustix::io::read(&master, &mut buf) {
                if n == 0 {
                    break;
                }
                total_bytes += n as u64;
                if let Some(s) = &mut sim {
                    s.feed(&buf[..n]);
                }
                if n < buf.len() {
                    break;
                }
            }
            break;
        }
        while sends.first().is_some_and(|(at, _)| t >= *at) {
            let (_, text) = sends.remove(0);
            let _ = write_all(master.as_fd(), &text);
        }
        if let Some(pos) = resizes.iter().position(|(at, _, _)| t >= *at) {
            let (_, c, r) = resizes.remove(pos);
            termios::tcsetwinsize(&master, winsize(c, r)).expect("TIOCSWINSZ");
            grid = (c, r);
            if let Some(s) = &mut sim {
                // A resized terminal: rebuild the model at the new size (placements reset).
                let mut n = new_sim(c, r);
                n.feed(b"\x1b[?1049h");
                *s = n;
            }
            // SAFETY: signalling our own child.
            unsafe { libc::kill(child.id() as i32, libc::SIGWINCH) };
        }
        if let Some((rate, text)) = &o.typing {
            if t >= o.warmup && quit_sent.is_none() && t < o.duration {
                let due = ((t - o.warmup) * rate) as usize;
                while typed < due {
                    let _ = write_all(master.as_fd(), &text[typed % text.len()..typed % text.len() + 1]);
                    typed += 1;
                }
            }
        }
        if let Some((at, sig)) = o.signal {
            if quit_sent.is_none() && t >= at {
                // SAFETY: signalling our own child.
                unsafe { libc::kill(child.id() as i32, sig) };
                quit_sent = Some(Instant::now());
            }
        }
        if quit_sent.is_none() && t >= o.duration {
            let idle = last_output.elapsed() > Duration::from_millis(800);
            // Phase 1 streams full frames at a fixed rate and never goes idle.
            let between_frames = t >= o.duration + STREAMING_GRACE_S && sim.as_ref().is_some_and(|s| s.sync_depth == 0);
            if !snapshot_done && (idle || between_frames) {
                if let (Some(s), Some(path)) = (&sim, &o.snapshot) {
                    ppm(path, s.cols * s.cell_w, s.rows * s.cell_h, &s.render()).expect("write snapshot");
                }
                snapshot_done = true;
            }
            if snapshot_done || t >= o.duration + 10.0 {
                let _ = write_all(master.as_fd(), &o.quit);
                quit_sent = Some(Instant::now());
            }
        }
        if quit_sent.is_some_and(|q| q.elapsed() > Duration::from_secs(10)) {
            eprintln!("fake-term: termway did not exit, killing it");
            let _ = child.kill();
        }

        let mut limit = buf.len();
        if let Some(kbps) = o.throttle {
            let rate = kbps as f64 * 1024.0;
            let now = Instant::now();
            tokens = (tokens + now.duration_since(tokens_at).as_secs_f64() * rate).min(rate * 0.02);
            tokens_at = now;
            if tokens < 1.0 {
                std::thread::sleep(Duration::from_secs_f64((1.0 - tokens) / rate).max(Duration::from_micros(200)));
                continue;
            }
            limit = limit.min(tokens as usize);
        }
        let mut fds = [PollFd::new(&master, PollFlags::IN)];
        match poll(&mut fds, Some(&timespec(Duration::from_millis(5)))) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => panic!("poll: {e}"),
        }
        if fds[0].revents().contains(PollFlags::IN) {
            let n = match rustix::io::read(&master, &mut buf[..limit]) {
                Ok(n) => n,
                Err(rustix::io::Errno::IO) => 0,
                Err(e) => panic!("read: {e}"),
            };
            tokens -= n as f64;
            if n > 0 {
                if let Some(d) = &mut dump {
                    d.write_all(&buf[..n]).expect("write --dump-output");
                }
                last_output = Instant::now();
                total_bytes += n as u64;
                buckets.add(n);
                let t = start.elapsed().as_secs_f64();
                if t >= o.warmup && t < o.duration {
                    window_bytes += n as u64;
                }
                match &mut sim {
                    Some(s) => {
                        s.feed(&buf[..n]);
                        queries.append(&mut s.queries);
                    }
                    None => scanner.scan(&buf[..n], &mut queries),
                }
            }
        }
        for q in queries.drain(..) {
            let reply: Vec<u8> = match q {
                Query::CellSize if o.bad_cell => b"\x1b[6;0;0t".to_vec(),
                Query::CellSize if o.reply_cell => format!("\x1b[6;{};{}t", o.cell.1, o.cell.0).into_bytes(),
                Query::WindowSize if o.reply_window => {
                    format!("\x1b[4;{};{}t", grid.1 as u32 * o.cell.1 as u32, grid.0 as u32 * o.cell.0 as u32)
                        .into_bytes()
                }
                Query::Mode(1016) => b"\x1b[?1016;2$y".to_vec(),
                Query::PrimaryDeviceAttributes if o.reply_da1 => b"\x1b[?62;22c".to_vec(),
                Query::Graphics { id } if o.reply_graphics => format!("\x1b_Gi={id};OK\x1b\\").into_bytes(),
                Query::Version if o.reply_version => match o.flavor {
                    Flavor::WezTerm => b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\".to_vec(),
                    Flavor::Kitty => b"\x1bP>|ghostty 1.3.1 (fake-term)\x1b\\".to_vec(),
                },
                _ => Vec::new(),
            };
            let _ = write_all(master.as_fd(), &reply);
        }
    }

    // The pty keeps the slave's attributes; a correct exit restores canonical mode + echo.
    let tty = termios::tcgetattr(&master).expect("tcgetattr");
    let restored = tty.local_modes.contains(termios::LocalModes::ICANON | termios::LocalModes::ECHO);
    let elapsed = start.elapsed().as_secs_f64();
    let window = (o.duration.min(elapsed) - o.warmup).max(1e-9);
    let code = exit_status.and_then(|s| s.code()).unwrap_or(-1);
    let (errors, placements, images, max_images, max_image_bytes, alt) = match &sim {
        Some(s) => (
            s.errors.len(),
            s.placement_count(),
            s.image_count(),
            s.stats.max_images,
            s.stats.max_image_bytes,
            s.alt_screen,
        ),
        None => (0, 0, 0, 0, 0, false),
    };
    if let Some(s) = &sim {
        for e in s.errors.iter().take(10) {
            eprintln!("fake-term: protocol error: {e}");
        }
    }
    let stats = o.stats.as_ref().map_or("null".into(), |p| merge_stats(p, o.warmup, o.duration));
    let report = format!(
        "{{\"label\":{:?},\"exit_code\":{code},\"elapsed\":{elapsed:.3},\"total_bytes\":{total_bytes},\
         \"window_seconds\":{window:.3},\"window_bytes\":{window_bytes},\"bytes_per_sec\":{:.0},\
         \"per_second\":{:?},\"decode\":{},\"protocol_errors\":{errors},\"placements_at_exit\":{placements},\
         \"images_at_exit\":{images},\"tty_restored\":{restored},\"alt_screen_at_exit\":{alt},\"max_images\":{max_images},\"max_image_bytes\":{max_image_bytes},\"termway\":{stats}}}",
        o.label,
        window_bytes as f64 / window,
        buckets.per_sec,
        o.decode,
    );
    println!("{report}");
    if let Some(p) = &o.report {
        fs::write(p, format!("{report}\n")).expect("write report");
    }
    let failed = errors > 0 || !restored || alt || (o.signal.is_none() && code != 0);
    std::process::exit(failed as i32);
}
