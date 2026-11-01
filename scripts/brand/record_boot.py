"""Record a real UEFI boot in QEMU as a themed asciicast for the README.

Boots build/kernel.iso, timestamps every serial line as it arrives, stops at
"kernel: idling", and writes an asciicast v2 file. Firmware chatter before the
boot banner is dropped. Lines are coloured like the other README images: the
tile in the accent colour, summary keys dimmed, log tags green, amber or red.
The file uses only ANSI indices; boot_gif.sh's agg theme maps them to the
brand palette. Real timing is kept, but each pause is
clamped so the recording stays short.

Usage: record_boot.py <out.cast>
"""
import glob
import json
import re
import subprocess
import sys
import time

out = sys.argv[1]
ovmf = sorted(glob.glob("/opt/homebrew/Cellar/qemu/*/share/qemu/edk2-x86_64-code.fd")
              + glob.glob("/usr/share/OVMF/OVMF_CODE*.fd"))[0]
qemu = subprocess.Popen(
    ["qemu-system-x86_64", "-M", "q35", "-m", "256M",
     "-drive", f"if=pflash,unit=0,format=raw,file={ovmf},readonly=on",
     "-cdrom", "build/kernel.iso", "-display", "none", "-serial", "stdio", "-no-reboot"],
    stdout=subprocess.PIPE, stdin=subprocess.DEVNULL)

ansi = re.compile(r"\x1b\[[0-9;=?]*[A-Za-z]")
timed = []  # (seconds since the banner, line)
t0 = None
deadline = time.monotonic() + 90
for raw in qemu.stdout:
    line = ansi.sub("", raw.decode("utf-8", "replace")).rstrip()
    now = time.monotonic()
    if t0 is None and line.strip().startswith(".---"):
        t0 = now
    if t0 is not None:
        timed.append((now - t0, line))
        if "kernel: idling" in line:
            break
    if now > deadline:
        break
qemu.kill()
if not timed or "kernel: idling" not in timed[-1][1]:
    sys.exit("record_boot: the boot did not reach `kernel: idling`")

# ANSI indices; boot_gif.sh's agg theme maps them to the brand palette.
FG, RED, GREEN, YELLOW, ACCENT, DIM, BRIGHT = "37", "31", "32", "33", "34", "90", "97"
TAG = {"[INFO]": GREEN, "[WARN]": YELLOW, "[ERROR]": RED}
TILE_W = 30


def paint(text, colour):
    return f"\x1b[{colour}m{text}\x1b[0m"


def colour(l):
    if l.startswith("  ") and l.strip()[:1] in ".|'":
        right = l[TILE_W:]
        return paint(l[:TILE_W], ACCENT) + paint(right, BRIGHT if right.strip()[:1] in "_|" else FG)
    if re.match(r"  \w+ {2,}", l):
        return paint(l[:13], DIM) + paint(l[13:], FG)
    m = re.match(r"(\[\w+\]) (.*)", l)
    if m:
        return paint(f"{m.group(1):<7} ", TAG.get(m.group(1), DIM)) + paint(m.group(2), FG)
    return paint(l, FG)


cols = max(len(l) for _, l in timed) + 2
events, t = [], 0.6
prompt = "$ cargo xtask run"
events.append([0.3, "o", "\x1b[?25l" + paint("$ ", DIM)])  # hide the cursor
for ch in prompt[2:]:
    t += 0.06
    events.append([round(t, 3), "o", paint(ch, DIM)])
t += 0.5
events.append([round(t, 3), "o", "\r\n"])
t += 1.0  # firmware and bootloader, compressed
prev = 0.0
for when, l in timed:
    t += min(max(when - prev, 0.025), 0.8)
    prev = when
    events.append([round(t, 3), "o", colour(l) + "\r\n"])
events.append([round(t + 3.0, 3), "o", ""])  # hold the last frame

header = {"version": 2, "width": cols, "height": 34,
          "title": "Magnetite booting in QEMU (UEFI, serial console)"}
with open(out, "w") as f:
    f.write(json.dumps(header) + "\n")
    for e in events:
        f.write(json.dumps(e) + "\n")
print(f"{len(timed)} lines, {cols} columns, {t:.1f}s")
