//! Command line parsing (hand-written to avoid a dependency).

use crate::compositor::Kind;
use crate::render::{Compat, Phase};
use std::path::PathBuf;
use std::time::Duration;

pub const USAGE: &str = "\
Usage: termway [OPTIONS] [-- COMMAND [ARGS...]]

Runs COMMAND (default: foot) in a headless Wayland compositor and draws it in this
terminal with the Kitty graphics protocol. stdout must be the terminal.

Options:
  --phase N               1 full frames, 2 damage rects, 3 tile cache, 4 tile cache +
                          half resolution while moving + scrolling by moving already
                          sent pixels (default 4)
  --compositor NAME       sway or cage (default sway)
  --x11                   run COMMAND (default: i3) as an X11 session on a rootful
                          Xwayland, for X11 window managers and desktops
  --sway-config FILE      include FILE (bar, key bindings, gaps, exec...) in sway's
                          configuration, for a sway desktop without nesting sway;
                          COMMAND is then optional (default: none) and the session
                          ends with sway; output mode and scale stay termway's
  --attach DISPLAY        use a running compositor (WAYLAND_DISPLAY name or socket path)
                          instead of starting one; COMMAND is not run
  --output NAME           output to capture (default: the first one)
  --no-resize-output      do not change the output mode to match the terminal
  --scale auto|F          output scale 1-4, so apps are not tiny on HiDPI terminals;
                          auto derives it from the cell height (default auto)
  --max-fps N             capture rate limit (default 30)
  --tile WxH              tile size in cells (default 1x1)
  --cache-mb N            terminal-side tile cache budget in MiB, estimated at 4 bytes
                          per pixel (default 128)
  --zlib-level N          zlib level for frames, rectangles and half-res images;
                          0 disables compression (default 6)
  --tile-zlib-level N     zlib level for cached tiles (default 1)
  --no-solid-fill         send uniform tiles as images instead of cell backgrounds
  --threads N             worker threads for converting, hashing and compressing
                          frames (default: CPU count, at most 8)
  --lowres-threshold F    phase 4: fraction of changed tiles that counts as motion
                          (default 0.05)
  --refine-delay-ms N     phase 4: quiet time before full-resolution refinement (default 200)
  --no-scroll-copy        phase 4: redraw scrolling areas instead of moving the pixels
                          already in the terminal
  --status-line           reserve the bottom row for live statistics
  --quit-key KEY          esc, ctrl-] or none (default esc; esc is then not forwarded)
  --mouse MODE            auto, pixels, cells or off (default auto)
  --overlay-cursor        include the compositor's cursor in captured frames
  --no-sync               do not wrap frames in synchronized output (DEC 2026)
  --graphics-compat MODE  auto, kitty (kitty, Ghostty) or wezterm; auto asks the
                          terminal's name with XTVERSION (default auto)
  --large-images FORMAT   auto, zlib or png: how images over 32 KiB are sent; auto
                          picks png on Ghostty, whose 1.3 zlib decoder can crash on
                          them (default auto)
  --cell-size WxH         skip cell size detection
  --skip-graphics-check   do not require an answer to the Kitty graphics query
  --log FILE              write log messages to FILE (default: printed on exit)
  --stats FILE            write per-second statistics as JSON lines
  --compositor-log FILE   compositor stdout/stderr (default: kept only until exit)
  --dump-frame FILE       write the last captured frame as PPM on exit
  -v, --verbose           debug logging
  -h, --help              show this help
  -V, --version           show the version
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuitKey {
    Escape,
    CtrlBracket,
    None,
}

/// Cell height that `--scale auto` maps to scale 1 (the 9x21 px reference terminal).
const AUTO_SCALE_CELL_H: f64 = 21.0;
pub const MAX_SCALE: f64 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scale {
    Auto,
    Fixed(f64),
}

