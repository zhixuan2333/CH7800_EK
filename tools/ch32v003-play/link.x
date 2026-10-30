/* CH32V003: 16 KiB flash executed from its 0x0 alias, 2 KiB SRAM. */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 16K
  RAM   : ORIGIN = 0x20000000, LENGTH = 2K
}

ENTRY(_start)

SECTIONS
{
  .text : {
    KEEP(*(.init))
    *(.text .text.*)
  } > FLASH

  .rodata : ALIGN(4) {
    *(.srodata .srodata.*)
    *(.rodata .rodata.*)
    . = ALIGN(4);
  } > FLASH

  .data : ALIGN(4) {
    _sdata = .;
    *(.sdata .sdata.*)
    *(.data .data.*)
    . = ALIGN(4);
    _edata = .;
  } > RAM AT > FLASH
  _sidata = LOADADDR(.data);

  .bss (NOLOAD) : ALIGN(4) {
    _sbss = .;
    *(.sbss .sbss.*)
    *(.bss .bss.*)
    . = ALIGN(4);
    _ebss = .;
  } > RAM

  _stack_top = ORIGIN(RAM) + LENGTH(RAM);

  /DISCARD/ : { *(.eh_frame*) }
}
