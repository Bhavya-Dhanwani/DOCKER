"""Turns bench/docker.json and bench/sasta.json (from scripts/bench.ps1) into SVG bar charts in bench/ for the README.
No dependencies:  python scripts/bench_charts.py"""
import json, pathlib

root = pathlib.Path(__file__).resolve().parent.parent / "bench"
d = json.loads((root / "docker.json").read_text(encoding="utf-8-sig"))
s = json.loads((root / "sasta.json").read_text(encoding="utf-8-sig"))

DOCKER, SASTA = "#6b7a90", "#2f81f7"
# (file, title, unit, key getter, lower is better)
charts = [
    ("ram-idle", "RAM used while idle", "MB", lambda r: r["idle_ram_mb"]),
    ("ram-loaded", f"RAM with {s['containers']} containers running", "MB", lambda r: r["loaded_ram_mb"]),
    ("ready", "Engine ready after launch", "s", lambda r: r["ready_s"]),
    ("start", "Start a container (median)", "ms", lambda r: r["start_median_ms"]),
    ("ps", "docker ps answers in (median)", "ms", lambda r: r["ps_median_ms"]),
    ("start20", f"Start {s['containers']} nginx containers", "ms", lambda r: r["start20_ms"]),
    ("remove20", f"Remove {s['containers']} containers", "ms", lambda r: r["remove20_ms"]),
    ("compose", "Compose up + down (web + cache)", "ms", lambda r: r["compose_up_ms"] + r["compose_down_ms"]),
    ("build", "Build, nothing cached", "ms", lambda r: r["build_cold_ms"]),
    ("pull", "Pull nginx:alpine and redis:alpine", "ms", lambda r: sum(r["pull_ms"].values())),
    ("disk", "VM disk file after the test", "MB", lambda r: r["vhdx_mb"]),
    ("install", "Installed size", "MB", lambda r: r["install_mb"]),
]


def fmt(v, unit):
    if unit == "s":
        return f"{v:.0f} s"
    if unit == "ms" and v >= 1000:
        return f"{v / 1000:.1f} s"
    return f"{v:,.0f} {unit}"


def svg(title, unit, dv, sv):
    top = max(dv, sv) or 1
    w, bar_x, bar_w = 560, 150, 280
    rows = [("Docker Desktop", dv, DOCKER), ("Sasta Docker", sv, SASTA)]
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="190" viewBox="0 0 {w} 190" font-family="Segoe UI, Helvetica, Arial, sans-serif">',
           f'<rect width="{w}" height="190" rx="14" fill="#0d1117" stroke="#30363d"/>',
           f'<text x="24" y="38" fill="#e6edf3" font-size="18" font-weight="600">{title}</text>']
    for i, (name, v, col) in enumerate(rows):
        y = 66 + i * 50
        bw = max(4, round(bar_w * v / top))
        out.append(f'<text x="24" y="{y + 20}" fill="#9da7b3" font-size="13">{name}</text>')
        out.append(f'<rect x="{bar_x}" y="{y}" width="{bw}" height="28" rx="5" fill="{col}"/>')
        out.append(f'<text x="{bar_x + bw + 10}" y="{y + 20}" fill="#e6edf3" font-size="13" font-weight="600">{fmt(v, unit)}</text>')
    if dv and sv:
        better = (dv - sv) / dv * 100
        ratio = dv / sv if sv else 0
        note = f"{ratio:.1f}x less" if ratio >= 1.5 else f"{better:.0f}% less"
        if sv > dv:
            note = f"{sv / dv:.1f}x more" if sv / dv >= 1.5 else f"{(sv - dv) / dv * 100:.0f}% more"
        out.append(f'<text x="24" y="172" fill="#3fb950" font-size="13" font-weight="600">Sasta Docker: {note}</text>' if sv <= dv
                   else f'<text x="24" y="172" fill="#d29922" font-size="13" font-weight="600">Sasta Docker: {note}</text>')
    out.append("</svg>")
    return "\n".join(out)


for name, title, unit, get in charts:
    (root / f"{name}.svg").write_text(svg(title, unit, get(d), get(s)), encoding="utf-8")
    print(name, get(d), get(s))
