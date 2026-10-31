"""Draw the Magnetite boot sequence diagram. Usage: boot_svg.py <out.svg>

Same palette and type as arch_svg.py. Each stage is a card; kmain's steps are
listed in the order kmain runs them, tagged with the milestone that added them.
"""
import sys
from xml.sax.saxutils import escape

W = 880
SANS = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
BG, PANEL, BOX, LINE, SOLID, FG, MUTED, ACCENT = (
    "#0f1218", "#161b23", "#1d2330", "#3a4250", "#4a5568", "#e6e9ef", "#9198a1", "#7fa6e8")

out = []


def text(x, y, s, size=12, fill=FG, weight=400, anchor="start", font=SANS):
    out.append(f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" '
               f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{escape(s)}</text>')


def rect(x, y, w, h, fill, stroke, dashed=False, rx=8):
    dash = ' stroke-dasharray="5 4"' if dashed else ""
    out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}" '
               f'stroke="{stroke}" stroke-width="1.2"{dash}/>')


def arrow(y0, y1):
    x = W / 2
    out.append(f'<line x1="{x}" y1="{y0}" x2="{x}" y2="{y1 - 7}" stroke="{ACCENT}" stroke-width="1.6"/>')
    out.append(f'<polygon points="{x - 5},{y1 - 8} {x + 5},{y1 - 8} {x},{y1}" fill="{ACCENT}"/>')


def card(y, title, where, bullets, dashed=False):
    h = 44 + 19 * len(bullets)
    rect(24, y, W - 48, h, PANEL, LINE if dashed else SOLID, dashed)
    text(44, y + 27, title, 14.5, MUTED if dashed else FG, 600)
    text(W - 44, y + 27, where, 11, MUTED, 400, "end", MONO)
    for i, b in enumerate(bullets):
        text(56, y + 50 + 19 * i, "·", 13, ACCENT, 700)
        text(68, y + 50 + 19 * i, b, 12, MUTED)
    return y + h


GAP = 34
y = 24
y = card(y, "Firmware", "UEFI or legacy BIOS", [
    "loads Limine from the USB stick or ISO",
])
arrow(y, y + GAP); y += GAP
y = card(y, "Limine", "boot/limine.conf", [
    "loads the kernel ELF (a PIE) at a randomized higher-half base: KASLR",
    "enables long mode and 4-level paging, maps all RAM at the HHDM offset",
    "answers the kernel's requests: memory map, HHDM, kernel address, framebuffer, bootloader, firmware",
])
arrow(y, y + GAP); y += GAP
y = card(y, "_start", "kernel/src/boot/mod.rs", [
    "checks the Limine base revision, halts if unsupported",
    "copies every response into a bootloader-neutral BootInfo",
])
arrow(y, y + GAP); y += GAP

# kmain: steps in run order, two columns, each tagged with its milestone.
steps = [
    ("M1", "serial and framebuffer log sinks"),
    ("M4", "boot banner and system summary"),
    ("M1", "GDT, TSS and IDT"),
    ("M2", "require NX and SSE2; enable SMEP and SMAP"),
    ("M2", "bitmap frame allocator"),
    ("M2", "own page tables (PML4)"),
    ("M2", "W^X and NX hardening, self-verified"),
    ("M2", "stack guard pages, kernel heap"),
    ("M2", "page-fault diagnostics"),
    ("M3", "Untyped retype, CNode capability space"),
    ("M3", "rights-checked capability invocation"),
    ("M4", "threads, round-robin context switch"),
    ("M4", "ring 3 entry and the syscall boundary"),
    ("M4", "LAPIC timer: preemptive scheduling"),
    ("M4", "synchronous IPC between kernel threads"),
]
rows = (len(steps) + 1) // 2
h = 56 + 24 * rows + 30
rect(24, y, W - 48, h, PANEL, SOLID)
text(44, y + 27, "kmain(&BootInfo)", 14.5, FG, 600, font=MONO)
text(W - 44, y + 27, "kernel/src/main.rs  ·  #![deny(unsafe_code)]", 11, MUTED, 400, "end", MONO)
col_w = (W - 88) / 2
for i, (m, s) in enumerate(steps):
    col, row = divmod(i, rows)
    x = 44 + col * col_w
    yy = y + 52 + 24 * row
    text(x + 16, yy + 13.5, f"{i + 1}", 11, MUTED, 400, "end", MONO)
    rect(x + 24, yy, 30, 18, BOX, SOLID, rx=9)
    text(x + 39, yy + 13, m, 10.5, ACCENT, 600, "middle", MONO)
    text(x + 64, yy + 13.5, s, 12, FG)
text(44, y + h - 16, "ends with one ready: line listing every protection in force, then idles on the timer",
     11.5, MUTED, 400, font=MONO)
y += h
arrow(y, y + GAP); y += GAP

y = card(y, "Root server", "user space · planned (M5)", [
    "first user task, holding only the capabilities it was granted (#82)",
    "brings up drivers, network and filesystems as servers over IPC",
], dashed=True)

y += 18
rect(24, y + 4, 26, 14, PANEL, SOLID, rx=3)
text(58, y + 15, "runs today", 11.5, MUTED)
rect(150, y + 4, 26, 14, PANEL, LINE, True, rx=3)
text(184, y + 15, "planned", 11.5, MUTED)
rect(262, y + 3, 30, 16, BOX, SOLID, rx=8)
text(277, y + 15, "M2", 10, ACCENT, 600, "middle", MONO)
text(302, y + 15, "milestone that added the step", 11.5, MUTED)

H = int(y + 34)
svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" '
       f'aria-label="Magnetite boot sequence: firmware, Limine, _start, kmain, root server">'
       f'<title>Magnetite boot sequence</title><rect width="{W}" height="{H}" rx="12" fill="{BG}"/>'
       + "".join(out) + "</svg>\n")
open(sys.argv[1], "w").write(svg)
print(W, H)
