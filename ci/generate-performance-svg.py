#!/usr/bin/env python3
"""Generate the README benchmark charts in assets/.

Each chart writes <prefix>-{light,dark}.svg for English and
<prefix>-<lang>-{light,dark}.svg for the other languages. After a new kitten
run, update that chart in CHARTS, run this script, and keep the README tables
in step.
"""

import sys
from html import escape
from pathlib import Path

CHARTS = [
    dict(
        prefix="performance",
        rival="WezTerm",
        legend=("ThinkTerm 0.4.3 · 2026-11-06", "WezTerm 20260117 · 2026-09-23"),
        machine="Mac · M1 Pro",
        caption="mac",
        scale_max=300,
        # workload: (ThinkTerm MB/s, rival MB/s)
        results={
            "ascii": (221.4, 23.6),
            "unicode": (193.2, 46.7),
            "multi": (285.3, 49.3),
            "csi": (89.9, 15.6),
            "long": (278.6, 194.9),
            "images": (278.4, 270.5),
        },
    ),
    dict(
        prefix="performance-linux",
        rival="kitty",
        legend=("ThinkTerm 0.4.2 · 2026-11-05", "kitty 0.47.4 · 2026-11-05"),
        machine="Ubuntu 20.04 · i9-12900K",
        caption="linux",
        scale_max=720,
        results={
            "ascii": (224.1, 80.0),
            "unicode": (186.8, 79.0),
            "csi": (71.1, 43.2),
            "long": (624.6, 241.2),
            "images": (663.3, 196.3),
        },
    ),
    dict(
        prefix="performance-progress",
        legend=(
            "early · 2026-09",
            "0.3.0 · 2026-10-21",
            "0.4.1 · 2026-11-04",
            "0.4.2 · 2026-11-05",
            "0.4.3 · 2026-11-06",
        ),
        machine="Mac · M1 Pro",
        caption="progress",
        scale_max=300,
        # workload: one MB/s per entry in legend, oldest first
        results={
            "ascii": (45.3, 133.0, 134.0, 206.1, 221.4),
            "unicode": (52.2, 104.5, 109.2, 192.4, 193.2),
            "multi": (67.8, 197.5, 234.4, 200.9, 285.3),
            "csi": (25.7, 49.5, 53.5, 90.9, 89.9),
            "long": (219.4, 213.8, 257.3, 219.3, 278.6),
            "images": (218.0, 257.2, 271.0, 216.4, 278.4),
        },
    ),
    dict(
        prefix="performance-linux-progress",
        legend=(
            "143b60a2 · 2026-10-02",
            "0.4.1 · 2026-11-05",
            "0.4.2 · 2026-11-05",
        ),
        machine="Ubuntu 20.04 · i9-12900K",
        caption="progress",
        scale_max=720,
        results={
            "ascii": (147.4, 156.2, 224.1),
            "unicode": (85.6, 96.8, 186.8),
            "csi": (50.0, 52.2, 71.1),
            "long": (533.5, 649.9, 624.6),
            "images": (391.8, 346.0, 663.3),
        },
    ),
]

LATIN_FONTS = "-apple-system, BlinkMacSystemFont, 'SF Pro Display', 'SF Pro Text', 'Segoe UI', 'Helvetica Neue', Helvetica, Arial"

