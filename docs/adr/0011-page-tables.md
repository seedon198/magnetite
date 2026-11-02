# 0011: Own the page tables - inherit the higher half, walk via the HHDM

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

M2 (#39) must put the kernel on page tables it controls, with an
`AddressSpace` abstraction (`map` / `unmap` / `translate`) that later
milestones build on: W^X / NX / KASLR (#40), guard pages and the heap (#41),
and per-process user address spaces (M4). The bootloader (Limine) already
installed working 4-level page tables - the kernel mapped higher-half at
`0xffffffff80000000`, and the whole of physical memory direct-mapped in the
Higher-Half Direct Map (HHDM), using large pages.

Two questions shape the design:

1. How much do we rebuild in #39 - a fresh mapping of everything, or inherit
   the working higher-half mappings and own the root?
2. How does the page-table walker read and write page tables, which themselves
   live in physical frames?

## Decision

**1. Own the root PML4, inherit the higher half.** Each `AddressSpace`
allocates its own PML4 from the frame allocator (#38). The kernel address space
is built by copying Limine's **higher-half** top-level entries (PML4 indices
256..512 - the kernel image and the HHDM) into our PML4, then switching `CR3`
to it. The lower half (indices 0..256) starts empty and is ours to manage at
4 KiB granularity via `map`/`unmap`. Everything the running kernel touches -
its image, stacks, the framebuffer, and all physical access - is in the higher
half, so the `CR3` switch is seamless.

A strict, per-section rebuild of the kernel image mapping with correct
permissions (`.text` R-X, `.rodata` R, `.data`/`.bss` RW+NX) and KASLR is
deliberately **deferred to #40**; #39 establishes ownership of the root and the
mapping API without pulling that work forward.

**2. Walk via the HHDM.** To read or write a page table at physical address
`P`, the walker accesses it at `P + hhdm_offset` - the same mechanism the frame
allocator already uses. No recursive-mapping PML4 slot to reserve or maintain,
and no per-address-space bookkeeping for it. The HHDM is read-write today; it
will be marked NX in #40 so page-table memory can never be executed.

**Page size:** 4 KiB only for new mappings, consistent with ADR 0010. The
inherited higher-half large pages are left as Limine built them. `translate`
understands huge-page entries (so it reads inherited mappings correctly) but
`map` only creates 4 KiB leaves.

## Consequences

- The kernel runs on its own PML4 from #39 on; new mappings are fully ours.
- Inherited higher-half entries still point at Limine's lower-level tables and
  keep Limine's (permissive) permissions until #40 rebuilds the kernel image
  mapping - an accepted, time-boxed gap, not the end state.
- The walker depends on the HHDM being present (it is, from Limine) and
  correct; this is already a load-bearing assumption of the frame allocator.
- `unmap` clears the leaf and then frees each intermediate table it leaves
  empty, from the page table up (#64). It never frees the root, or a
  kernel-half PDPT, whose top-level entry every user space copies.
- Pure entry/flag/address math lives in the `page-table` crate
  (`#![forbid(unsafe_code)]`, host-tested); the `unsafe` walk and the `CR3` /
  `invlpg` instructions live in `hal::paging`.
- Exposing `AddressSpace::switch` as a safe method relies on the invariant that
  every `AddressSpace` maps the kernel higher half; this holds by construction
  (every one is seeded from the kernel higher half).
