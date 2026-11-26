#!/usr/bin/env bash
# Measures every phase in four scenarios on a fake 282x60 terminal with 9x21 cells
# (the assumed Ghostty setup) and writes a markdown table.
#   scripts/bench.sh [extra termway args...]
# Environment: OUT (output dir), DURATION, WARMUP, THROTTLE_KBPS, PHASES, SCENARIOS,
#   APP (command run in the compositor, default "foot"), COLS/ROWS/CELL (terminal
#   geometry, default 282/60/9x21; e.g. 190/45/20x42 for a maximised 200% WezTerm)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --quiet
BIN=target/release
OUT=${OUT:-target/bench}
DURATION=${DURATION:-12}
WARMUP=${WARMUP:-2}
PHASES=${PHASES:-"1 2 3 4"}
SCENARIOS=${SCENARIOS:-"static typing scroll pagescroll"}
mkdir -p "$OUT"
read -r -a app <<<"${APP:-foot}"
geometry=(--cols "${COLS:-282}" --rows "${ROWS:-60}" --cell "${CELL:-9x21}")
throttle=()
[[ -n "${THROTTLE_KBPS:-}" ]] && throttle=(--throttle-kbps "$THROTTLE_KBPS")

for scenario in $SCENARIOS; do
  case $scenario in
    static) input=() ;;
    typing) input=(--type '10:the quick brown fox jumps over the lazy dog ') ;;
    scroll) input=(--send-at '1:while :; do ls -R /usr; done\r') ;;
    # Page-like scrolling: one line at a time, 30 times a second.
    pagescroll) input=(--send-at '1:less /usr/share/common-licenses/GPL-3\r' --type '30:j') ;;
    *) echo "unknown scenario $scenario" >&2; exit 2 ;;
  esac
  for phase in $PHASES; do
    name="p${phase}_${scenario}"
    rm -f "$OUT/$name.stats.jsonl"
    "$BIN/fake-term" --duration "$DURATION" --warmup "$WARMUP" "${geometry[@]}" "${throttle[@]}" "${input[@]}" \
      --label "$name" --stats "$OUT/$name.stats.jsonl" --report "$OUT/$name.json" -- \
      "$BIN/termway" --phase "$phase" --stats "$OUT/$name.stats.jsonl" --log "$OUT/$name.log" "$@" -- "${app[@]}" \
      >/dev/null 2>"$OUT/$name.err" || echo "$name: fake-term exited with $?" >&2
    echo "done $name" >&2
  done
done
python3 scripts/summarize.py "$OUT"/p*_*.json | tee "$OUT/summary.md"
