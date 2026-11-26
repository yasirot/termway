#!/usr/bin/env bash
# End-to-end check against a real headless sway + foot: the screen that fake-term
# reconstructs from termway's output must equal the frame termway captured.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --quiet
BIN=target/release
OUT=${OUT:-target/e2e}
mkdir -p "$OUT"
fail=0

check() {
  local name=$1; shift
  rm -f "$OUT/$name".*
  if ! "$BIN/fake-term" --decode --snapshot "$OUT/$name.sim.ppm" --report "$OUT/$name.json" "$@" >/dev/null 2>"$OUT/$name.err"; then
    echo "$name: FAILED ($(tail -1 "$OUT/$name.err"))"; fail=1; return
  fi
  if [[ ! -f $OUT/$name.sim.ppm || ! -f $OUT/$name.frame.ppm ]]; then
    echo "$name: FAILED (no snapshot or frame dump)"; fail=1; return
  fi
  if "$BIN/fake-term" --compare "$OUT/$name.frame.ppm" "$OUT/$name.sim.ppm" >"$OUT/$name.cmp"; then
    echo "$name: OK ($(cat "$OUT/$name.cmp"))"
  else
    echo "$name: MISMATCH ($(cat "$OUT/$name.cmp"))"; fail=1
  fi
}

SCRIPT=(--send-at '1.5:printf "\e[42m green \e[0m\\n"; ls -R /usr | head -400\r' --send-at '4:echo done\r')
for phase in 1 2 3 4; do
  for tile in 1x1 4x2; do
    name="phase${phase}_tile${tile}"
    check "$name" --duration 7 --warmup 1 "${SCRIPT[@]}" -- \
      "$BIN/termway" --phase "$phase" --tile "$tile" --log "$OUT/$name.log" --dump-frame "$OUT/$name.frame.ppm"
  done
done
check resize --duration 7 --warmup 1 "${SCRIPT[@]}" --resize-at 3:200x40 -- \
  "$BIN/termway" --log "$OUT/resize.log" --dump-frame "$OUT/resize.frame.ppm"
check status_line --duration 5 --warmup 1 "${SCRIPT[@]}" -- \
  "$BIN/termway" --status-line --log "$OUT/status_line.log" --dump-frame "$OUT/status_line.frame.ppm"
# HiDPI terminals (200% and 150% cells): --scale auto must scale the output.
check hidpi --cols 96 --rows 24 --cell 20x42 --duration 7 --warmup 1 "${SCRIPT[@]}" -- \
  "$BIN/termway" --log "$OUT/hidpi.log" --dump-frame "$OUT/hidpi.frame.ppm"
check hidpi_resize --cols 121 --rows 31 --cell 14x32 --duration 7 --warmup 1 "${SCRIPT[@]}" --resize-at 3:101x27 -- \
  "$BIN/termway" --log "$OUT/hidpi_resize.log" --dump-frame "$OUT/hidpi_resize.frame.ppm"
expect_log() {
  local name=$1 pattern=$2
  grep -q -- "$pattern" "$OUT/$name.log" || { echo "$name: FAILED (log lacks \"$pattern\")"; fail=1; }
}
reject_log() {
  local name=$1 pattern=$2
  ! grep -q -- "$pattern" "$OUT/$name.log" || { echo "$name: FAILED (log has \"$pattern\")"; fail=1; }
}
# WezTerm on a 200% display: pointer motion makes sway draw (and later hide) its cursor.
MOUSE=(--send-at '2:\e[<35;100;100M\e[<35;400;300M\e[<35;900;600M' --send-at '4.5:\e[<35;1200;700M')
for phase in 1 2 3 4; do
  name="wezterm_phase${phase}"
  check "$name" --emulate wezterm --cols 96 --rows 24 --cell 20x42 --duration 7 --warmup 1 "${SCRIPT[@]}" "${MOUSE[@]}" -- \
    "$BIN/termway" --phase "$phase" --log "$OUT/$name.log" --dump-frame "$OUT/$name.frame.ppm"
  expect_log "$name" "graphics compat wezterm (detected)"
done
check wezterm_resize --emulate wezterm --cols 96 --rows 24 --cell 20x42 --duration 7 --warmup 1 "${SCRIPT[@]}" "${MOUSE[@]}" \
  --resize-at 3:80x20 -- "$BIN/termway" --log "$OUT/wezterm_resize.log" --dump-frame "$OUT/wezterm_resize.frame.ppm"
