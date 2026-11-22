#!/usr/bin/env python3
"""Rendering benchmark: the same demo scenes, measured before and after a change.

Runs the viewer (release build by default) in demo mode once per scene, with
AURORA_PROFILE=1, an uncapped frame rate and no vertical sync, the same
window size and graphics level each time, then reads each scene's
`perf summary` lines (docs/TESTING.md, "Profiling") and keeps the median of
every number after a warm-up. The results of a run are saved under a label,
so that two runs (a baseline and a change) can be compared scene by scene,
GPU element by element.

Usage (from the repository root, after `cargo build --release -p aurora-viewer`):

    python3 scripts/render-bench.py --label main                 # every scene
    python3 scripts/render-bench.py --label change --repeat 3    # alternate 3 times
    python3 scripts/render-bench.py --compare main change        # the table of both
    python3 scripts/render-bench.py --scenes plaza,foliage --preset medium --label m

Measure on a quiet machine: nothing else drawing, the window in front, the
same power plan. Each scene runs `--repeat` times (alternating scenes), and
the median of the runs is kept. The demo's saved settings are not changed:
every setting a scene needs is an AURORA_* variable of that launch only.
"""

import argparse
import json
import os
import platform
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

# name: (what it measures, variables)
SCENES = {
    "plaza": ("the demo plaza from the start position", {}),
    "crowd": ("40 avatars in rigged meshes", {"AURORA_DEMO_CROWD": "40", "AURORA_DEMO_POS": "150,127"}),
    "foliage": (
        "64 layers of cut-out (Mask) panes, like mesh grass",
        {"AURORA_DEMO_ALPHA_MASK_AB": "planes:64:mask", "AURORA_DEMO_POS": "144,125", "AURORA_DEMO_TOD": "5"},
    ),
    "blend": (
        "64 layers of blended panes",
        {"AURORA_DEMO_ALPHA_MASK_AB": "planes:64:blend", "AURORA_DEMO_POS": "144,125", "AURORA_DEMO_TOD": "5"},
    ),
    "textures": ("9,000 textured cubes", {"AURORA_DEMO_TEXTURES": "9000"}),
}

# every launch: the frame rate the GPU gives, no pacing wait, a stable sun
COMMON = {
    "AURORA_DEMO": "1",
    "AURORA_PROFILE": "1",
    "AURORA_FPS_LIMIT": "0",
    "AURORA_PRESENT_MODE": "immediate",
}
# the first summaries: loading, pipelines, textures streaming in
WARMUP = 10
NUMBER = re.compile(r"(\w+)=(-?\d+(?:\.\d+)?)")
HEADLINE = ["fps", "frame", "p95", "gpu"]


def logs_dir() -> Path:
    """The viewer's logs directory (logging.rs `logs_dir`)."""
    home = Path.home()
    system = platform.system()
    if system == "Windows":
        return Path(os.environ.get("LOCALAPPDATA", home / "AppData/Local")) / "Aurora/AuroraViewer/data/logs"
    if system == "Darwin":
        return home / "Library/Application Support/org.Aurora.AuroraViewer/logs"
    return Path(os.environ.get("XDG_DATA_HOME", home / ".local/share")) / "auroraviewer/logs"


def default_exe() -> Path:
    name = "aurora-viewer.exe" if platform.system() == "Windows" else "aurora-viewer"
    return Path("target/release") / name


def results_dir() -> Path:
    return Path(".scratch/render-bench")


def run(exe: Path, scene: str, seconds: float, logs: Path, size: str, preset: str, extra: dict) -> dict:
    _, variables = SCENES[scene]
    env = {k: v for k, v in os.environ.items() if not k.startswith("AURORA_")}
    env.update(COMMON)
    env.update({"AURORA_WINDOW_SIZE": size, "AURORA_GRAPHICS_PRESET": preset})
    env.update(variables)
    env.update(extra)
    name = f"render-bench-{scene}"
    env["AURORA_LOG_NAME"] = name
    log = logs / f"{name}.log"
    log.unlink(missing_ok=True)
    print(f"  {scene}: {seconds:.0f} s", flush=True)
    proc = subprocess.Popen([str(exe), "--title", f"Bench {scene} [AUTOMATION - DO NOT TOUCH]"], env=env)
    try:
        time.sleep(seconds)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
    time.sleep(1.5)  # the log is flushed once a second
    return read(log)


def read(path: Path) -> dict:
    """The median of every number of the scene's summaries, warm-up left out."""
    if not path.exists():
        return {}
    values: dict[str, list[float]] = {}
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = [l for l in text.splitlines() if "perf summary:" in l]
    for line in lines[WARMUP:]:
        body = line.split("perf summary:", 1)[1]
        # the `max:` and `off_cpu:` segments repeat keys with their longest frame
        kept = [seg for seg in body.split(" | ") if not seg.strip().startswith(("max:", "off_cpu:"))]
        for k, v in NUMBER.findall(" ".join(kept)):
            values.setdefault(k, []).append(float(v))
    out = {k: statistics.median(v) for k, v in values.items()}
    out["samples"] = max(0, len(lines) - WARMUP)
    settings = [l.split("perf settings", 1)[1].strip() for l in text.splitlines() if "perf settings" in l]
    if settings:
        out["settings"] = settings[-1][:600]
    return out


