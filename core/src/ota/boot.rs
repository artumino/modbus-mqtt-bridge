//! Where a firmware image goes: `embassy-boot`'s DFU partition. The bootloader swaps it in (and
//! back, unless the new image confirms itself).

use embassy_boot::{BlockingFirmwareUpdater, FirmwareUpdaterError, State};
use embedded_storage::nor_flash::NorFlash;

#[cfg(feature = "defmt")]
use defmt::{error, info};
#[cfg(feature = "log")]
use log::{error, info};

use super::{OtaError, SIGNATURE_LEN};

pub struct BootSlot<'d, DFU: NorFlash, STATE: NorFlash> {
    updater: BlockingFirmwareUpdater<'d, DFU, STATE>,
    capacity: usize,
    while_verifying: fn(&mut dyn FnMut()),
}

impl<'d, DFU: NorFlash, STATE: NorFlash> BootSlot<'d, DFU, STATE> {
    /// `capacity` is the largest image the active partition can hold
    pub fn new(updater: BlockingFirmwareUpdater<'d, DFU, STATE>, capacity: usize) -> Self {
        Self {
            updater,
            capacity,
            while_verifying: |verify| verify(),
        }
    }

    /// Hashing the whole image is one long blocking call: `wrapper` runs it, e.g. with the
    /// watchdog paused
    pub fn verify_with(mut self, wrapper: fn(&mut dyn FnMut())) -> Self {
        self.while_verifying = wrapper;
        self
    }

    /// Call once the running image has proven it works (a full polling cycle succeeded).
    /// After a swap the bootloader reverts to the previous image on every reset until this is called.
    pub fn confirm_boot(&mut self) {
        match self.updater.get_state() {
            Ok(State::Swap | State::Revert) => match self.updater.mark_booted() {
                Ok(()) => info!("Running firmware confirmed"),
                Err(_) => error!("Failed to confirm the running firmware"),
            },
            Ok(_) => {}
            Err(_) => error!("Failed to read the bootloader state"),
        }
    }

    /// `false` while the running image is a trial that was not confirmed yet
    pub(super) fn accepts_update(&mut self) -> bool {
        matches!(self.updater.get_state(), Ok(State::Boot | State::Revert | State::DfuDetach))
    }

    pub(super) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Called before the first [`write`](Self::write) of every download
    pub(super) fn begin_download(&mut self) -> Result<(), OtaError> {
        // `write_firmware` skips erasing the sector it erased last, even when that was during an
        // earlier, aborted download and the sector has been written since. Touch the second sector
        // first so the first real write (sector 0) always erases.
        const { assert!(DFU::WRITE_SIZE <= 8) };
        self.write(DFU::ERASE_SIZE, &[0xFF; 8][..DFU::WRITE_SIZE])
    }

    /// Writes `data` at `offset`, erasing as required. Writes arrive in ascending, page aligned order.
    pub(super) fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), OtaError> {
        self.updater.write_firmware(offset, data).map_err(|_| OtaError::FlashWrite)
    }

    /// Checks the stored image against the signature of its SHA-512 digest and, only if it matches,
    /// arranges for the bootloader to install it on the next reset.
    pub(super) fn verify_and_arm(
        &mut self,
        public_key: &[u8; 32],
        signature: &[u8; SIGNATURE_LEN],
        size: u32,
    ) -> Result<(), OtaError> {
        let mut result = Err(OtaError::FlashWrite);
        (self.while_verifying)(&mut || {
            result = self
                .updater
                .verify_and_mark_updated(public_key, signature, size)
                .map_err(|err| match err {
                    FirmwareUpdaterError::Signature(_) => OtaError::ContentSignatureInvalid,
                    _ => OtaError::FlashWrite,
                });
        });
        result
    }
}