# The kitty model on WezTerm is what left pointer trails; the terminal model must catch it.
if "$BIN/fake-term" --decode --emulate wezterm --cols 96 --rows 24 --cell 20x42 --duration 5 --warmup 1 "${SCRIPT[@]}" "${MOUSE[@]}" -- \
  "$BIN/termway" --graphics-compat kitty --log "$OUT/wezterm_as_kitty.log" >/dev/null 2>"$OUT/wezterm_as_kitty.err"; then
  echo "wezterm_as_kitty: FAILED (kitty-style output was accepted by the WezTerm model)"; fail=1
elif grep -q "WezTerm" "$OUT/wezterm_as_kitty.err"; then
  echo "wezterm_as_kitty: OK (rejected: $(grep -m1 WezTerm "$OUT/wezterm_as_kitty.err"))"
else
  echo "wezterm_as_kitty: FAILED ($(tail -1 "$OUT/wezterm_as_kitty.err"))"; fail=1
fi
# Ghostty 1.3 can crash inflating large zlib images, so they go as PNG; the Ghostty model
# (fake-term's default) rejects large zlib ones.
expect_log hidpi "images over 32 KiB sent as PNG (detected)"
if "$BIN/fake-term" --decode --cols 96 --rows 24 --cell 20x42 --duration 5 --warmup 1 "${SCRIPT[@]}" -- \
  "$BIN/termway" --large-images zlib --log "$OUT/ghostty_zlib.log" >/dev/null 2>"$OUT/ghostty_zlib.err"; then
  echo "ghostty_zlib: FAILED (large zlib images were accepted by the Ghostty model)"; fail=1
elif grep -q "Ghostty 1.3" "$OUT/ghostty_zlib.err"; then
  echo "ghostty_zlib: OK (rejected: $(grep -m1 "Ghostty 1.3" "$OUT/ghostty_zlib.err"))"
else
  echo "ghostty_zlib: FAILED ($(tail -1 "$OUT/ghostty_zlib.err"))"; fail=1
fi
# Page-like scrolling on a HiDPI terminal: scroll copy must move the pixels (and stay exact).
PAGE=(--send-at '1:less /usr/share/common-licenses/GPL-3\r' --type '30:j')
for emu in kitty wezterm; do
  name="pagescroll_${emu}"
  check "$name" --emulate "$emu" --cols 96 --rows 24 --cell 20x42 --duration 5 --warmup 2 "${PAGE[@]}" -- \
    "$BIN/termway" --stats "$OUT/$name.stats" --log "$OUT/$name.log" --dump-frame "$OUT/$name.frame.ppm"
  grep -q '"scroll_shifts":[1-9]' "$OUT/$name.stats" || { echo "$name: FAILED (no scroll-copy shifts)"; fail=1; }
done
# Scrolling text with the pointer resting over blank space (moved now and then so sway
# keeps drawing it): its tile stays on top of the region while the pieces move under it.
# WezTerm duplicated such overlays on every shift until it hung and crashed; its model
# rejects any placement that would do that.
FEED=(--send-at '1:for i in $(seq 100); do echo "line $i of slowly scrolling output"; sleep 0.03; done\r')
for i in $(seq 0 9); do
  FEED+=(--send-at "$(awk -v i="$i" 'BEGIN { printf "%.1f", 1.8 + i * 0.4 }'):\e[<35;$((1750 + i % 2));500M")
done
for emu in kitty wezterm; do
  name="feedscroll_${emu}"
  check "$name" --emulate "$emu" --cols 96 --rows 24 --cell 20x42 --duration 8 --warmup 1 "${FEED[@]}" -- \
    "$BIN/termway" --stats "$OUT/$name.stats" --log "$OUT/$name.log" --dump-frame "$OUT/$name.frame.ppm"
  grep -q '"scroll_shifts":[1-9]' "$OUT/$name.stats" || { echo "$name: FAILED (no scroll-copy shifts)"; fail=1; }
done
# A terminal that does not say what it is (WezTerm behind ssh.exe, say) gets no scroll copy.
check silent_terminal --no-version-reply --cols 96 --rows 24 --cell 20x42 --duration 5 --warmup 2 "${PAGE[@]}" -- \
  "$BIN/termway" --stats "$OUT/silent_terminal.stats" --log "$OUT/silent_terminal.log" --dump-frame "$OUT/silent_terminal.frame.ppm"
