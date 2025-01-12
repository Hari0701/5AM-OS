/* Where a 5AM-OS program lives.
 *
 * Fixed address, no relocation: this kernel has no dynamic loader and the
 * loader it does have reads program headers and copies bytes, nothing more.
 * 0x400000 is the traditional x86 ELF load address and is comfortably clear of
 * the stack the kernel maps lower down.
 *
 * The ALIGN(4096) between sections is what keeps each PT_LOAD segment in its
 * own pages. Without it a read-only and a read-write segment can share a page,
 * and the loader would have to decide which permission wins -- which is a real
 * problem real loaders have, and one worth not having yet.
 */
ENTRY(_start)

SECTIONS {
    . = 0x400000;

    .text : ALIGN(4096) { *(.text .text.*) }
    .rodata : ALIGN(4096) { *(.rodata .rodata.*) }
    .data : ALIGN(4096) { *(.data .data.*) }
    .bss : ALIGN(4096) { *(.bss .bss.*) *(COMMON) }

    /DISCARD/ : { *(.eh_frame) *(.comment) *(.note.*) }
}
