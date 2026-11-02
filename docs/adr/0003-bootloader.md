# 0003: Bootloader: Limine

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

An x86-64 PC powers on in 16-bit real mode (BIOS) or 64-bit mode under UEFI.
Reaching a clean long-mode environment with a memory map and framebuffer is a
large sub-project and the least reusable part of an OS. We need to boot from
**USB on generic laptops** as well as in QEMU, under both UEFI and BIOS.

## Options considered

1. **Write our own bootloader**: maximally educational but months of real-mode,
   A20, protected mode, long-mode, UEFI and FAT/USB work before any kernel
   code runs.
2. **GRUB + Multiboot2**: ubiquitous but hands off in 32-bit protected mode, so
   we still write the long-mode trampoline; awkward to build images on macOS.
3. **Limine (Limine boot protocol)**: modern, maintained, hands off in
   **64-bit long mode** with the kernel **already higher-half mapped**, plus
   memory map, HHDM, framebuffer, modules and SMP startup. UEFI **and** BIOS,
   so it boots real laptops from USB. Prebuilt binaries work on macOS.
4. **`rust-osdev/bootloader` crate**: pure-Rust and ergonomic, but UEFI/BIOS
   support and features have historically lagged Limine, and it couples us to
   its evolving API.

## Decision

Use **Limine** with the Limine boot protocol. Pin a specific Limine release;
fetch its binary branch at build time (via `xtask`) rather than vendoring
binaries in git. The pin is a commit on that branch, never its moving head:
`xtask` fetches exactly that commit and, on every build, checks that
`limine/` is still at it with no tracked file modified (#112). Keep the
protocol bindings behind our own `BootInfo` so the
kernel never touches Limine types directly.

## Consequences

- Rust code runs in long mode, higher-half, on day one, on both QEMU and real
  USB-booted hardware.
- We depend on Limine's protocol; isolation behind `BootInfo` lets us add a
  custom or pure-Rust bootloader later (new ADR) without touching the core.
- We still write our own GDT, IDT and page tables and stop relying on Limine's
  after early boot.
