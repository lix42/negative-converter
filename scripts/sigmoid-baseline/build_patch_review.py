#!/usr/bin/env python3
"""Build the Phase-1 patch-review page.

Reads the saved `propose_patches` output, emits a self-referencing HTML page that shows
each frame as a rendered positive with the candidate rectangles drawn on it, plus a
magnified crop of each rectangle (pure CSS `background-position`, so one JPEG per frame
serves both views). Every frame carries a stable mark (G1..P4) and its own question
block so a later discussion can name a specific box.
"""
import re
import sys
from pathlib import Path

RAW = Path(sys.argv[1])
OUT = Path(sys.argv[2])

# frame filename -> (mark, roll label)
MARKS = {
    "20260724-leica-1137.tif": ("G1", "Gold 200"),
    "20260724-leica-1144.tif": ("G2", "Gold 200"),
    "20260724-leica-1151.tif": ("G3", "Gold 200"),
    "20260713-nikon-971.tif": ("E1", "Ektar 100"),
    "20260714-nikon-989.tif": ("E2", "Ektar 100"),
    "20260714-nikon-991.tif": ("E3", "Ektar 100"),
    "20260722-nikon-1102.tif": ("P1", "Portra 160"),
    "20260723-nikon-1111.tif": ("P2", "Portra 160"),
    "20260723-nikon-1121.tif": ("P3", "Portra 160"),
    "20260723-nikon-1127.tif": ("P4", "Portra 160"),
}
CATS = [("shadow", "shadow", "#4ea3ff"), ("mid-tone", "mid-tone", "#ffd23f"),
        ("diffuse", "diffuse white", "#ff5d5d")]


def parse(raw: str):
    frames, roll, dmax, cur, cat = [], None, None, None, None
    for line in raw.splitlines():
        m = re.match(r"=== (\S+)\s+Dmin.*Dmax=([\d.]+)", line)
        if m:
            roll, dmax = m.group(1), float(m.group(2))
            continue
        m = re.match(r"  (\S+\.tif)\s+(\d+)x(\d+)\s+interior tile D.p50: min ([\d.]+) / med ([\d.]+) / max ([\d.]+)", line)
        if m:
            cur = {"file": m.group(1), "roll": roll, "dmax": dmax,
                   "w": int(m.group(2)), "h": int(m.group(3)),
                   "tmin": float(m.group(4)), "tmed": float(m.group(5)), "tmax": float(m.group(6)),
                   "boxes": {}}
            frames.append(cur)
            continue
        m = re.match(r"    (shadow|diffuse white|mid-tone)", line)
        if m:
            cat = m.group(1).split()[0]
            continue
        m = re.match(r"\s+(\d+),\s*(\d+),\s*(\d+),\s*(\d+)\s+D.p50\s+([\d.]+)\s+p05\s+([\d.]+)\s+p95\s+([\d.]+)\s+spread\s+([\d.]+)", line)
        if m and cur is not None and cat and cat not in cur["boxes"]:
            cur["boxes"][cat] = {
                "x": int(m.group(1)), "y": int(m.group(2)),
                "bw": int(m.group(3)), "bh": int(m.group(4)),
                "p50": float(m.group(5)), "p05": float(m.group(6)),
                "p95": float(m.group(7)), "spread": float(m.group(8)),
            }
    return frames


def crop_style(f, b, jpg):
    """Magnified crop via background-position — no separate crop files needed."""
    W, H = f["w"], f["h"]
    zx = W / b["bw"] * 100
    px = b["x"] / (W - b["bw"]) * 100 if W > b["bw"] else 0
    py = b["y"] / (H - b["bh"]) * 100 if H > b["bh"] else 0
    # Single quotes inside the CSS url() are load-bearing: this string goes into a
    # double-quoted HTML `style` attribute, and double quotes here would terminate the
    # attribute at `url(` — leaving the crop with no background at all (it renders as a
    # black box, which is exactly how this bug presented).
    return (f"background-image:url('{jpg}');background-size:{zx:.3f}% auto;"
            f"background-position:{px:.4f}% {py:.4f}%;background-repeat:no-repeat")


