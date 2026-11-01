"""Draw the step-by-step flow diagrams used in the docs. Usage: flow_svg.py <out_dir>

Writes dev-toolchain.svg, interrupt-flow.svg and timer-preemption.svg. Same
palette and type as arch_svg.py and boot_svg.py: each stage is a card, joined
by arrows that may carry a label (the command or event that moves you on).
"""
import sys
from pathlib import Path
from xml.sax.saxutils import escape

W = 880
SANS = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
BG, PANEL, BOX, LINE, SOLID, FG, MUTED, ACCENT = (
    "#0f1218", "#161b23", "#1d2330", "#3a4250", "#4a5568", "#e6e9ef", "#9198a1", "#7fa6e8")


class Flow:
    def __init__(self, title):
        self.title, self.out, self.y = title, [], 24

    def text(self, x, y, s, size=12, fill=FG, weight=400, anchor="start", font=SANS):
        self.out.append(f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" '
                        f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{escape(s)}</text>')

    def rect(self, x, y, w, h, fill, stroke, dashed=False, rx=8):
        dash = ' stroke-dasharray="5 4"' if dashed else ""
        self.out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}" '
                        f'stroke="{stroke}" stroke-width="1.2"{dash}/>')

    def card(self, title, where="", bullets=(), mono_title=True, numbered=False):
        """A stage. Bullets are plain strings, or (code, note) pairs where the
        code is set in monospace and the note follows in muted text."""
        y, h = self.y, 44 + 21 * len(bullets) - (8 if not bullets else 0)
        self.rect(24, y, W - 48, h, PANEL, SOLID)
        self.text(44, y + 27, title, 14 if mono_title else 14.5, FG, 600, font=MONO if mono_title else SANS)
        if where:
            self.text(W - 44, y + 27, where, 11, MUTED, 400, "end", MONO)
        for i, b in enumerate(bullets):
            by = y + 51 + 21 * i
            mark = f"{i + 1}" if numbered else "·"
            self.text(60, by, mark, 11 if numbered else 13, ACCENT, 700, "end", MONO if numbered else SANS)
            code, note = b if isinstance(b, tuple) else ("", b)
            x = 70
            if code:
                self.text(x, by, code, 12, FG, 400, font=MONO)
                x += 7.25 * len(code) + 10
            if note:
                self.text(x, by, note, 12, MUTED)
        self.y += h

    def arrow(self, label=""):
        x, y0, y1 = 64, self.y, self.y + 40
        self.out.append(f'<line x1="{x}" y1="{y0}" x2="{x}" y2="{y1 - 7}" stroke="{ACCENT}" stroke-width="1.6"/>')
        self.out.append(f'<polygon points="{x - 5},{y1 - 8} {x + 5},{y1 - 8} {x},{y1}" fill="{ACCENT}"/>')
        if label:
            self.text(x + 16, y0 + 24, label, 12, ACCENT, 500, font=MONO)
        self.y = y1

    def save(self, path):
        H = self.y + 24
        svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
               f'role="img" aria-label="{escape(self.title)}"><title>{escape(self.title)}</title>'
               f'<rect width="{W}" height="{H}" rx="12" fill="{BG}"/>' + "".join(self.out) + "</svg>\n")
        Path(path).write_text(svg)
        print(path.name, W, H)


out = Path(sys.argv[1])

# Development: from source to a running kernel.
f = Flow("Magnetite toolchain: Rust crates to kernel ELF to ISO to QEMU")
f.card("Rust crates", "kernel, hal, libs/*", mono_title=False)
f.arrow("cargo build --target x86_64-unknown-none")
f.card("kernel ELF", "higher-half PIE", mono_title=False)
f.arrow("cargo xtask iso   (Limine + xorriso)")
f.card("kernel.iso", "boots under UEFI and BIOS", mono_title=False)
f.arrow("cargo xtask run")
f.card("qemu-system-x86_64", "q35 · OVMF for UEFI", [
    ("serial", "output goes straight to your terminal"),
    (":1234", "GDB stub with cargo xtask debug, for gdb or lldb"),
])
f.save(out / "dev-toolchain.svg")

# Interrupts: from the CPU raising a vector to iretq.
f = Flow("Interrupt and exception dispatch path")
f.card("CPU raises vector V", "hardware", [
    ("", "pushes SS, RSP, RFLAGS, CS, RIP, plus an error code for some vectors"),
], mono_title=False)
f.arrow()
f.card("vector_stub_V", "generated asm, one per vector", [
    ("push 0", "as a dummy error code if the CPU did not push one"),
    ("push V", "then jmp common_stub"),
])
f.arrow()
f.card("common_stub", "one shared asm routine", [
    ("push 15 GPRs", "the stack now holds a full TrapFrame"),
    ("rdi = rsp", "(&mut TrapFrame), align rsp to 16 bytes, call idt_dispatch"),
    ("on return", "restore GPRs, drop vector and error code, iretq"),
])
f.arrow()
f.card("idt_dispatch(&mut TrapFrame)", "Rust · hal::idt", [
    ("HANDLERS[vector]", "set: call it"),
    ("otherwise", "default_exception() prints a diagnostic report"),
])
f.arrow("handler returns")
f.card("iretq", "back in common_stub", [
    ("", "registers restored from the TrapFrame; the interrupted code resumes"),
])
f.save(out / "interrupt-flow.svg")

# Preemption: one LAPIC timer tick.
f = Flow("Timer interrupt and preemption path")
f.card("timer IRQ, vector 0x30", "LAPIC · every 10 ms", [
    ("", "common_stub builds a TrapFrame on the current thread's stack"),
])
f.arrow()
f.card("timer_interrupt", "kernel/src/main.rs", [
    ("TICKS += 1", ""),
    ("lapic::eoi()", "before switching: the next thread may run a whole slice first"),
    ("ring 3 too", "a tick in user mode preempts the same way: each thread has its own kernel stack"),
    ("pick_next()", "SCHED.lock(), rotate round-robin; the lock is released here"),
    ("context::switch(cur, next)", "cur's TrapFrame stays on its own stack"),
], numbered=True)
f.arrow("other threads run; a later tick picks cur again")
f.card("resume cur", "", [
    ("switch returns", "into the handler, which returns to common_stub"),
    ("iretq", "restores the TrapFrame; cur continues where it was interrupted"),
], mono_title=False)
f.save(out / "timer-preemption.svg")