LANGS = {
    "en": dict(
        progress_title="ThinkTerm across versions",
        fonts=LATIN_FONTS,
        decimal=".",
        subhead="kitten __benchmark__ · {machine} · MB/s, higher is better",
        ratio="Ratio",
        labels=dict(
            ascii="Only ASCII chars",
            unicode="Unicode chars",
            multi="Unique multi-codepoint Unicode cells",
            csi="CSI codes with few chars",
            long="Long escape codes",
            images="Images",
        ),
        captions=dict(
            mac="ThinkTerm measured through its local mux: the shell runs in the mux server and output passes through it before reaching the window.",
            linux="ThinkTerm measured through its local mux · 180×50 · Xvfb with CPU software rendering",
        ),
    ),
    "zh-CN": dict(
        progress_title="ThinkTerm 版本进步",
        fonts=LATIN_FONTS + ", 'PingFang SC', 'Hiragino Sans GB', 'Microsoft YaHei', 'Noto Sans CJK SC'",
        decimal=".",
        subhead="kitten __benchmark__ · {machine} · MB/s，越大越好",
        ratio="倍数",
        labels=dict(
            ascii="纯 ASCII 字符",
            unicode="Unicode 字符",
            multi="多码点 Unicode 字符",
            csi="CSI 序列夹少量字符",
            long="长转义序列",
            images="图片",
        ),
        captions=dict(
            mac="ThinkTerm 测试时开着本地 mux：shell 跑在 mux 服务端里，输出要先经过 mux 再到窗口。",
            linux="ThinkTerm 走本地 mux · 180×50 · Xvfb 虚拟显示，CPU 软件渲染",
        ),
    ),
    "ja-JP": dict(
        progress_title="バージョンごとの ThinkTerm",
        fonts=LATIN_FONTS + ", 'Hiragino Sans', 'Hiragino Kaku Gothic ProN', 'Yu Gothic', 'Meiryo', 'Noto Sans CJK JP'",
        decimal=".",
        subhead="kitten __benchmark__ · {machine} · MB/s、大きいほど高速",
        ratio="倍率",
        labels=dict(
            ascii="ASCII 文字のみ",
            unicode="Unicode 文字",
            multi="複数コードポイントの Unicode セル",
            csi="文字の少ない CSI シーケンス",
            long="長いエスケープシーケンス",
            images="画像",
        ),
        captions=dict(
            mac="ThinkTerm はローカル mux を有効にして計測：シェルは mux サーバーで動き、出力は mux を経由してウィンドウに届きます。",
            linux="ThinkTerm はローカル mux 経由 · 180×50 · Xvfb、CPU ソフトウェアレンダリング",
        ),
    ),
    "de-DE": dict(
        progress_title="ThinkTerm über die Versionen",
        fonts=LATIN_FONTS,
        decimal=",",
        subhead="kitten __benchmark__ · {machine} · MB/s, höher ist besser",
        ratio="Faktor",
        labels=dict(
            ascii="Nur ASCII-Zeichen",
            unicode="Unicode-Zeichen",
            multi="Unicode-Zellen aus mehreren Codepoints",
            csi="CSI-Sequenzen mit wenigen Zeichen",
            long="Lange Escape-Sequenzen",
            images="Bilder",
        ),
        captions=dict(
            mac="ThinkTerm mit lokalem mux gemessen: Die Shell läuft im mux-Server, und ihre Ausgabe geht über den mux ins Fenster.",
            linux="ThinkTerm über lokalen mux gemessen · 180×50 · Xvfb mit CPU-Software-Rendering",
        ),
    ),
    "fr-FR": dict(
        progress_title="ThinkTerm au fil des versions",
        fonts=LATIN_FONTS,
        decimal=",",
        subhead="kitten __benchmark__ · {machine} · Mo/s, plus c’est élevé, mieux c’est",
        ratio="Rapport",
        labels=dict(
            ascii="Caractères ASCII uniquement",
            unicode="Caractères Unicode",
            multi="Cellules Unicode à plusieurs points de code",
            csi="Séquences CSI avec peu de caractères",
            long="Longues séquences d’échappement",
            images="Images",
        ),
        captions=dict(
            mac="ThinkTerm mesuré avec son mux local : le shell tourne dans le serveur mux, et sa sortie passe par le mux avant la fenêtre.",
            linux="ThinkTerm mesuré via son mux local · 180×50 · Xvfb avec rendu logiciel sur CPU",
        ),
    ),
}

THEMES = {
    "light": dict(
        card="#ffffff", card_edge="#d2d2d7", ink="#1d1d1f", sub="#6e6e73", faint="#86868b",
        grid="#ececf0", axis="#d2d2d7", ours_a="#2b8cff", ours_b="#0071e3", theirs="#c7c7cc", mids=("#c9e0fb", "#8fbff6"),
        win_fill="#e8f1ff", win_edge="#c4d7f6", win_ink="#0071e3",
        even_fill="#f5f5f7", even_edge="#e3e3e8", even_ink="#6e6e73", glow=0.28,
    ),
    "dark": dict(
        card="#000000", card_edge="#2c2c2e", ink="#f5f5f7", sub="#a1a1a6", faint="#8e8e93",
        grid="#1c1c1e", axis="#3a3a3c", ours_a="#5fb0ff", ours_b="#2f8cff", theirs="#48484a", mids=("#1d3f6b", "#2f6bb3"),
        win_fill="#0b1f3d", win_edge="#1f4a85", win_ink="#4da3ff",
        even_fill="#1c1c1e", even_edge="#3a3a3c", even_ink="#a1a1a6", glow=0.5,
    ),
}

W = 1000
MIN_X0 = 372      # bar origin, pushed right when a label is too wide
BAR_END = 842     # where the scale maximum lands
TOP = 178
ROW_H = 66
BAR_H = 16
GAP = 5
PILL_CX = 912
LEGEND_X = 372
LEGEND_END = 880  # legend lines stop short of the ratio heading
LEGEND_LINE = 24


