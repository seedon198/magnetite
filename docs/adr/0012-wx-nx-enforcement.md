# 0012: Enforce W^X and NX ourselves (kernel image + HHDM)

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

With the kernel owning its page tables (#39, ADR 0011), M2 (#40) enforces two
foundational hardening invariants rather than trusting the bootloader:

- **W^X** - no mapping is simultaneously writable and executable.
- **NX** - only code is executable; all data (including the direct physical
  map) is non-executable.

The bootloader (Limine) already applies reasonable per-section permissions, but
a security kernel must set and *verify* its own policy. Open questions: where to
get the kernel's section boundaries, how to apply fine-grained permissions given
the inherited mappings, how to make the multi-GiB HHDM non-executable cheaply,
and whether KASLR belongs in this change.

## Decision

**Section boundaries from linker symbols.** `linker.ld` already page-aligns
each output section and exports `__text_start/end`, `__rodata_start/end`,
`__data_start/end`. The boot module reads these symbols (only taking their
addresses) and hands the ranges to the memory manager. No ELF parsing.

**Apply permissions in place with `protect`.** Limine maps the kernel image at
4 KiB granularity, so instead of rebuilding its page tables we walk to each
existing 4 KiB leaf and rewrite its flags (`AddressSpace::protect`), keeping the
physical target: `.text` → R-X, `.rodata` → R+NX, `.data`/`.bss` → RW+NX. If a
kernel page were found mapped by a large page (it is not, in practice) it is
counted and skipped rather than mis-permissioned; boot verification would catch
the resulting gap.

**HHDM non-executable at the top level.** The NX bit is cumulative across all
paging levels, so setting NX on the HHDM's PML4 entries makes the entire direct
physical map non-executable in one cheap step, without rebuilding its large-page
tables. This closes the "execute from the physmap alias" primitive. `CR3` is
reloaded afterwards to flush the TLB.

**The HHDM alias of the image is read-only (added for #119).** W^X per
mapping is not enough: the HHDM maps the kernel image's frames read-write a
second time, so a write through `hhdm + phys` would change code that then runs
at the normal `.text` address. After hardening, the kernel splits the HHDM huge
pages covering the image (`AddressSpace::split_huge`, using the pure
`page_table::split_huge`) and makes the alias of every `.text` and `.rodata`
page read-only and NX. It is read-only rather than unmapped so kernel reads
through the HHDM keep working. Verification checks every alias page.

**Enable `EFER.NXE`** before any of this, so the NX page-flag is honored.

**Verify, don't assume.** After applying, the kernel reads back the actual
flags - `.text` is executable and not writable, `.data` is NX, the HHDM's
top-level entry is NX - and asserts the invariants, logging a summary.

**KASLR is out of scope here** (#47). Real KASLR requires a position-independent
kernel and relocation handling - a cross-cutting build change kept in its own
issue so this one stays small and low-risk.

## Consequences

- The kernel enforces and verifies W^X/NX itself; a kernel bug that corrupts a
  code pointer cannot turn writable data into executable code, and the physmap can't
  be used as an execution surface.
- Relies on Limine's 4 KiB kernel mapping for in-place `protect`; if that ever
  changes, the huge-page counter surfaces it. Page splitting exists now (#119)
  and could be applied there too.
- Splitting the HHDM around the image costs a few page-table frames (one per
  2 MiB page split, plus one per 1 GiB page split) for the life of the kernel.
- NX on the HHDM is set at the PML4 level; verification therefore checks the
  top-level entry, not the (unchanged) huge leaf.
- A *negative* test (actually faulting on an NX/W^X violation) depends on the
  page-fault handler not halting; it arrives with #42 (page-fault diagnostics).
  Until then, verification is by flag inspection.
- `unmap` reclaims the intermediate tables it empties (ADR 0011, #64).