def merge(runs: list[dict]) -> dict:
    """The median over repeated runs of the same scene."""
    keys = {k for r in runs for k, v in r.items() if isinstance(v, float)}
    out = {k: statistics.median([r[k] for r in runs if k in r]) for k in keys}
    out["samples"] = sum(r.get("samples", 0) for r in runs)
    out["runs"] = len(runs)
    for r in runs:
        if "settings" in r:
            out["settings"] = r["settings"]
    return out


def compare(a_label: str, b_label: str) -> str:
    a = json.loads((results_dir() / f"{a_label}.json").read_text())
    b = json.loads((results_dir() / f"{b_label}.json").read_text())
    out = [f"Run `{a_label}` against `{b_label}` ({a['meta']['size']}, preset {a['meta']['preset']})", ""]
    out.append(f"| Scene | fps {a_label} | fps {b_label} | Δ fps | GPU ms {a_label} | GPU ms {b_label} | Δ GPU |")
    out.append("|---|---|---|---|---|---|---|")
    for scene in a["scenes"]:
        ra, rb = a["scenes"][scene], b["scenes"].get(scene, {})
        if "fps" not in ra or "fps" not in rb:
            continue
        dfps = (rb["fps"] / ra["fps"] - 1) * 100 if ra["fps"] else 0
        ga, gb = ra.get("gpu"), rb.get("gpu")
        dg = "–" if not ga or gb is None else f"{(gb / ga - 1) * 100:+.1f} %"
        out.append(
            f"| {scene} | {ra['fps']:.0f} | {rb['fps']:.0f} | {dfps:+.1f} % | "
            f"{ga if ga is not None else '–'} | {gb if gb is not None else '–'} | {dg} |"
        )
    out.append("")
    for scene in a["scenes"]:
        ra, rb = a["scenes"][scene], b["scenes"].get(scene, {})
        gkeys = sorted(
            {k for k in list(ra) + list(rb) if k.startswith("g_")},
            key=lambda k: -max(ra.get(k, 0), rb.get(k, 0)),
        )[:14]
        if not gkeys:
            continue
        out.append(f"**{scene}**: " + ", ".join(f"`{k[2:]}` {ra.get(k, 0):.3f}→{rb.get(k, 0):.3f}" for k in gkeys))
        out.append("")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--exe", type=Path, default=default_exe())
    ap.add_argument("--label", help="name of this run's results (.scratch/render-bench/<label>.json)")
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--repeat", type=int, default=2)
    ap.add_argument("--scenes", default=",".join(SCENES), help="comma-separated, among: " + ", ".join(SCENES))
    ap.add_argument("--size", default="1920,1080")
    ap.add_argument("--preset", default="high")
    ap.add_argument("--env", action="append", default=[], help="extra KEY=VALUE for every launch")
    ap.add_argument("--compare", nargs=2, metavar=("A", "B"))
    args = ap.parse_args()

    if args.compare:
        print(compare(*args.compare))
        return 0
    if not args.label:
        print("--label or --compare is needed", file=sys.stderr)
        return 2
    scenes = [s.strip() for s in args.scenes.split(",") if s.strip()]
    unknown = [s for s in scenes if s not in SCENES]
    if unknown:
        print(f"unknown scenes: {', '.join(unknown)}", file=sys.stderr)
        return 2
    if not args.exe.exists():
        print(f"{args.exe} not found: cargo build --release -p aurora-viewer", file=sys.stderr)
        return 2
    extra = dict(e.split("=", 1) for e in args.env)
    logs = logs_dir()
    runs: dict[str, list[dict]] = {s: [] for s in scenes}
    print(f"Running {len(scenes)} scenes x {args.repeat} at {args.size}, preset {args.preset}; leave the windows alone.")
    for _ in range(args.repeat):
        for s in scenes:
            r = run(args.exe, s, args.seconds, logs, args.size, args.preset, extra)
            if r:
                runs[s].append(r)
    result = {
        "meta": {"size": args.size, "preset": args.preset, "seconds": args.seconds, "env": extra, "exe": str(args.exe)},
        "scenes": {s: merge(r) for s, r in runs.items() if r},
    }
    results_dir().mkdir(parents=True, exist_ok=True)
    path = results_dir() / f"{args.label}.json"
    path.write_text(json.dumps(result, indent=1))
    print(f"\nSaved {path}")
    for s, r in result["scenes"].items():
        head = " ".join(f"{k}={r[k]:.2f}" for k in HEADLINE if k in r)
        top = sorted(((k, v) for k, v in r.items() if k.startswith("g_")), key=lambda kv: -kv[1])[:12]
        print(f"{s}: {head}\n   " + " ".join(f"{k[2:]}={v:.3f}" for k, v in top))
    return 0


if __name__ == "__main__":
    sys.exit(main())
