"""Generates the example designs for the test page with PhotoCraft's CLI.

Run on the machine with PhotoCraft installed:  python examples/generate.py
Each design is made at its template's placeholder size, so it fits exactly (as Photoshop's own
Replace Contents expects). Opaque designs for full-surface placeholders (T-shirt, canvas), black
silhouettes for single-colour ones (bottle label, die-cut stencil).
"""
import json, os, subprocess, sys

CLI = r"C:\Program Files\PhotoCraft\photocraft-cli.exe"
OUT = os.path.dirname(os.path.abspath(__file__))

def make(path, w, h, background, steps):
    args = [CLI, "run", "--new", json.dumps({"width": w, "height": h, "background": background})]
    for cmd, params in steps:
        args += ["--cmd", cmd]
        if params is not None:
            args += ["--params", json.dumps(params)]
    args += ["--out", path]
    os.makedirs(os.path.dirname(path), exist_ok=True)
    r = subprocess.run(args, capture_output=True, text=True)
    if r.returncode != 0:
        print("FAILED", path, r.stderr[-800:], file=sys.stderr)
        sys.exit(1)
    print("ok", os.path.relpath(path, OUT))

def shape(kind, rect, fill, **kw):
    p = {"kind": kind, "rect": rect, "fill": fill}
    p.update(kw)
    return ("shape.create", p)

def text(x, y, s, size, color, font="Arial Black", align="center"):
    return ("type.create", {"x": x, "y": y, "text": s, "font": font, "size": size, "color": color, "align": align})

def gradient(stops, angle=0, style="linear"):
    return {"gradient": {"stops": stops, "angle": angle, "style": style}}

manifest = {}

def add(template, files):
    manifest[template] = files

# ---- T-shirt: full-surface placeholder (2593 x 2873), designs must be opaque -----------------
W, H = 2593, 2873
d = "tshirt"
make(f"{OUT}/{d}/1.png", W, H, "#f4a261", [
    shape("ellipse", [596, 500, 1400, 1400], gradient([[0, "#e76f51"], [1, "#264653"]], 60)),
    shape("star", [1050, 2050, 500, 500], "#ffffff", sides=5, starRatio=0.5),
    text(1296, 2000, "SUNSET", 320, "#ffffff"),
])
dots = []
for row in range(8):
    for col in range(7):
        x = 150 + col * 340 + (170 if row % 2 else 0)
        y = 150 + row * 340
        dots.append(shape("ellipse", [x, y, 150, 150], "#1d3557"))
make(f"{OUT}/{d}/2.png", W, H, "#ffffff", dots)
make(f"{OUT}/{d}/3.png", W, H, "#e9ecef", [
    shape("star", [1146, 600, 300, 300], "#212529", sides=6, starRatio=0.55),
    text(1296, 1120, "STUDIO", 180, "#212529"),
    shape("rect", [1146, 1180, 300, 12], "#212529"),
])
add("T-shirt_landscape_white", [
    {"file": f"{d}/1.png", "label": "Sunset print (opaque)"},
    {"file": f"{d}/2.png", "label": "Polka dots (opaque)"},
    {"file": f"{d}/3.png", "label": "Minimal logo (opaque)"},
])

# ---- Bottle label (12992 x 2716): single-colour print, black silhouettes --------------------
W, H = 12992, 2716
d = "bottle"
make(f"{OUT}/{d}/1.png", W, H, "transparent", [
    text(6496, 1850, "AQUA", 1250, "#000000"),
    shape("rect", [4700, 2050, 3600, 60], "#000000"),
])
make(f"{OUT}/{d}/2.png", W, H, "transparent", [shape("rect", [900 + i * 1300, 300, 300, 2100], "#000000") for i in range(9)])
make(f"{OUT}/{d}/3.png", W, H, "transparent", [
    shape("star", [1800, 300, 2100, 2100], "#000000", sides=5, starRatio=0.5),
    text(8200, 1750, "PURE", 1000, "#000000"),
])
add("Bottle_landscape_red", [
    {"file": f"{d}/1.png", "label": "AQUA wordmark"},
    {"file": f"{d}/2.png", "label": "Stripes"},
    {"file": f"{d}/3.png", "label": "Star + PURE"},
])