def main():
    frames = parse(RAW.read_text())
    frames.sort(key=lambda f: MARKS[f["file"]][0])

    rows = "\n".join(
        f'<tr><td><a href="#{MARKS[f["file"]][0]}"><b>{MARKS[f["file"]][0]}</b></a></td>'
        f'<td>{MARKS[f["file"]][1]}</td><td class="mono">{f["file"]}</td>'
        + "".join(
            f'<td class="mono">{f["boxes"][c]["p50"]:.3f} <span class="dim">({100 * f["boxes"][c]["p50"] / f["dmax"]:.0f}%)</span></td>'
            if c in f["boxes"] else "<td>—</td>" for c, _, _ in CATS)
        + f'<td class="mono">{(f["boxes"]["diffuse"]["p50"] - f["boxes"]["mid-tone"]["p50"]):.3f}</td></tr>'
        for f in frames)

    secs = []
    for f in frames:
        mark, roll = MARKS[f["file"]]
        jpg = f"{mark}.jpg"
        boxes = "".join(
            f'<div class="box" style="left:{100 * b["x"] / f["w"]:.3f}%;top:{100 * b["y"] / f["h"]:.3f}%;'
            f'width:{100 * b["bw"] / f["w"]:.3f}%;height:{100 * b["bh"] / f["h"]:.3f}%;'
            f'border-color:{col}" title="{lbl}"><span style="background:{col}">{lbl}</span></div>'
            for c, lbl, col in CATS if (b := f["boxes"].get(c)))
        crops = "".join(
            f'<figure><div class="crop" style="{crop_style(f, b, jpg)};outline-color:{col}"></div>'
            f'<figcaption><b style="color:{col}">{lbl}</b><br>'
            f'<span class="mono">D′ p50 {b["p50"]:.4f} · {100 * b["p50"] / f["dmax"]:.0f}% of Dmax<br>'
            f'p05 {b["p05"]:.4f} · p95 {b["p95"]:.4f} · spread {b["spread"]:.4f}<br>'
            f'{b["x"]},{b["y"]},{b["bw"]},{b["bh"]}</span></figcaption></figure>'
            for c, lbl, col in CATS if (b := f["boxes"].get(c)))
        # Two-sided sweep. An earlier downward-only range (-2 … 0) had every frame pick
        # EV 0 — the boundary — which means the optimum was at or beyond it and the range
        # was simply wrong. Upward variants clip 7–26 % of highlights at contrast 2.0,
        # which is itself a finding: exposure is the wrong knob for a raised floor.
        evs = [("-2", "m20"), ("-1.5", "m15"), ("-1", "m10"), ("-0.5", "m05"),
               ("0", "p00"), ("+0.5", "p05x"), ("+1", "p10x"), ("+1.5", "p15x")]
        variants = "".join(
            f'<figure class="v"><img src="{mark}-{tok}.jpg" alt="{mark} EV {ev}" loading="lazy">'
            f'<figcaption><b>EV {ev.replace("-", "\u2212")}</b></figcaption></figure>'
            for ev, tok in evs)

        secs.append(f"""
<section id="{mark}">
  <h2><span class="mark">{mark}</span> {roll} · <span class="mono">{f['file']}</span></h2>
  <p class="meta">Frozen Dmax <b>{f['dmax']:.4f}</b> · interior tile D′ min {f['tmin']:.3f} / med {f['tmed']:.3f} / max {f['tmax']:.3f} · {f['w']}×{f['h']}</p>
  <div class="full"><img src="{jpg}" alt="{mark}">{boxes}</div>
  <div class="crops">{crops}</div>
  <h3 style="margin-top:20px">Exposure variants — real <span class="mono">--print-exposure</span> at <span class="mono">--sigmoid-contrast 2.0</span></h3>
  <p class="meta">The full frame above is the <b>shipped</b> contrast 1.0; these are contrast 2.0
     (the datasheet-derived ≈2.07), which restores a black reference so exposure becomes judgeable.
     Which one reads as correctly exposed?</p>
  <div class="variants">{variants}</div>
  <div class="slider">
    <label>Free-form eyeball (non-photometric, does <b>not</b> map to a pipeline knob):
      brightness <input type="range" min="0.4" max="2.2" step="0.05" value="1"
        oninput="this.closest('section').querySelectorAll('.variants img,.full img').forEach(i=>{{i.dataset.b=this.value;i.style.filter='brightness('+this.value+') contrast('+(i.dataset.c||1)+')'}});this.nextElementSibling.textContent=(+this.value).toFixed(2)"><output>1.00</output>
      &nbsp; contrast <input type="range" min="0.6" max="2.4" step="0.05" value="1"
        oninput="this.closest('section').querySelectorAll('.variants img,.full img').forEach(i=>{{i.dataset.c=this.value;i.style.filter='brightness('+(i.dataset.b||1)+') contrast('+this.value+')'}});this.nextElementSibling.textContent=(+this.value).toFixed(2)"><output>1.00</output>
    </label>
  </div>
  <div class="qs">
    <h3>Questions for {mark}</h3>
    <ol>
      <li><b>{mark}-white</b> — is the <span style="color:#ff5d5d">red</span> box a <b>diffuse</b> reflector (cloud, white garment, painted wall) rather than a specular glint, a light source, or open sky? &nbsp; <i>yes / no — if no, where should it go?</i></li>
      <li><b>{mark}-mid</b> — is the <span style="color:#ffd23f">yellow</span> box a plausible <b>mid-grey-equivalent</b> surface? It was picked purely as the frame's median tile, which is a statistical accident, not a grey card. &nbsp; <i>yes / no</i></li>
      <li><b>{mark}-shadow</b> — is the <span style="color:#4ea3ff">blue</span> box <b>textured deep shadow</b> rather than a flat black hole or something non-photographic? &nbsp; <i>yes / no</i></li>
      <li><b>{mark}-exposure</b> — in the variant row above, <b>which EV reads as correctly
          exposed?</b> Answer with the EV label (e.g. &ldquo;{mark}: −0.5&rdquo;). This replaces the
          under/over question, which was genuinely hard to answer: at the shipped contrast the
          raised black floor leaves no black reference, so a frame reads as neither. It is the
          <i>relative</i> answers across frames that classify exposure — a frame needing more
          positive EV than its neighbours was thinner, i.e. underexposed.</li>
    </ol>
  </div>
</section>""")

    OUT.write_text(f"""<!doctype html>
<meta charset="utf-8">
<title>Phase 1 — patch review</title>
<style>
:root {{ color-scheme: dark light; }}
body {{ font: 15px/1.55 system-ui, sans-serif; margin: 0 auto; padding: 24px; max-width: 1180px;
        background: #14161a; color: #e8e8ea; }}
h1 {{ font-size: 22px; margin: 0 0 4px; }}
h2 {{ font-size: 17px; margin: 0 0 2px; }}
h3 {{ font-size: 14px; margin: 0 0 6px; text-transform: uppercase; letter-spacing: .06em; color: #9aa0a8; }}
.mono {{ font-family: ui-monospace, Menlo, monospace; font-size: 12.5px; }}
.dim {{ color: #9aa0a8; }}
.mark {{ display: inline-block; background: #2d6cdf; color: #fff; font-weight: 700;
         padding: 1px 8px; border-radius: 5px; margin-right: 6px; }}
.meta {{ color: #9aa0a8; font-size: 13px; margin: 0 0 10px; }}
section {{ border-top: 1px solid #2a2e36; padding: 22px 0; }}
.full {{ position: relative; display: block; line-height: 0; }}
.full img {{ width: 100%; height: auto; border-radius: 6px; }}
.box {{ position: absolute; border: 2.5px solid; border-radius: 3px; box-sizing: border-box; }}
.box span {{ position: absolute; top: -1px; left: -1px; font: 700 10.5px/1.5 system-ui, sans-serif;
             color: #000; padding: 0 5px; border-radius: 2px 0 3px 0; white-space: nowrap; }}
.crops {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(230px, 1fr)); gap: 14px; margin-top: 14px; }}
figure {{ margin: 0; }}
.crop {{ aspect-ratio: 328 / 342; outline: 2.5px solid; outline-offset: -2.5px; border-radius: 4px; }}
figcaption {{ font-size: 12px; margin-top: 6px; color: #c7ccd3; }}
.variants {{ display: grid; grid-template-columns: repeat(8, 1fr); gap: 10px; }}
.variants img {{ width: 100%; height: auto; border-radius: 5px; display: block; }}
figure.v figcaption {{ text-align: center; font-size: 12.5px; margin-top: 5px; }}
.slider {{ margin-top: 12px; font-size: 13px; color: #9aa0a8; }}
.slider input {{ vertical-align: middle; width: 150px; }}
.slider output {{ font-family: ui-monospace, monospace; font-size: 12px; }}
@media (max-width: 720px) {{ .variants {{ grid-template-columns: repeat(2, 1fr); }} }}
.qs {{ margin-top: 18px; background: #1b1f26; border-left: 3px solid #2d6cdf;
       padding: 12px 16px; border-radius: 0 6px 6px 0; }}
.qs ol {{ margin: 0; padding-left: 20px; }}
.qs li {{ margin: 7px 0; }}
table {{ border-collapse: collapse; width: 100%; font-size: 13px; margin: 14px 0 6px; }}
th, td {{ text-align: left; padding: 5px 9px; border-bottom: 1px solid #2a2e36; }}
th {{ color: #9aa0a8; font-weight: 600; }}
.note {{ background: #1b1f26; border-radius: 6px; padding: 12px 16px; font-size: 13.5px; }}
@media (prefers-color-scheme: light) {{
  body {{ background: #fff; color: #16181d; }}
  section {{ border-color: #e3e6ea; }} th, td {{ border-color: #e3e6ea; }}
  .qs, .note {{ background: #f5f7f9; }} .meta, .dim, h3 {{ color: #5d646d; }}
  figcaption {{ color: #3c424a; }}
}}
</style>

<h1>Phase 1 — confirm the declared patches</h1>
<p class="meta">Ten <code>real</code> frames across the three fixture rolls. Rendered with each roll's
frozen Dmin/Dmax through the <b>current sigmoid</b> (its shoulder avoids the highlight clipping the
exponential recipe would show — so the paleness you see is the defect under investigation).</p>

<div class="note">
<b>What I need.</b> The boxes were chosen by a deterministic rule, not by looking: shadow = lowest
median density with above-median texture, diffuse white = highest textured tile with the top two
dropped, mid-tone = the tile nearest the frame's median. That last rule is the weak one — a frame's
median tile is not a grey card. Confirm or move each box; every frame has its own numbered questions
below, keyed to its mark (e.g. <b>G1-white</b>), so we can name a specific box later.
<br><br>
<b>Why it matters.</b> The mid-tone → diffuse-white density difference is the calibration against the
manufacturer's published Δ (0.36; 0.40 for Gold 200). The Δ column below currently scatters
0.085–0.850 — that scatter is the auto-selection being semantically wrong, not a measurement result.
</div>

<table>
<tr><th>mark</th><th>stock</th><th>frame</th><th>shadow D′</th><th>mid-tone D′</th><th>diffuse white D′</th><th>Δ mid→white</th></tr>
{rows}
</table>
<p class="meta">Percentages are of that roll's frozen Dmax. Note no diffuse-white candidate reaches
100% — the measured headroom that the plan's leading hypothesis predicts.</p>

{''.join(secs)}
""", encoding="utf-8")
    print(f"wrote {OUT} ({len(frames)} frames)")


main()
