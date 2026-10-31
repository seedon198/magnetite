"""Draw the Magnetite architecture diagram. Usage: arch_svg.py <out.svg>"""
import sys
from xml.sax.saxutils import escape

W = 880
SANS = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
BG, PANEL, BOX, LINE, FG, MUTED, ACCENT, OK = "#0f1218", "#161b23", "#1d2330", "#3a4250", "#e6e9ef", "#9198a1", "#7fa6e8", "#7fb88a"

out = []


def text(x, y, s, size=12, fill=FG, weight=400, anchor="start", font=SANS):
    out.append(f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{escape(s)}</text>')


def rect(x, y, w, h, fill, stroke, dashed=False, rx=6):
    dash = ' stroke-dasharray="5 4"' if dashed else ""
    out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}" stroke="{stroke}" stroke-width="1.2"{dash}/>')


def layer(y, h, title, note, dashed=False):
    rect(24, y, W - 48, h, PANEL, LINE, dashed, rx=8)
    text(40, y + 24, title, 14, FG, 600)
    text(W - 40, y + 24, note, 11, MUTED, 400, "end", MONO)


def boxes(y, h, items):
    n = len(items)
    gap, x0 = 10, 40
    bw = (W - 80 - gap * (n - 1)) / n
    for i, (title, lines, planned) in enumerate(items):
        x = x0 + i * (bw + gap)
        rect(round(x, 1), y, round(bw, 1), h, BOX, LINE if planned else "#4a5568", planned)
        text(round(x + 12, 1), y + 22, title, 12.5, MUTED if planned else FG, 600)
        for j, l in enumerate(lines):
            text(round(x + 12, 1), y + 41 + j * 16, l, 11.5, MUTED)


y = 24
layer(y, 112, "User space · ring 3", "isolated servers, least privilege")
boxes(y + 38, 62, [
    ("User tasks", ["ring-3 entry works", "first task: #82"], False),
    ("Drivers", ["devmgr, virtio, NIC", "as servers (M5-M6)"], True),
    ("Network stack", ["sandboxed parsing", "(M6)"], True),
    ("Filesystem", ["VFS server", "(M5)"], True),
    ("Security tools", ["scripting runtime,", "native tools (M7)"], True),
])

y = 150
rect(24, y, W - 48, 34, "#18213a", ACCENT, rx=6)
text(W / 2, y + 22, "syscall / sysret  ·  capability invocation  ·  synchronous IPC", 12.5, ACCENT, 600, "middle")

y = 198
layer(y, 150, "Kernel · ring 0", "#![deny(unsafe_code)] · linker glue only in boot/")
boxes(y + 38, 96, [
    ("Capabilities", ["CNode capability space", "Untyped memory + retype", "rights-checked invoke", "attenuate-only rights"], False),
    ("Scheduling", ["threads, context switch", "round-robin policy", "LAPIC timer preemption", ""], False),
    ("Memory", ["frame allocator (bitmap)", "own page tables, HHDM", "W^X, NX, KASLR", "guard pages, heap"], False),
    ("IPC", ["Endpoint send / recv", "call / reply", "capability transfer", "badges"], False),
])

y = 362
layer(y, 112, "hal · the only crate allowed unsafe", "every block: // SAFETY:")
boxes(y + 38, 62, [
    ("CPU tables", ["GDT, TSS, IDT", "exceptions"], False),
    ("Paging", ["page tables", "TLB, CR3"], False),
    ("Context", ["thread switch", "ring-3 entry"], False),
    ("Syscall", ["STAR / LSTAR", "entry stub"], False),
    ("Timers", ["LAPIC, PIC", "PIT calibration"], False),
    ("Console", ["serial 16550", "framebuffer"], False),
])

y = 488
rect(24, y, W - 48, 34, PANEL, LINE, rx=6)
text(W / 2, y + 22, "Limine (UEFI + BIOS)  →  x86-64 hardware  ·  QEMU q35", 12.5, MUTED, 500, "middle")

y = 536
text(24, y + 18, "Pure no_std libraries, unit-tested on the host:", 12, MUTED)
x = 314
for name in ["capability", "sched", "frame-alloc", "page-table", "boot-info"]:
    w = 14 + 7.4 * len(name)
    rect(x, y + 3, round(w, 1), 22, BOX, "#4a5568", rx=11)
    text(round(x + w / 2, 1), y + 18, name, 11.5, OK, 500, "middle", MONO)
    x += w + 8

y = 580
rect(24, y + 4, 26, 14, BOX, "#4a5568", rx=3)
text(58, y + 15, "working on main", 11.5, MUTED)
rect(184, y + 4, 26, 14, BOX, LINE, True, rx=3)
text(218, y + 15, "planned (milestone or issue)", 11.5, MUTED)

H = y + 34
svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" '
       f'aria-label="Magnetite architecture: user space, syscall boundary, kernel, hal, boot">'
       f'<title>Magnetite architecture</title><rect width="{W}" height="{H}" rx="12" fill="{BG}"/>'
       + "".join(out) + "</svg>\n")
open(sys.argv[1], "w").write(svg)
print(W, H)