expect_log silent_terminal "did not answer XTVERSION: scroll copy disabled"
grep -q '"scroll_shifts":[1-9]' "$OUT/silent_terminal.stats" && { echo "silent_terminal: FAILED (scroll copy used)"; fail=1; }
# A nested sway must start on the Wayland backend (not headless) and must not outlive termway.
printf 'xwayland disable\nexec foot\n' >"$OUT/inner_sway.conf"
check nested_sway --duration 7 --warmup 1 "${SCRIPT[@]}" -- \
  "$BIN/termway" --scale 1 --log "$OUT/nested_sway.log" --dump-frame "$OUT/nested_sway.frame.ppm" -- sway -c "$OUT/inner_sway.conf"
sleep 0.5
if pgrep -f "$OUT/inner_sway.conf" >/dev/null; then
  echo "nested_sway: FAILED (processes left after exit)"; pkill -f "$OUT/inner_sway.conf"; fail=1
fi
# A sway desktop from the user's config, without nesting a second sway: the outer gap
# shows the configured background, and termway starts no app of its own.
printf 'output * bg #285577 solid_color\ngaps outer 30\nexec foot\n' >"$OUT/desk.conf"
check sway_config --duration 7 --warmup 1 "${SCRIPT[@]}" -- \
  "$BIN/termway" --sway-config "$OUT/desk.conf" --log "$OUT/sway_config.log" --dump-frame "$OUT/sway_config.frame.ppm"
ppm_pixel() {
  local file=$1 x=$2 y=$3 width header
  width=$(sed -n 2p "$file" | cut -d' ' -f1)
  header=$(head -n 3 "$file" | wc -c)
  od -An -tx1 -j $((header + (y * width + x) * 3)) -N3 "$file" | xargs
}
if [[ -f $OUT/sway_config.frame.ppm ]]; then
  pixel=$(ppm_pixel "$OUT/sway_config.frame.ppm" 5 5)
  [[ $pixel == "28 55 77" ]] || { echo "sway_config: FAILED (pixel 5,5 is $pixel, not the configured background)"; fail=1; }
fi
# An X11 window manager (i3) on rootful Xwayland. The X screen is sized in output
# pixels on a HiDPI terminal and follows resizes; X clients without the session's
# cookie are refused. Needs i3, xterm and Xwayland in the container.
X11=$OUT/x11
rm -rf "$X11"
mkdir -p "$X11"
cat >"$OUT/i3.conf" <<I3CONF
font pango:monospace 8
bar {
    status_command while date +%H:%M:%S; do sleep 1; done
}
bindsym Mod1+Return exec sh -c 'echo alt-return >>$X11/keys'
bindsym Mod1+Shift+Return exec sh -c 'echo alt-shift-return >>$X11/keys'
exec xterm
exec sh -c 'sleep 2; i3-msg -t get_outputs >$X11/outputs1; XAUTHORITY=/dev/null xterm -e true 2>/dev/null; echo "noauth=\$?" >$X11/auth; xterm -e true; echo "auth=\$?" >>$X11/auth'
exec sh -c 'sleep 5; i3-msg -t get_outputs >$X11/outputs2'
I3CONF
# Alt+Enter as WezTerm sends it, then Alt+Shift+Enter as Ghostty does (modifyOtherKeys form).
check x11_i3 --cols 96 --rows 24 --cell 20x42 --duration 8 --warmup 1 --send-at '2.4:\e\r' --send-at '2.8:\e[27;4;13~' \
  --send-at '3:echo hello\r' --resize-at 3.5:80x20 -- \
  "$BIN/termway" --x11 --log "$OUT/x11_i3.log" --dump-frame "$OUT/x11_i3.frame.ppm" -- i3 -c "$OUT/i3.conf"
grep -q '"width":1920,"height":1008' "$X11/outputs1" 2>/dev/null ||
  { echo "x11_i3: FAILED (X screen is not 1920x1008 output pixels: $(cat "$X11/outputs1" 2>/dev/null))"; fail=1; }
grep -q '"width":1600,"height":840' "$X11/outputs2" 2>/dev/null ||
  { echo "x11_i3: FAILED (X screen did not follow the resize: $(cat "$X11/outputs2" 2>/dev/null))"; fail=1; }
[[ $(tr '\n' ' ' <"$X11/auth" 2>/dev/null) == "noauth=1 auth=0 " ]] ||
  { echo "x11_i3: FAILED (cookie check: $(cat "$X11/auth" 2>/dev/null))"; fail=1; }
