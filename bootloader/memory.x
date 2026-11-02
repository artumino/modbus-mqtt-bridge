MEMORY
{
  /* NOTE 1 K = 1 KiBi = 1024 bytes */
  BOOT2                             : ORIGIN = 0x10000000, LENGTH = 0x100
  FLASH                             : ORIGIN = 0x10000100, LENGTH = 24K - 0x100
  BOOTLOADER_STATE                  : ORIGIN = 0x10006000, LENGTH = 4K
  ACTIVE                            : ORIGIN = 0x10007000, LENGTH = 1000K
  DFU                               : ORIGIN = 0x10101000, LENGTH = 1004K
  /* Not used by the bootloader, it documents the rest of the chip (see embedded/memory.x) */
  SETTINGS                          : ORIGIN = 0x101FC000, LENGTH = 16K
  RAM                               : ORIGIN = 0x20000000, LENGTH = 256K
}

__bootloader_state_start = ORIGIN(BOOTLOADER_STATE) - ORIGIN(BOOT2);
__bootloader_state_end = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE) - ORIGIN(BOOT2);

__bootloader_active_start = ORIGIN(ACTIVE) - ORIGIN(BOOT2);
__bootloader_active_end = ORIGIN(ACTIVE) + LENGTH(ACTIVE) - ORIGIN(BOOT2);

__bootloader_dfu_start = ORIGIN(DFU) - ORIGIN(BOOT2);
__bootloader_dfu_end = ORIGIN(DFU) + LENGTH(DFU) - ORIGIN(BOOT2);
