#!/usr/bin/env python3
"""Plan and present the candidate-anchor comparison for `algo/reference-anchored-sigmoid`.

Two modes, sharing one definition of the anchor rules so the rendered images and the page
can never disagree:

    --plan  fixtures.json                 -> one "mark|id|anchor|contrast|roll|recipe|file" row per render
    --page  fixtures.json  out.html       -> the comparison page

**Every anchoring form reduces to one number** — the sigmoid anchor `A` (`curve.dmax`) plus
a contrast — because `t = contrast·(D′ − A)` is unchanged and only the rule for choosing `A`
differs:

    white pinned at W          ->  A = W
    mid pinned at M (-> 0.18)  ->  A = M + 0.745/contrast
    black pinned at T (at D'=0)->  A = -log10(T)/contrast
"""
import json
import math
import sys

MID_DECADES = -math.log10(0.18)          # 0.7447: how far below white a mid-grey sits
DS_MID = {"Ektar": 0.62, "Portra160-2026-07-22": 0.67, "2026-07-24-Gold200": 0.73}
DS_DELTA = {"2026-07-24-Gold200": 0.40}  # others 0.36


def rules(roll, dmax):
    """(id, label, anchor, contrast, default_eligible, note) for one roll."""
    c_ds = MID_DECADES / DS_DELTA.get(roll, 0.36)
    mid = lambda m, c: m + MID_DECADES / c
    blk = lambda t, c: -math.log10(t) / c
    return [
        ("c1", "1 · white@Dmax · c=1.0 (shipped)", dmax, 1.0, True,
         "the shipped default — midtones nearly right, blacks pale"),
        ("c2", "2 · white@Dmax · c=2.0", dmax, 2.0, True,
         "photographic contrast, but the pivot around white drags midtones down"),
        ("c3", "3 · mid@0.5·Dmax · c=2.0", mid(0.5 * dmax, 2.0), 2.0, True,
         "Dmax-referenced, so contingent on a film-base fix"),
        ("c4", "4 · auto (content-driven) · c=2.0", "auto", 2.0, False,
         "explicit-mode only; NOTE the shipped Auto measures the whole frame, so the "
         "opaque film holder dominates the 99.5th percentile"),
        ("c5a", "5a · black@0.002 · c=2.0", blk(0.002, 2.0), 2.0, True,
         "black-pinned at a target consistent with c=2.0 (anchor 1.349)"),
        ("c5b", "5b · black@0.005 · c=2.0", blk(0.005, 2.0), 2.0, True,
         "black-pinned, shallower target (anchor 1.151)"),
        ("c7", "7 · auto (content-driven) · c=0.745/Δ", "auto", c_ds, False,
         "explicit-mode only; same holder contamination as 4"),
        ("c8", "8 · mid@Dmin+datasheet · c=0.745/Δ", mid(DS_MID[roll], c_ds), c_ds, True,
         "reference-driven and Dmax-free — the leading shippable form"),
    ]


SHOULDER_STUDY = [("c3", "3 · mid@0.5·Dmax"), ("c8", "8 · mid@Dmin+datasheet")]
SHOULDERS = [0.2, 0.6, 1.0]


def shoulder_rules(roll, dmax):
    """The shoulder study: the two GO forms at three shoulder widths.

    The shipped `shoulder = 0.2` was calibrated for a regime where content essentially
    never exceeded white (anchor at Dmax, nothing reaches it). Moving the anchor down to
    diffuse white makes the shoulder **load-bearing for the first time** — so this is a
    form-viability question, not the fine-tuning that was deferred. Confirmed analytically
    on P3: at 0.2 the output gap across the curtain's density range collapses to 0.00003,
    while 1.0 restores 0.0502 for ~0.24 EV of midtone cost.
    """
    by_id = {r[0]: r for r in rules(roll, dmax)}
    out = []
    for cid, short in SHOULDER_STUDY:
        _i, _l, anchor, contrast, _de, _n = by_id[cid]
        for sh in SHOULDERS:
            tag = f"{cid}s{str(sh).replace('.', '')}"
            note = "shipped shoulder" if sh == 0.2 else \
                   f"wider shoulder — recovers highlight differentiation above white"
            out.append((tag, f"{short} · shoulder {sh}", anchor, contrast, sh, True, note))
    return out


