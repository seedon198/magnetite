"""Draw the capability-model and address-space diagrams. Usage: model_svg.py <out_dir>

Writes capabilities.svg and memory-layout.svg in the same palette and type as
the other diagrams. Addresses come from kernel/src/main.rs; object kinds and
rights from libs/capability.
"""
import sys
from pathlib import Path
from xml.sax.saxutils import escape

W = 880
SANS = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
BG, PANEL, BOX, LINE, SOLID, FG, MUTED, ACCENT, OK, WARN = (
    "#0f1218", "#161b23", "#1d2330", "#3a4250", "#4a5568", "#e6e9ef", "#9198a1", "#7fa6e8", "#7fb88a", "#e0b062")


class Svg:
    def __init__(self):
        self.out = []

    def text(self, x, y, s, size=12, fill=FG, weight=400, anchor="start", font=SANS, extra=""):
        self.out.append(f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" font-weight="{weight}" '
                        f'fill="{fill}" text-anchor="{anchor}"{extra}>{escape(s)}</text>')

    def rect(self, x, y, w, h, fill, stroke, dashed=False, rx=6):
        dash = ' stroke-dasharray="5 4"' if dashed else ""
        self.out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}" '
                        f'stroke="{stroke}" stroke-width="1.2"{dash}/>')

    def line(self, d, color=ACCENT, width=1.4, dashed=False):
        dash = ' stroke-dasharray="4 4"' if dashed else ""
        self.out.append(f'<path d="{d}" fill="none" stroke="{color}" stroke-width="{width}"{dash}/>')

    def head(self, x, y, direction="right", color=ACCENT):
        pts = {"right": f"{x - 8},{y - 4.5} {x - 8},{y + 4.5} {x},{y}",
               "left": f"{x + 8},{y - 4.5} {x + 8},{y + 4.5} {x},{y}",
               "down": f"{x - 4.5},{y - 8} {x + 4.5},{y - 8} {x},{y}"}[direction]
        self.out.append(f'<polygon points="{pts}" fill="{color}"/>')

    def chip(self, x, y, label, color=ACCENT):
        w = 12 + 6.6 * len(label)
        self.rect(x, y, round(w, 1), 18, BOX, SOLID, rx=9)
        self.text(round(x + w / 2, 1), y + 13, label, 10.5, color, 600, "middle", MONO)
        return w

    def save(self, path, h, title):
        svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {h}" width="{W}" height="{h}" '
               f'role="img" aria-label="{escape(title)}"><title>{escape(title)}</title>'
               f'<rect width="{W}" height="{h}" rx="12" fill="{BG}"/>' + "".join(self.out) + "</svg>\n")
        Path(path).write_text(svg)
        print(Path(path).name, W, h)


out = Path(sys.argv[1])

# ---------------------------------------------------------------- capabilities
s = Svg()
s.rect(24, 24, 420, 360, PANEL, SOLID, rx=10)
s.text(44, 52, "A task (example)", 14.5, FG, 600)
s.text(424, 52, "its capability space (a CNode)", 11, MUTED, 400, "end", MONO)

slots = [
    ("Untyped", ["RETYPE"]),
    ("CNode", ["READ", "WRITE"]),
    ("Endpoint", ["SEND", "RECV", "GRANT"]),
    ("Endpoint", ["SEND"]),
    ("Notification", ["SEND"]),
    ("Frame", ["READ", "WRITE", "MAP"]),
    (None, []),
]
SY, SH = 72, 34
slot_mid = []
for i, (kind, rights) in enumerate(slots):
    y = SY + i * (SH + 6)
    s.rect(84, y, 344, SH, BOX if kind else PANEL, SOLID if kind else LINE, dashed=not kind)
    s.text(100, y + 22, f"{i}", 12, MUTED, 500, font=MONO)
    if kind:
        s.text(122, y + 22, kind, 12.5, FG, 600)
        x = 418
        for r in reversed(rights):
            w = 12 + 6.6 * len(r)
            x -= w + 6
            s.chip(round(x, 1), y + 8, r, OK)
        if i == 3:
            s.text(222, y + 22, "badge 7", 11, MUTED, 400, font=MONO)
    else:
        s.text(122, y + 22, "empty slot", 12, MUTED)
    slot_mid.append(y + SH / 2)

