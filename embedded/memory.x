/* Layout shared with ../bootloader/memory.x, keep both in sync */
MEMORY {
    BOOT2            : ORIGIN = 0x10000000, LENGTH = 0x100
    BOOTLOADER_STATE : ORIGIN = 0x10006000, LENGTH = 4K
    FLASH            : ORIGIN = 0x10007000, LENGTH = 1000K
    DFU              : ORIGIN = 0x10101000, LENGTH = 1004K
    SETTINGS         : ORIGIN = 0x101FC000, LENGTH = 16K
    RAM              : ORIGIN = 0x20000000, LENGTH = 256K
}

__bootloader_state_start = ORIGIN(BOOTLOADER_STATE) - ORIGIN(BOOT2);
__bootloader_state_end = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE) - ORIGIN(BOOT2);

__bootloader_active_start = ORIGIN(FLASH) - ORIGIN(BOOT2);
__bootloader_active_end = ORIGIN(FLASH) + LENGTH(FLASH) - ORIGIN(BOOT2);

__bootloader_dfu_start = ORIGIN(DFU) - ORIGIN(BOOT2);
__bootloader_dfu_end = ORIGIN(DFU) + LENGTH(DFU) - ORIGIN(BOOT2);

/* Configuration and registry map received over the air: 2 slots each, see core/src/ota/settings.rs */
__settings_start = ORIGIN(SETTINGS) - ORIGIN(BOOT2);
__settings_end = ORIGIN(SETTINGS) + LENGTH(SETTINGS) - ORIGIN(BOOT2);