def label_width(s):
    """Rough width of a 15px semibold label; CJK glyphs are full width."""
    return sum(15.0 if ord(ch) >= 0x2E80 else 7.6 for ch in s)


def blend(a, b, t):
    """The colour a fraction t of the way from a to b, both #rrggbb."""
    pairs = ((int(a[i:i + 2], 16), int(b[i:i + 2], 16)) for i in (1, 3, 5))
    return "#" + "".join(f"{round(x + (y - x) * t):02x}" for x, y in pairs)


def build(chart, lang, t):
    L = LANGS[lang]
    c = THEMES[t]
    # Newest first in a comparison, oldest first along a version history.
    progress = chart["caption"] == "progress"
    versions = len(chart["legend"])
    row_h = BAR_H * versions + GAP * (versions - 1) + 27 if progress else ROW_H

    # Legend entries fill a line and wrap, so the bars start below the last one.
    legend = []
    legend_x0 = 72 if progress else LEGEND_X
    lx, line = legend_x0, 0
    for name in chart["legend"]:
        width = 30 + round(len(name) * 7.0)
        if lx + width > LEGEND_END and lx > legend_x0:
            lx, line = legend_x0, line + 1
        legend.append((name, lx, line))
        lx += width + 30
    top = TOP + LEGEND_LINE * line

    rows = [(L["labels"][k], v) for k, v in chart["results"].items()]
    scale_max = chart["scale_max"]
    x0 = max(MIN_X0, round(max(label_width(n) for n, _ in rows)) + 70)
    scale_w = BAR_END - x0
    subhead = L["subhead"].format(machine=chart["machine"])
    title = L["progress_title"] if progress else f'ThinkTerm vs {chart["rival"]}'

    def x_of(v):
        return x0 + v / scale_max * scale_w

    def num(v, digits=1):
        return f"{v:.{digits}f}".replace(".", L["decimal"])

    def ratio(a, b):
        r = a / b
        return num(r, 2 if r < 1 else 1) + "×"

    def lead(values):
        """The two values the ratio compares: newer, then older."""
        return (values[-1], values[0]) if progress else values

    h = top + row_h * len(rows) + (56 if progress else 96)
    o = []
    o.append(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {h}" width="{W}" height="{h}" role="img" aria-labelledby="t d">')
    o.append(f'<title id="t">{title}</title>')
    o.append(f'<desc id="d">{escape(subhead)}. '
             + escape("; ".join(
                 f'{n}: ' + ", ".join(f"{name.split(' · ')[0]} {num(v)}" for name, v in zip(chart["legend"], vs))
                 + f' ({ratio(*lead(vs))})' if progress
                 else f'{n}: ThinkTerm {num(vs[0])}, {chart["rival"]} {num(vs[1])} ({ratio(*vs)})'
                 for n, vs in rows))
             + '.</desc>')
    o.append('<defs>')
    o.append(f'<linearGradient id="ours" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{c["ours_b"]}"/><stop offset="1" stop-color="{c["ours_a"]}"/></linearGradient>')
    o.append(f'<filter id="glow" x="-10%" y="-80%" width="120%" height="300%"><feDropShadow dx="0" dy="3" stdDeviation="4" flood-color="{c["ours_b"]}" flood-opacity="{c["glow"]}"/></filter>')
    o.append('</defs>')
    o.append('<style>')
    o.append(f"text {{ font-family: {L['fonts']}, sans-serif; fill: {c['ink']}; }}")
    o.append('.headline { font-size: 32px; font-weight: 700; letter-spacing: -0.6px; }')
    o.append(f'.subhead {{ font-size: 16px; fill: {c["sub"]}; }}')
    o.append('.label { font-size: 15px; font-weight: 600; letter-spacing: -0.1px; }')
    o.append(f'.legend {{ font-size: 13.5px; font-weight: 500; fill: {c["sub"]}; }}')
    o.append('.val-ours { font-size: 13px; font-weight: 700; }')
    o.append(f'.val-theirs {{ font-size: 12.5px; font-weight: 500; fill: {c["sub"]}; }}')
    o.append(f'.tick {{ font-size: 11.5px; fill: {c["faint"]}; }}')
    o.append(f'.caption {{ font-size: 13px; font-weight: 500; fill: {c["faint"]}; }}')
    o.append(f'.grid {{ stroke: {c["grid"]}; stroke-width: 1; }}')
    o.append(f'.axis {{ stroke: {c["axis"]}; stroke-width: 1.2; }}')
    o.append('.ours { fill: url(#ours); }')
    o.append('.ours-bar { fill: url(#ours); filter: url(#glow); }')
    o.append(f'.theirs {{ fill: {c["theirs"]}; }}')
    # Between the oldest and the newest, each version a step nearer the newest.
    mids = versions - 2 if progress else 0
    for i in range(mids):
        step = i / (mids - 1) if mids > 1 else 1
        o.append(f'.mid{i} {{ fill: {blend(*c["mids"], step)}; }}')
    o.append(f'.win {{ fill: {c["win_fill"]}; stroke: {c["win_edge"]}; }}')
    o.append(f'.win-t {{ font-size: 14px; font-weight: 700; fill: {c["win_ink"]}; }}')
    o.append(f'.even {{ fill: {c["even_fill"]}; stroke: {c["even_edge"]}; }}')
    o.append(f'.even-t {{ font-size: 14px; font-weight: 700; fill: {c["even_ink"]}; }}')
    o.append('</style>')
    o.append(f'<rect x="0.5" y="0.5" width="{W - 1}" height="{h - 1}" rx="28" fill="{c["card"]}" stroke="{c["card_edge"]}"/>')
    o.append(f'<text x="500" y="62" class="headline" text-anchor="middle">{title}</text>')
    o.append(f'<text x="500" y="92" class="subhead" text-anchor="middle">{escape(subhead)}</text>')

    # legend
    ly = 132
    if progress:
        bars = ("theirs", *(f"mid{i}" for i in range(mids)), "ours")
    else:
        bars = ("ours", "theirs")
    for (name, lx, line), cls in zip(legend, bars):
        y = ly + LEGEND_LINE * line
        o.append(f'<rect x="{lx}" y="{y - 10}" width="22" height="12" rx="6" class="{cls}"/>')
        o.append(f'<text x="{lx + 30}" y="{y}" class="legend">{name}</text>')
    o.append(f'<text x="{PILL_CX}" y="{ly}" class="legend" text-anchor="middle">{escape(L["ratio"])}</text>')

    # grid, six steps whatever the scale
    gy0 = top - 14
    gy1 = top + row_h * len(rows) - 10
    step = scale_max // 6
    for v in range(step, scale_max + 1, step):
        x = x_of(v)
        o.append(f'<line x1="{x:.1f}" y1="{gy0}" x2="{x:.1f}" y2="{gy1}" class="grid"/>')
        o.append(f'<text x="{x:.1f}" y="{gy1 + 20}" class="tick" text-anchor="middle">{v}</text>')
    o.append(f'<line x1="{x0}" y1="{gy0}" x2="{x0}" y2="{gy1}" class="axis"/>')
    o.append(f'<text x="{x0}" y="{gy1 + 20}" class="tick" text-anchor="middle">0</text>')

    r = BAR_H / 2
    for i, (name, values) in enumerate(rows):
        y = top + i * row_h
        mid = y + (BAR_H * len(values) + GAP * (len(values) - 1)) / 2
        o.append(f'<text x="{x0 - 22}" y="{mid + 5:.1f}" class="label" text-anchor="end">{escape(name)}</text>')
        for j, (v, cls) in enumerate(zip(values, bars)):
            yb = y + j * (BAR_H + GAP)
            width = x_of(v) - x0
            bar, val = ("ours-bar", "val-ours") if cls == "ours" else (cls, "val-theirs")
            o.append(f'<rect x="{x0}" y="{yb}" width="{width:.1f}" height="{BAR_H}" rx="{r}" class="{bar}"/>')
            o.append(f'<text x="{x0 + width + 8:.1f}" y="{yb + 12.5}" class="{val}">{num(v)}</text>')
        a, b = lead(values)
        # A lead too small to show in the ratio is not a win.
        cls = "win" if a / b >= 1.05 else "even"
        o.append(f'<rect x="{PILL_CX - 34}" y="{mid - 14:.1f}" width="68" height="28" rx="14" class="{cls}"/>')
        o.append(f'<text x="{PILL_CX}" y="{mid + 5:.1f}" class="{cls}-t" text-anchor="middle">{ratio(a, b)}</text>')

    if not progress:
        o.append(f'<text x="500" y="{h - 34}" class="caption" text-anchor="middle">{escape(L["captions"][chart["caption"]])}</text>')
    o.append('</svg>')
    return "\n".join(o) + "\n"


def main():
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent / "assets"
    for chart in CHARTS:
        for lang in LANGS:
            prefix = chart["prefix"] if lang == "en" else f'{chart["prefix"]}-{lang}'
            for t in THEMES:
                (out / f"{prefix}-{t}.svg").write_text(build(chart, lang, t))


if __name__ == "__main__":
    main()
