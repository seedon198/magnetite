# 0015: Kernel KASLR via a position-independent image

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

M2 (#47) randomizes the kernel's load address each boot (KASLR), so an attacker
cannot predict the addresses of kernel code and data. Our kernel was linked at a
fixed higher-half base (`0xffffffff80000000`) with `relocation-model=static`.

The Limine protocol can slide a **position-independent executable (PIE,
`ET_DYN`)** to a random base and apply its relocations itself - we do not have to
process relocations in the kernel. The work is therefore: make the kernel a PIE,
and turn the bootloader feature on.

## Decision

**Build the kernel as a PIE and let Limine relocate it.**

- `relocation-model=pic`, linked `-pie --no-dynamic-linker`, producing an
  `ET_DYN` image whose absolute references become `R_X86_64_RELATIVE` dynamic
  relocations. `-z notext` permits those relocations in read-only sections
  (static pointer tables such as the IDT stubs live in `.rodata`); Limine
  applies them at load, before we re-protect `.rodata` (#40).
- The linker script gains a `PT_DYNAMIC` segment and keeps `.dynamic` and
  `.rela.dyn` in loadable segments so Limine can find `DT_RELA` and relocate.
- `boot/limine.conf` sets `kaslr: yes` (and `randomise_hhdm_base: yes`).
- The kernel logs its **slide** (`kernel_virt_base - 0xffffffff80000000`) so a
  backtrace address can be mapped back to the link base for `llvm-addr2line`
  (subtract the slide first).

This composes with the rest of M2 at no extra cost: W^X keys off linker symbols
(which relocate with the image), and the HHDM is always read dynamically from
`boot_info.hhdm_offset`.

## Consequences

- The kernel base is randomized per boot (verified in QEMU across boots); the
  whole kernel - paging, W^X/NX, guard pages, heap, #PF diagnostics, backtrace -
  runs correctly at the slid base.
- Backtrace addresses include the slide; the logged `kaslr: slide=…` line makes
  symbolication a subtraction. (Fully slide-aware in-kernel symbolization would
  need a symbol table; out of scope.)
- **The HHDM base is randomized too** (#70), in 1 GiB steps over a quarter of
  the higher half. It first seemed tied to a newer protocol revision, but the
  cause was elsewhere: Limine reads `randomise_hhdm_base` only while handling a
  paging-mode request, and the kernel made none. It now requests 4-level
  paging (`PagingModeRequest`), which it assumes anyway, and the HHDM base
  changes on every boot under UEFI and BIOS alike.
- `-z notext` means some relocations sit in `.rodata`; they are applied by
  Limine before handoff and the section is then re-protected read-only, so there
  is no writable-executable window.