# ---- Hoodie print area (6496 x 1358) --------------------------------------------------------
W, H = 6496, 1358
d = "hoodie"
make(f"{OUT}/{d}/1.png", W, H, "transparent", [
    shape("star", [300, 230, 900, 900], "#ffffff", sides=5, starRatio=0.5),
    text(3900, 1020, "NORTH", 900, "#ffffff", font="Impact"),
])
make(f"{OUT}/{d}/2.png", W, H, "transparent", [
    shape("ellipse", [400, 150, 1050, 1050], "#f4a261"),
    shape("roundedRect", [1900, 250, 2200, 850], "#2a9d8f", radii=120),
    shape("star", [4600, 100, 1150, 1150], "#e9c46a", sides=5, starRatio=0.5),
])
make(f"{OUT}/{d}/3.png", W, H, "transparent", [
    shape("roundedRect", [100, 150, 6296, 1050], gradient([[0, "#ff9a8b"], [1, "#ff6a88"]], 0), radii=200),
    text(3248, 980, "HOODIE CO.", 620, "#1d1d1d"),
])
add("hoodie_landscape", [
    {"file": f"{d}/1.png", "label": "White wordmark"},
    {"file": f"{d}/2.png", "label": "Colour shapes"},
    {"file": f"{d}/3.png", "label": "Gradient bar"},
])

# ---- Tote bag (2296 x 2329) -------------------------------------------------------------------
W, H = 2296, 2329
d = "bag"
make(f"{OUT}/{d}/1.png", W, H, "transparent", [
    shape("ellipse", [548, 350, 1200, 1200], None, stroke={"width": 60, "color": "#111111"}),
    text(1148, 1050, "MARKET", 260, "#111111"),
    text(1148, 1850, "EST. 2026", 120, "#111111", font="Georgia"),
])
make(f"{OUT}/{d}/2.png", W, H, "transparent", [
    shape("ellipse", [348, 500, 1000, 1000], "#e63946"),
    shape("ellipse", [948, 500, 1000, 1000], "#f1c40f"),
    shape("ellipse", [648, 1000, 1000, 1000], "#2a9d8f"),
])
make(f"{OUT}/{d}/3.png", W, H, "transparent", [
    text(1148, 1400, "AB", 1100, "#111111"),
    shape("rect", [648, 1500, 1000, 50], "#111111"),
])
add("Bags_square - original", [
    {"file": f"{d}/1.png", "label": "Black logo"},
    {"file": f"{d}/2.png", "label": "Colour circles"},
    {"file": f"{d}/3.png", "label": "Monogram"},
])

# ---- Canvas (1600 x 1600): full-surface artwork, opaque -------------------------------------
W, H = 1600, 1600
d = "canvas"
make(f"{OUT}/{d}/1.png", W, H, "#2b2d42", [
    ("layer.newFillLayer.gradient", {"from": "#ff9a8b", "to": "#2b2d42", "angle": 90, "style": "linear"}),
    shape("ellipse", [500, 350, 600, 600], "#ffd166"),
    shape("rect", [0, 1150, 1600, 450], "#1b1b2f"),
])
make(f"{OUT}/{d}/2.png", W, H, "#0b132b", [
    shape("polygon", [100 + (i % 4) * 375, 100 + (i // 4) * 375, 325, 325], ["#5bc0be", "#ffd166", "#ef476f", "#06d6a0"][i % 4], sides=3 + i % 4) for i in range(16)
])
make(f"{OUT}/{d}/3.png", W, H, "#f8f1e5", [
    text(800, 900, "HELLO", 420, "#222222"),
    shape("rect", [300, 980, 1000, 30], "#e63946"),
    text(800, 1200, "nice to meet you", 90, "#555555", font="Georgia"),
])
add("canvas_square - original", [
    {"file": f"{d}/1.png", "label": "Sunset (opaque)"},
    {"file": f"{d}/2.png", "label": "Geometric (opaque)"},
    {"file": f"{d}/3.png", "label": "Poster (opaque)"},
])

# ---- Coffee die-cut stencil (625 x 471): dark silhouettes only --------------------------------
W, H = 625, 471
d = "diecut"
make(f"{OUT}/{d}/1.png", W, H, "transparent", [shape("star", [112, 30, 400, 400], "#000000", sides=5, starRatio=0.5)])
make(f"{OUT}/{d}/2.png", W, H, "transparent", [
    shape("ellipse", [150, 70, 170, 170], "#000000"),
    shape("ellipse", [305, 70, 170, 170], "#000000"),
    ("shape.create", {"kind": "path", "fill": "#000000", "path": {"subpaths": [{"closed": True, "knots": [[160, 190], [465, 190], [312, 420]]}]}}),
])
make(f"{OUT}/{d}/3.png", W, H, "transparent", [
    text(312, 300, "CAFE", 170, "#000000"),
    shape("ellipse", [262, 340, 100, 60], "#000000"),
])
add("Coffee Die-Cut Mockup - original", [
    {"file": f"{d}/1.png", "label": "Star (black)"},
    {"file": f"{d}/2.png", "label": "Heart (black)"},
    {"file": f"{d}/3.png", "label": "CAFE (black)"},
])

with open(f"{OUT}/manifest.json", "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=1, ensure_ascii=False)
print("wrote manifest.json with", sum(len(v) for v in manifest.values()), "examples")