def plan(fx, shoulder_study=False):
    for mk in sorted(fx["frames"]):
        f = fx["frames"][mk]
        roll = f["roll"]
        recipe = fx["rolls"][roll]["recipe"]
        rs = shoulder_rules(roll, f["roll_dmax"]) if shoulder_study else \
             [(r[0], r[1], r[2], r[3], 0.2, r[4], r[5]) for r in rules(roll, f["roll_dmax"])]
        for cid, _lbl, anchor, contrast, sh, _de, _n in rs:
            a = "auto" if anchor == "auto" else f"{anchor:.6f}"
            print(f"{mk}|{cid}|{a}|{contrast:.6f}|{sh}|{roll}|{recipe}|{f['file']}")


def page(fx, out, shoulder_study=False):
    marks = sorted(fx["frames"])
    secs = []
    for mk in marks:
        f = fx["frames"][mk]
        roll = f["roll"]
        tiles = []
        rs = shoulder_rules(roll, f["roll_dmax"]) if shoulder_study else \
             [(r[0], r[1], r[2], r[3], 0.2, r[4], r[5]) for r in rules(roll, f["roll_dmax"])]
        for cid, lbl, anchor, contrast, sh, de, note in rs:
            a = "auto (measured)" if anchor == "auto" else f"{anchor:.4f}"
            badge = "" if de else '<span class="tag">explicit-mode only</span>'
            tiles.append(
                f'<figure class="v" data-src="{mk}-{cid}.jpg" data-key="{mk}-{cid}" '
                f'data-form="{lbl}">'
                f'<img src="{mk}-{cid}.jpg" alt="{mk} {cid}" loading="lazy">'
                f'<figcaption><b>{lbl}</b>{badge}<br>'
                f'<span class="mono">anchor {a} · contrast {contrast:.3f} · shoulder {sh}</span><br>'
                f'<span class="note">{note}</span></figcaption></figure>')
        ev = f.get("exposure_preference_ev")
        evtxt = f"your preferred EV at c=2.0: <b>{ev:+.1f}</b>" if ev is not None else \
                f"<b>mixed</b> — {f.get('ev_note', 'scene range exceeds SDR')}"
        secs.append(f"""
<section id="{mk}">
  <h2><span class="mark">{mk}</span> {roll} · <span class="mono">{f['file']}</span></h2>
  <p class="meta">Dmax {f['roll_dmax']:.4f} · {evtxt} · valid patches:
     shadow {str(f['patches']['shadow']['valid']).lower()},
     mid {str(f['patches']['mid']['valid']).lower()},
     white {str(f['patches']['white']['valid']).lower()}</p>
  <div class="grid">{''.join(tiles)}</div>
</section>""")

    out.write_text(f"""<!doctype html>
<meta charset="utf-8">
<title>Candidate anchoring forms</title>
<style>
:root {{ color-scheme: dark light; }}
body {{ font: 15px/1.55 system-ui, sans-serif; margin: 0 auto; padding: 24px; max-width: 1400px;
        background: #14161a; color: #e8e8ea; }}
h1 {{ font-size: 22px; margin: 0 0 6px; }} h2 {{ font-size: 17px; margin: 0 0 2px; }}
.mono {{ font-family: ui-monospace, Menlo, monospace; font-size: 12px; }}
.mark {{ display:inline-block; background:#2d6cdf; color:#fff; font-weight:700;
         padding:1px 8px; border-radius:5px; margin-right:6px; }}
.meta {{ color:#9aa0a8; font-size:13px; margin:0 0 12px; }}
section {{ border-top:1px solid #2a2e36; padding:22px 0; }}
.grid {{ display:grid; grid-template-columns:repeat(3, 1fr); gap:14px; }}
@media (max-width:1000px) {{ .grid {{ grid-template-columns:repeat(2,1fr); }} }}
figure.v {{ margin:0; cursor:zoom-in; }}
figure.v img {{ width:100%; height:auto; border-radius:5px; display:block; }}
figcaption {{ font-size:12px; margin-top:6px; color:#c7ccd3; }}
.note {{ color:#9aa0a8; font-size:11.5px; }}
.tag {{ background:#5a3030; color:#ffd9d9; font-size:10px; padding:1px 5px;
        border-radius:3px; margin-left:6px; white-space:nowrap; }}
#lb {{ position:fixed; inset:0; z-index:80; display:grid; align-content:center;
       justify-items:center; background:rgba(8,9,11,.95); }}
#lb[hidden] {{ display:none; }}
#lbImg {{ max-width:88vw; max-height:84vh; border-radius:4px; }}
#lbLabel {{ margin-top:12px; font:600 15px/1 ui-monospace,Menlo,monospace; }}
#lbHint {{ margin-top:6px; font:15px/1.4 system-ui,sans-serif; color:#c7ccd3; }}
#lb button {{ position:fixed; top:50%; transform:translateY(-50%); font-size:30px;
              padding:14px 20px; border:0; border-radius:8px; cursor:pointer;
              background:rgba(255,255,255,.14); color:#fff; }}
#lbPrev {{ left:2vw; }} #lbNext {{ right:2vw; }}
.note-box {{ background:#1b1f26; border-radius:6px; padding:12px 16px; font-size:13.5px; }}
@media (prefers-color-scheme: light) {{
  body {{ background:#fff; color:#16181d; }} section {{ border-color:#e3e6ea; }}
  .note-box {{ background:#f5f7f9; }} .meta,.note {{ color:#5d646d; }}
  figcaption {{ color:#3c424a; }}
}}
</style>

<h1>Candidate anchoring forms — which to support</h1>
<p class="meta">All render through <span class="mono">--output-preset ultra-hdr-v1</span>, whose
JPEG base is the <span class="mono">pipeline::sdr</span> rendition — the renderer the metrics
measure. <b>No exposure applied</b>, so each tile shows where its anchor actually places the
image. Click any tile to enlarge; ‹ / › or arrow keys step across the row.</p>

<div class="note-box">
Every form reduces to <b>one number</b> — the sigmoid anchor plus a contrast — so these differ
only in how the anchor is chosen. Two things to watch: <b>4 and 7</b> are the shipped
content-driven <span class="mono">Auto</span>, which measures the <i>whole frame</i> and is
therefore dominated by the opaque film holder (it resolves to 2.23–2.37 against a roll Dmax of
1.28–1.38) — so they show holder contamination, not content adaptation. And <b>1</b> is the
shipped default: judge whether its blacks are the defect you originally reported.
</div>

{''.join(secs)}

<div id="lb" hidden>
  <button id="lbPrev" type="button" aria-label="previous">&lsaquo;</button>
  <img id="lbImg" alt="">
  <div id="lbLabel"></div>
  <div id="lbHint"></div>
  <button id="lbNext" type="button" aria-label="next">&rsaquo;</button>
</div>
<script>
(function () {{
  var lb=document.getElementById('lb'), img=document.getElementById('lbImg'),
      label=document.getElementById('lbLabel'), row=[], i=0;
  function show(n) {{ i=(n+row.length)%row.length; var f=row[i];
    img.src=f.dataset.src; label.textContent=f.dataset.key;
    document.getElementById('lbHint').textContent=f.dataset.form; }}
  function open(fig) {{ row=Array.prototype.slice.call(
      fig.closest('.grid').querySelectorAll('figure.v')); show(row.indexOf(fig)); lb.hidden=false; }}
  function close() {{ lb.hidden=true; img.removeAttribute('src'); row=[]; }}
  document.addEventListener('click', function (e) {{
    var fig=e.target.closest?e.target.closest('figure.v'):null;
    if (fig) {{ open(fig); return; }}
    if (lb.hidden) return;
    if (e.target.id==='lbPrev') {{ show(i-1); return; }}
    if (e.target.id==='lbNext') {{ show(i+1); return; }}
    if (lb.contains(e.target)) close();
  }});
  document.addEventListener('keydown', function (e) {{
    if (lb.hidden) return;
    if (e.key==='Escape') close();
    else if (e.key==='ArrowLeft') show(i-1);
    else if (e.key==='ArrowRight') show(i+1);
  }});
}})();
</script>
""", encoding="utf-8")
    print(f"wrote {out} ({len(marks)} frames x {len(rules(fx['frames'][marks[0]]['roll'], 1.0)) if not shoulder_study else len(SHOULDER_STUDY) * len(SHOULDERS)} tiles)")


def main():
    mode = sys.argv[1]
    fx = json.loads(open(sys.argv[2]).read())
    study = "--shoulder" in sys.argv
    if mode == "--plan":
        plan(fx, study)
    elif mode == "--page":
        from pathlib import Path
        page(fx, Path(sys.argv[3]), study)
    else:
        sys.exit("usage: --plan fixtures.json | --page fixtures.json out.html")


main()
