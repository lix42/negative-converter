import json, subprocess, sys, os, pathlib
NC = "./target/debug/nc"
ROOT = pathlib.Path("../nc-assets")
recipes = pathlib.Path("scripts/real-scan-verify/recipes")
man = json.load(open(ROOT/"manifest.json"))
out = pathlib.Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
rows=[]
for roll, data in sorted(man["rolls"].items()):
    rp = recipes/f"{roll}.json"
    if not rp.exists(): continue
    frames=[f for f in data["frames"] if f["role"]=="real"]
    if not frames: continue
    scan = ROOT/frames[0]["file"]
    base = json.load(open(rp))["film_base"]["source"]["explicit"]
    for label, argv, ext in (
        ("v2 (legacy TIFF)", ["--output-preset","legacy"], "tiff"),
        ("v3 (gain-map-hdr)", [], "jpg"),
    ):
        dst = out/f"{roll}-{label.split()[0]}.{ext}"
        cmd=[NC,"convert",str(scan),"-o",str(dst),"--film-base",",".join(map(str,base)),
             "--max-memory","16GiB",*argv]
        p=subprocess.run(cmd,capture_output=True,text=True)
        if p.returncode!=0:
            rows.append((roll,label,"FAILED",p.stderr.strip().splitlines()[-1][:90])); continue
        r=json.loads(p.stdout)
        loss=r.get("loss") or {}
        tot=loss.get("total_samples") or 1
        clipped=(loss.get("clipped_low",0)+loss.get("clipped_high",0))/tot*100
        mean=r.get("output_stats",{}).get("mean")
        rows.append((roll,label,f"{clipped:.3f}%", ", ".join(f"{m:.4f}" for m in mean) if mean else "-"))
print(json.dumps(rows, indent=1))
