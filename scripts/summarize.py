#!/usr/bin/env python3
"""Turns fake-term JSON reports into a markdown table (stdlib only).

usage: summarize.py REPORT.json...
"""
import json
import sys

SCEN = {"static": "(a) 静止", "typing": "(b) タイピング", "scroll": "(c) スクロール", "pagescroll": "(d) ページスクロール"}
# Term.Everything at native size: 2538x1260 RGBA, base64, 15 fps (assumed).
TE_NATIVE = 2538 * 1260 * 4 * 4 / 3 * 15

rows = []
for path in sys.argv[1:]:
    with open(path) as f:
        r = json.load(f)
    phase, scen = r["label"].split("_", 1)
    rows.append((scen, phase, r, r.get("termway") or {}))
order = {k: i for i, k in enumerate(SCEN)}
rows.sort(key=lambda x: (order.get(x[0], 9), x[1]))


def fmt_bytes(b):
    if b >= 1 << 20:
        return f"{b / (1 << 20):.2f} MiB/s"
    if b >= 1 << 10:
        return f"{b / (1 << 10):.1f} KiB/s"
    return f"{b:.0f} B/s"


def num(v, f="{:.1f}"):
    return "-" if v is None else f.format(v)


def ratio(b):
    return "∞（無送信）" if b < 1 else f"1/{TE_NATIVE / b:,.0f}"


print(
    "| シナリオ | Phase | 出力バイト/秒 | Term.Everything 等倍比 | 実効fps | 更新タイル/秒 "
    "| 送信画像/秒 | うち新規タイル | キャッシュ命中/秒 | ヒット率 | termway CPU | sway CPU |"
)
print("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
for scen, phase, r, t in rows:
    hit = t.get("hit_rate")
    tile_phase = phase in ("p3", "p4")
    print(
        f"| {SCEN.get(scen, scen)} | {phase[1:]} | {fmt_bytes(r['bytes_per_sec'])} | {ratio(r['bytes_per_sec'])} "
        f"| {num(t.get('fps'))} | {num(t.get('changed_tiles'), '{:.0f}')} | {num(t.get('images_sent'), '{:.0f}')} "
        f"| {num(t.get('tiles_sent'), '{:.0f}') if tile_phase else '-'} "
        f"| {num(t.get('cache_hits'), '{:.0f}') if tile_phase else '-'} "
        f"| {'-' if hit is None or not tile_phase else f'{hit * 100:.1f}%'} "
        f"| {num(t.get('cpu_pct'))}% | {num(t.get('compositor_cpu_pct'))}% |"
    )
