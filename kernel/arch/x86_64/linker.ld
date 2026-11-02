/* Higher-half kernel linker script for x86-64 (see docs/architecture/memory-layout.md
 * and docs/adr/0003-bootloader.md). Modeled on the Limine protocol requirements.
 *
 * The kernel is built as a position-independent executable (PIE) so Limine can
 * slide it to a random base and apply its RELATIVE relocations (KASLR, #47 /
 * ADR 0015). That needs a PT_DYNAMIC segment and the .dynamic / .rela.dyn
 * sections kept and loaded.
 *
 * Each loadable output section gets its own PT_LOAD segment so the kernel can
 * map them with exact W^X permissions: .text is R-X, .rodata is R--, .data/.bss
 * are RW-. Section boundary symbols are exported for the memory manager (M2).
 *
 * RELRO (#126): the relocated-but-then-constant sections (.data.rel.ro,
 * .dynamic, .got, .got.plt) open the data segment, between the page-aligned
 * __relro_start and __relro_end. Limine writes them while relocating, before
 * handoff; harden_memory then makes them read-only. */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text    PT_LOAD    FLAGS((1 << 2) | (1 << 0)) ; /* R + X */
    rodata  PT_LOAD    FLAGS((1 << 2)) ;            /* R     */
    data    PT_LOAD    FLAGS((1 << 2) | (1 << 1)) ; /* R + W */
    dynamic PT_DYNAMIC FLAGS((1 << 2) | (1 << 1)) ; /* R + W - relocations */
}

SECTIONS
{
    /* Topmost 2 GiB of the address space, as the Limine spec mandates. This is
     * the link base; with KASLR, Limine slides the kernel from here each boot. */
    . = 0xffffffff80000000;
    __kernel_start = .;

    .text : {
        __text_start = .;
        *(.text .text.*)
    } :text
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __text_end = .;

    .rodata : {
        __rodata_start = .;
        *(.rodata .rodata.*)
    } :rodata

    /* RELATIVE relocations Limine applies before handoff. Read-only data, so it
     * lives in the rodata segment and under .rodata's permissions. */
    .rela.dyn : {
        *(.rela .rela.*)
    } :rodata
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __rodata_end = .;

    __data_start = .;
    __relro_start = .;
    /* Listed before .data, whose .data.* would otherwise claim .data.rel.ro. */
    .data.rel.ro : {
        *(.data.rel.ro .data.rel.ro.*)
    } :data

    /* Dynamic linking info: loaded (:data) and pointed at by PT_DYNAMIC
     * (:dynamic) so Limine can find DT_RELA and relocate. */
    .dynamic : {
        *(.dynamic)
    } :data :dynamic

    .got : { *(.got .got.*) } :data
    .got.plt : { *(.got.plt .got.plt.*) } :data
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __relro_end = .;

    .data : {
        *(.data .data.*)

        /* Limine scans this range for request magic; the markers bound it. */
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :data

    /* .bss must be last in :data so the on-disk image isn't padded with zeros. */
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __data_end = .;

    __kernel_end = .;

    /* These can trip up some linkers/hosts and we don't use them. */
    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