# Derivation: slot 3 was copied from slot 2 with fewer rights.
s.line(f"M 84 {slot_mid[2]} C 52 {slot_mid[2]}, 52 {slot_mid[3]}, 82 {slot_mid[3]}", WARN, 1.6)
s.head(84, slot_mid[3], "right", WARN)
s.text(44, 408, "Slot 3 was derived from slot 2: SEND only, badged. Deriving can only remove rights, never add them.", 11.5, WARN)

# Kernel objects.
s.rect(476, 24, 380, 360, PANEL, SOLID, rx=10)
s.text(496, 52, "Kernel objects", 14.5, FG, 600)
s.text(836, 52, "physical memory, via the HHDM", 11, MUTED, 400, "end", MONO)
objs = {}
OX, OW = 496, 290
y = 72
s.rect(OX, y, OW, 74, BOX, SOLID)
s.text(OX + 14, y + 22, "Untyped", 12.5, FG, 600)
s.text(OX + 14, y + 40, "a region of free RAM, bump-allocated", 11.5, MUTED)
s.rect(OX + 14, y + 50, OW - 28, 10, PANEL, SOLID, rx=3)
s.rect(OX + 14, y + 50, (OW - 28) * 0.42, 10, ACCENT, ACCENT, rx=3)
s.text(OX + OW - 14, y + 70, "watermark", 10, MUTED, 400, "end", MONO)
objs["Untyped"] = y + 37
y += 74 + 14
for kind, note in [("CNode", "slots holding capabilities"), ("Endpoint", "synchronous IPC rendezvous (#81)"),
                   ("Notification", "asynchronous signal word"), ("Frame", "one physical page, mappable")]:
    s.rect(OX, y, OW, 44, BOX, SOLID)
    s.text(OX + 14, y + 19, kind, 12.5, FG, 600)
    s.text(OX + 14, y + 35, note, 11, MUTED)
    objs[kind] = y + 22
    y += 44 + 10

# Retype rail: every object below Untyped was carved out of it.
rx = OX + OW + 22
s.line(f"M {OX + OW} {objs['Untyped']} H {rx} V {objs['Frame']}", ACCENT, 1.4)
for k in ["CNode", "Endpoint", "Notification", "Frame"]:
    s.line(f"M {rx} {objs[k]} H {OX + OW + 9}", ACCENT, 1.4)
    s.head(OX + OW + 1, objs[k], "left")
s.text(rx + 14, (objs["Untyped"] + objs["Frame"]) / 2, "retype: carve + zero", 11, ACCENT, 600, "middle", MONO,
       extra=f' transform="rotate(90 {rx + 14} {(objs["Untyped"] + objs["Frame"]) / 2})"')

# Capability → object references.
targets = ["Untyped", "CNode", "Endpoint", "Endpoint", "Notification", "Frame"]
for i, t in enumerate(targets):
    y0, y1 = slot_mid[i], objs[t]
    s.line(f"M 428 {y0} C 462 {y0}, 462 {y1}, {OX - 2} {y1}", "#5c6b85", 1.2)
    s.head(OX, y1, "right", "#5c6b85")

# Invocation band.
y = 424
s.rect(24, y, W - 48, 58, "#18213a", ACCENT, rx=8)
s.text(44, y + 24, "invoke(cspace, cap, method)", 13, ACCENT, 600, font=MONO)
s.text(44, y + 44, "The kernel looks the capability up in the caller's space, checks it is the right kind and holds the "
       "method's rights, then runs it.", 11.5, MUTED)