impl Scale {
    /// Apps size their text in logical pixels, while the output mode follows the
    /// terminal in physical pixels; scaling keeps app text proportional to the
    /// terminal's. Quarter steps match the usual desktop scaling settings.
    pub fn resolve(self, cell_h: u32) -> f64 {
        match self {
            Self::Fixed(f) => f,
            Self::Auto => ((cell_h as f64 / AUTO_SCALE_CELL_H * 4.0).round() / 4.0).clamp(1.0, MAX_SCALE),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseMode {
    Auto,
    Pixels,
    Cells,
    Off,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub phase: Phase,
    pub compositor: Kind,
    pub sway_config: Option<PathBuf>,
    pub x11: bool,
    pub attach: Option<String>,
    pub output: Option<String>,
    pub resize_output: bool,
    pub scale: Scale,
    pub max_fps: u32,
    pub tile: (u32, u32),
    pub cache_mb: u64,
    pub zlib_level: u32,
    pub tile_zlib_level: u32,
    pub solid_fill: bool,
    pub threads: usize,
    pub lowres_threshold: f64,
    pub refine_delay: Duration,
    pub scroll_copy: bool,
    pub status_line: bool,
    pub quit_key: QuitKey,
    pub mouse: MouseMode,
    pub overlay_cursor: bool,
    pub sync_output: bool,
    /// `None`: detect from the terminal's XTVERSION answer.
    pub compat: Option<Compat>,
    /// Send large images as PNG; `None`: detect from the terminal's XTVERSION answer.
    pub large_png: Option<bool>,
    pub cell_size: Option<(u32, u32)>,
    pub skip_graphics_check: bool,
    pub log: Option<PathBuf>,
    pub stats: Option<PathBuf>,
    pub compositor_log: Option<PathBuf>,
    pub dump_frame: Option<PathBuf>,
    pub verbose: bool,
    pub command: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            phase: Phase::Progressive,
            compositor: Kind::Sway,
            sway_config: None,
            x11: false,
            attach: None,
            output: None,
            resize_output: true,
            scale: Scale::Auto,
            max_fps: 30,
            tile: (1, 1),
            cache_mb: 128,
            zlib_level: 6,
            tile_zlib_level: 1,
            solid_fill: true,
            threads: crate::par::default_threads(),
            lowres_threshold: 0.05,
            refine_delay: Duration::from_millis(200),
            scroll_copy: true,
            status_line: false,
            quit_key: QuitKey::Escape,
            mouse: MouseMode::Auto,
            overlay_cursor: false,
            sync_output: true,
            compat: None,
            large_png: None,
            cell_size: None,
            skip_graphics_check: false,
            log: None,
            stats: None,
            compositor_log: None,
            dump_frame: None,
            verbose: false,
            command: vec!["foot".into()],
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Run(Box<Options>),
    Help,
    Version,
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Action, String> {
    let mut o = Options::default();
    let mut command_given = false;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{name} needs a value"))
        };
        fn num<T: std::str::FromStr>(name: &str, v: String) -> Result<T, String> {
            v.parse().map_err(|_| format!("invalid value {v:?} for {name}"))
        }
        match flag.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "-V" | "--version" => return Ok(Action::Version),
            "-v" | "--verbose" => o.verbose = true,
            "--" => {
                let rest: Vec<String> = it.by_ref().collect();
                if rest.is_empty() {
                    return Err("-- must be followed by a command".into());
                }
                o.command = rest;
                command_given = true;
            }
            "--phase" => {
                let n: u32 = num(&flag, value(&flag)?)?;
                o.phase = Phase::from_number(n).ok_or_else(|| format!("--phase must be 1-4, got {n}"))?;
            }
            "--compositor" => {
                let v = value(&flag)?;
                o.compositor = Kind::parse(&v).ok_or_else(|| format!("unknown compositor {v:?} (sway or cage)"))?;
            }
            "--sway-config" => o.sway_config = Some(value(&flag)?.into()),
            "--x11" => o.x11 = true,
            "--attach" => o.attach = Some(value(&flag)?),
            "--output" => o.output = Some(value(&flag)?),
            "--no-resize-output" => o.resize_output = false,
            "--scale" => {
                let v = value(&flag)?;
                o.scale = match v.as_str() {
                    "auto" => Scale::Auto,
                    _ => match v.parse::<f64>() {
                        Ok(f) if (1.0..=MAX_SCALE).contains(&f) => Scale::Fixed(f),
                        _ => return Err(format!("invalid --scale {v:?}, expected auto or a number within 1-4")),
                    },
                };
            }
            "--max-fps" => {
                o.max_fps = num(&flag, value(&flag)?)?;
                if !(1..=240).contains(&o.max_fps) {
                    return Err("--max-fps must be 1-240".into());
                }
            }
            "--tile" => {
                let v = value(&flag)?;
                o.tile = parse_size(&v).ok_or_else(|| format!("invalid --tile {v:?}, expected WxH in cells"))?;
            }
            "--cache-mb" => o.cache_mb = num(&flag, value(&flag)?)?,
            "--zlib-level" => {
                o.zlib_level = num(&flag, value(&flag)?)?;
                if o.zlib_level > 9 {
                    return Err("--zlib-level must be 0-9".into());
                }
            }
            "--tile-zlib-level" => {
                o.tile_zlib_level = num(&flag, value(&flag)?)?;
                if o.tile_zlib_level > 9 {
                    return Err("--tile-zlib-level must be 0-9".into());
                }
            }
            "--no-solid-fill" => o.solid_fill = false,
            "--threads" => {
                o.threads = num(&flag, value(&flag)?)?;
                if !(1..=64).contains(&o.threads) {
                    return Err("--threads must be 1-64".into());
                }
            }
            "--lowres-threshold" => {
                o.lowres_threshold = num(&flag, value(&flag)?)?;
                if !(0.0..=1.0).contains(&o.lowres_threshold) {
                    return Err("--lowres-threshold must be within 0..1".into());
                }
            }
            "--refine-delay-ms" => {
                let ms: u64 = num(&flag, value(&flag)?)?;
                if ms > 60_000 {
                    return Err("--refine-delay-ms must be 0-60000".into());
                }
                o.refine_delay = Duration::from_millis(ms);
            }
            "--no-scroll-copy" => o.scroll_copy = false,
            "--status-line" => o.status_line = true,
            "--quit-key" => {
                o.quit_key = match value(&flag)?.as_str() {
                    "esc" => QuitKey::Escape,
                    "ctrl-]" => QuitKey::CtrlBracket,
                    "none" => QuitKey::None,
                    v => return Err(format!("unknown --quit-key {v:?} (esc, ctrl-] or none)")),
                }
            }
            "--mouse" => {
                o.mouse = match value(&flag)?.as_str() {
                    "auto" => MouseMode::Auto,
                    "pixels" => MouseMode::Pixels,
                    "cells" => MouseMode::Cells,
                    "off" => MouseMode::Off,
                    v => return Err(format!("unknown --mouse {v:?}")),
                }
            }
            "--overlay-cursor" => o.overlay_cursor = true,
            "--no-sync" => o.sync_output = false,
            "--graphics-compat" => {
                o.compat = match value(&flag)?.as_str() {
                    "auto" => None,
                    "kitty" => Some(Compat::Kitty),
                    "wezterm" => Some(Compat::WezTerm),
                    v => return Err(format!("unknown --graphics-compat {v:?} (auto, kitty or wezterm)")),
                }
            }
            "--large-images" => {
                o.large_png = match value(&flag)?.as_str() {
                    "auto" => None,
                    "zlib" => Some(false),
                    "png" => Some(true),
                    v => return Err(format!("unknown --large-images {v:?} (auto, zlib or png)")),
                }
            }
            "--cell-size" => {
                let v = value(&flag)?;
                o.cell_size = Some(parse_size(&v).ok_or_else(|| format!("invalid --cell-size {v:?}"))?);
            }
            "--skip-graphics-check" => o.skip_graphics_check = true,
            "--log" => o.log = Some(value(&flag)?.into()),
            "--stats" => o.stats = Some(value(&flag)?.into()),
            "--compositor-log" => o.compositor_log = Some(value(&flag)?.into()),
            "--dump-frame" => o.dump_frame = Some(value(&flag)?.into()),
            other => {
                let long = format!("-{}", other.split('=').next().unwrap_or(other));
                if !other.starts_with("--") && USAGE.contains(&format!(" {long} ")) {
                    return Err(format!("unknown option {other:?}; did you mean {long}?"));
                }
                return Err(format!("unknown option {other:?} (see --help)"));
            }
        }
    }
    if o.x11 {
        if o.compositor != Kind::Sway {
            return Err("--x11 needs --compositor sway".into());
        }
        if o.attach.is_some() {
            return Err("--x11 cannot be used with --attach (termway does not start that compositor)".into());
        }
        if !command_given {
            o.command = vec!["i3".into()];
        }
    }
    if o.sway_config.is_some() {
        if o.compositor != Kind::Sway {
            return Err("--sway-config needs --compositor sway".into());
        }
        if o.attach.is_some() {
            return Err("--sway-config cannot be used with --attach (termway does not start that compositor)".into());
        }
        // A desktop config starts its own apps with `exec`.
        if !command_given && !o.x11 {
            o.command.clear();
        }
    }
    Ok(Action::Run(Box::new(o)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Options, String> {
        match parse(args.iter().map(|s| s.to_string()))? {
            Action::Run(o) => Ok(*o),
            a => panic!("unexpected {a:?}"),
        }
    }

    #[test]
    fn defaults() {
        assert_eq!(run(&[]).unwrap(), Options::default());
    }

    #[test]
    fn options_and_command() {
        let o = run(&["--phase", "2", "--tile=2x1", "--cache-mb", "64", "--status-line", "--", "foot", "-e", "htop"])
            .unwrap();
        assert_eq!(o.phase, Phase::Damage);
        assert_eq!(o.tile, (2, 1));
        assert_eq!(o.cache_mb, 64);
        assert!(o.status_line);
        assert_eq!(o.command, ["foot", "-e", "htop"]);
    }

    #[test]
    fn command_keeps_its_own_flags() {
        let o = run(&["--", "foot", "--phase", "9"]).unwrap();
        assert_eq!(o.phase, Phase::Progressive);
        assert_eq!(o.command, ["foot", "--phase", "9"]);
    }

    #[test]
    fn errors_are_explicit() {
        for (args, needle) in [
            (&["--phase", "5"][..], "1-4"),
            (&["--phase"][..], "needs a value"),
            (&["--tile", "0x2"][..], "invalid --tile"),
            (&["--compositor", "weston"][..], "unknown compositor"),
            (&["--bogus"][..], "unknown option"),
            (&["-scale", "1"][..], "unknown option \"-scale\"; did you mean --scale?"),
            (&["-verbose"][..], "did you mean --verbose?"),
            (&["-bogus"][..], "unknown option \"-bogus\" (see --help)"),
            (&["--"][..], "followed by a command"),
            (&["--max-fps", "0"][..], "1-240"),
            (&["--cell-size", "9x"][..], "invalid --cell-size"),
            (&["--quit-key", "f1"][..], "unknown --quit-key"),
            (&["--scale", "0.5"][..], "invalid --scale"),
            (&["--scale", "big"][..], "invalid --scale"),
            (&["--scale", "NaN"][..], "invalid --scale"),
            (&["--graphics-compat", "xterm"][..], "unknown --graphics-compat"),
            (&["--large-images", "jpeg"][..], "unknown --large-images"),
            (&["--threads", "0"][..], "1-64"),
            (&["--refine-delay-ms", "18446744073709551615"][..], "0-60000"),
        ] {
            let e = run(args).unwrap_err();
            assert!(e.contains(needle), "{args:?}: {e}");
        }
    }

    #[test]
    fn sway_config_makes_the_command_optional() {
        let o = run(&["--sway-config", "desk.conf"]).unwrap();
        assert_eq!(o.sway_config.as_deref(), Some(std::path::Path::new("desk.conf")));
        assert!(o.command.is_empty(), "no default app");
        let o = run(&["--sway-config=desk.conf", "--", "firefox"]).unwrap();
        assert_eq!(o.command, ["firefox"]);
        assert_eq!(run(&[]).unwrap().command, ["foot"], "the default app stays without it");
        for (args, needle) in [
            (&["--sway-config", "d", "--compositor", "cage"][..], "needs --compositor sway"),
            (&["--compositor", "cage", "--sway-config", "d"][..], "needs --compositor sway"),
            (&["--sway-config", "d", "--attach", "wayland-1"][..], "cannot be used with --attach"),
            (&["--sway-config"][..], "needs a value"),
        ] {
            let e = run(args).unwrap_err();
            assert!(e.contains(needle), "{args:?}: {e}");
        }
    }

    #[test]
    fn x11_runs_i3_unless_told_otherwise() {
        assert_eq!(run(&["--x11"]).unwrap().command, ["i3"]);
        assert_eq!(run(&["--x11", "--", "openbox-session"]).unwrap().command, ["openbox-session"]);
        assert_eq!(
            run(&["--x11", "--sway-config", "d"]).unwrap().command,
            ["i3"],
            "--sway-config keeps the X11 session"
        );
        assert!(!run(&[]).unwrap().x11);
        for (args, needle) in [
            (&["--x11", "--compositor", "cage"][..], "--x11 needs --compositor sway"),
            (&["--x11", "--attach", "wayland-1"][..], "--x11 cannot be used with --attach"),
        ] {
            let e = run(args).unwrap_err();
            assert!(e.contains(needle), "{args:?}: {e}");
        }
    }

    #[test]
    fn scale_option() {
        assert_eq!(run(&["--scale", "1.5"]).unwrap().scale, Scale::Fixed(1.5));
        assert_eq!(run(&["--scale=auto"]).unwrap().scale, Scale::Auto);
    }

    #[test]
    fn graphics_compat_option() {
        assert_eq!(run(&["--graphics-compat", "wezterm"]).unwrap().compat, Some(Compat::WezTerm));
        assert_eq!(run(&["--graphics-compat=kitty"]).unwrap().compat, Some(Compat::Kitty));
        assert_eq!(run(&["--graphics-compat", "auto"]).unwrap().compat, None);
    }

    #[test]
    fn large_images_option() {
        assert_eq!(run(&["--large-images", "png"]).unwrap().large_png, Some(true));
        assert_eq!(run(&["--large-images=zlib"]).unwrap().large_png, Some(false));
        assert_eq!(run(&["--large-images", "auto"]).unwrap().large_png, None);
    }

    #[test]
    fn auto_scale_follows_cell_height() {
        for (cell_h, want) in [(13, 1.0), (21, 1.0), (26, 1.25), (32, 1.5), (42, 2.0), (63, 3.0), (200, 4.0)] {
            assert_eq!(Scale::Auto.resolve(cell_h), want, "cell height {cell_h}");
        }
        assert_eq!(Scale::Fixed(1.75).resolve(21), 1.75);
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(["--help".to_string()]).unwrap(), Action::Help);
        assert_eq!(parse(["-V".to_string()]).unwrap(), Action::Version);
    }
}