[[ $(tr '\n' ' ' <"$X11/keys" 2>/dev/null) == "alt-return alt-shift-return " ]] ||
  { echo "x11_i3: FAILED (i3 key bindings: $(cat "$X11/keys" 2>/dev/null))"; fail=1; }
expect_log hidpi "at scale 2$"
expect_log hidpi_resize "at scale 1.5$"
expect_log hidpi_resize "resize to 101x27 cells of 14x32 px, scale 1.5"
# A mode change must not make the outstanding capture fail.
reject_log resize "screencopy failed"
reject_log hidpi_resize "screencopy failed"
# Exit paths: fake-term fails unless every image is deleted, the alternate screen is
# left and the tty attributes are restored.
check_exit() {
  local name=$1; shift
  rm -f "$OUT/$name".*
  if "$BIN/fake-term" --decode --report "$OUT/$name.json" "$@" >/dev/null 2>"$OUT/$name.err"; then
    echo "$name: OK (clean exit)"
  else
    echo "$name: FAILED ($(tail -1 "$OUT/$name.err"))"; fail=1
  fi
}
for sig in INT TERM HUP; do
  check_exit "signal_$sig" --duration 30 --send-at '1:ls\r' --signal-at "2.5:$sig" -- "$BIN/termway" --log "$OUT/signal_$sig.log"
done
check_exit app_exit --duration 30 --send-at '1.5:exit\r' -- "$BIN/termway" --log "$OUT/app_exit.log"
expect_log app_exit "foot exited with status 0"
reject_log app_exit "WARN"
reject_log app_exit "Broken pipe"
# Apps that end without a window (a single-instance app handing off, or a failure) are explained.
check_exit no_window --duration 30 -- "$BIN/termway" --log "$OUT/no_window.log" -- sh -c 'sleep 1'
expect_log no_window "sh exited without opening a window"
check_exit app_failed --duration 30 -- "$BIN/termway" --log "$OUT/app_failed.log" -- sh -c 'echo boom >&2; exit 3'
expect_log app_failed "sh exited with status 3"
expect_log app_failed "sh failed without opening a window"
expect_log app_failed "^boom$"
reject_log app_failed "clipped/padded"
# Without a command the session is sway's: it ends when sway exits. Config errors (here a
# trailing comment, which sway rejects) are reported.
printf 'output * bg #285577 solid_color   # trailing comment\nexec foot\n' >"$OUT/desk_error.conf"
check_exit sway_config_exit --duration 30 --send-at '1.5:swaymsg exit\r' -- \
  "$BIN/termway" --sway-config "$OUT/desk_error.conf" --log "$OUT/sway_config_exit.log"
expect_log sway_config_exit "sway config: Error on line 1 "
expect_log sway_config_exit "sway exited (exit status: 0)"
reject_log sway_config "sway config:"
reject_log sway_config "no window has opened"
# A config that starts no app leaves the screen black; that must not go unexplained.
printf 'gaps outer 8\n' >"$OUT/desk_empty.conf"
check_exit sway_config_empty --duration 7 -- \
  "$BIN/termway" --sway-config "$OUT/desk_empty.conf" --log "$OUT/sway_config_empty.log"
expect_log sway_config_empty "no window has opened"
# The X11 session ends with the window manager, and Xwayland goes with it.
printf 'exec xterm\n' >"$OUT/i3_exit.conf"
check_exit x11_exit --duration 30 --send-at '2.5:i3-msg exit\r' -- "$BIN/termway" --x11 --log "$OUT/x11_exit.log" -- i3 -c "$OUT/i3_exit.conf"
expect_log x11_exit "i3 exited with status 0"
reject_log x11_exit "WARN"
sleep 0.5
# Other termway sessions may run their own Xwayland; a leftover is one whose runtime dir is gone.
for pid in $(pgrep -f '^Xwayland -hidpi '); do
  auth=$(tr '\0' '\n' <"/proc/$pid/cmdline" | grep -A1 '^-auth$' | tail -1)
  [[ -e $auth ]] || { echo "x11_exit: FAILED (Xwayland $pid outlived its session)"; fail=1; }
done
check_exit quit_key --duration 3 -- "$BIN/termway" --log "$OUT/quit_key.log"
check_exit quit_ctrl_bracket --duration 3 --quit-key '\x1d' -- "$BIN/termway" --quit-key 'ctrl-]' --log "$OUT/quit_ctrl.log"
exit $fail