s.text(W - 44, y + 24, "no capability, no access", 11.5, FG, 600, "end")
s.save(out / "capabilities.svg", y + 58 + 24, "Magnetite capability model: a task's capability space, kernel objects, retype, attenuation and invocation")

# ---------------------------------------------------------------- memory layout
s = Svg()
s.text(24, 44, "Virtual address space", 14.5, FG, 600)
s.text(W - 24, 44, "high addresses at the top · not to scale", 11, MUTED, 400, "end", MONO)
AX, BX, BW = 24, 214, 512  # address column, band x, band width
CX = BX + BW + 16          # permissions column

rows = [
    # (start, end, title, note, perms, style, height)
    ("0xffffffff80000000", "top", "Kernel image", "randomized base in the top 2 GiB (KASLR)", [], "kernel", 112),
    ("0xffffff8000000000", "0xffffffff80000000", "Kernel heap", "grows up on demand, capped below the KASLR window", ["RW", "NX"], "kernel", 48),
    ("0xfffffe0000000000", "+4 KiB", "LAPIC registers", "one uncached page for the timer", ["RW", "NX", "UC"], "kernel", 44),
    ("0xffff800000000000", "", "HHDM", "direct map of all physical RAM", ["RW", "NX"], "kernel", 48),
    ("0x0000800000000000", "0xffff7fffffffffff", "Non-canonical hole", "any access raises #GP", [], "hole", 40),
    ("0x0000000000001000", "0x00007fffffffffff", "User space", "one per task; today a ring-3 stub at 0x40000000", ["U"], "user", 56),
    ("0x0000000000000000", "0x0000000000000fff", "Unmapped", "catches NULL dereferences", [], "hole", 44),
]
y = 64
kernel_top = y
for start, end, title, note, perms, style, h in rows:
    dashed = style == "hole"
    fill = PANEL if dashed else ("#18213a" if style == "user" else BOX)
    stroke = LINE if dashed else (ACCENT if style == "user" else SOLID)
    s.rect(BX, y, BW, h, fill, stroke, dashed, rx=4)
    s.text(BX + 14, y + 20, title, 12.5, MUTED if dashed else FG, 600)
    s.text(BX + 14, y + 37, note, 11, MUTED)
    s.text(AX, y + h - 6 if start != "0xffffffff80000000" else y + h - 6, start, 11, MUTED, 400, font=MONO)
    if title == "Kernel image":
        sy = y + 46
        for sec, p in [(".text", ["R", "X"]), (".rodata", ["R", "NX"]), (".data .bss", ["RW", "NX"])]:
            s.rect(BX + 14, sy, BW - 28, 18, PANEL, SOLID, rx=3)
            s.text(BX + 24, sy + 13, sec, 11, FG, 500, font=MONO)
            x = BX + BW - 22
            for c in reversed(p):
                w = 12 + 6.6 * len(c)
                x -= w + 4
                s.chip(round(x, 1), sy, c, OK)
            sy += 21
        s.text(CX, y + 20, "W^X: no page is both", 11.5, OK, 600)
        s.text(CX, y + 36, "writable and executable,", 11.5, MUTED)
        s.text(CX, y + 52, "verified at boot", 11.5, MUTED)
    x = CX
    for c in perms:
        x += s.chip(x, y + 10, c, OK if c != "U" else ACCENT) + 6
    if title == "HHDM":
        kernel_bottom = y + h
    y += h + 8

# Bracket the kernel half.
bx = BX - 14
s.line(f"M {bx + 6} {kernel_top} H {bx} V {kernel_bottom} H {bx + 6}", ACCENT, 1.4)
s.text(bx - 8, (kernel_top + kernel_bottom) / 2, "kernel half: same in every address space", 11, ACCENT, 600, "middle",
       MONO, extra=f' transform="rotate(-90 {bx - 8} {(kernel_top + kernel_bottom) / 2})"')
s.save(out / "memory-layout.svg", y + 16, "Magnetite virtual address space: kernel image, heap, LAPIC, HHDM, hole, user space")
