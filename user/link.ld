/* Link layout for Magnetite user programs (#82, ADR 0019): a static,
 * non-PIE executable at 0x400000 with text (R X), rodata (R) and data plus
 * bss (RW) in separate page-aligned segments, so the loader maps each with
 * exactly its permissions and never W+X. */
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data   PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x400000;
    .text : { *(.text .text.*) } :text

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(4096);
    .data : { *(.data .data.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
