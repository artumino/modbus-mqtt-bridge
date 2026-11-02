//! The two settings files (device configuration and registry map) and where they are kept.
//!
//! They live in a dedicated flash region of four 4 KiB slots, two per file (A/B). A slot is a
//! 128 byte header followed by the file:
//!
//! ```text
//! 0   magic "MMBS"
//! 4   revision      u32 LE, only ever grows
//! 8   length        u32 LE
//! 12  state         u8, bits are only ever cleared (see below)
//! 16  signature     64 bytes, over SHA-512(domain | kind | file)
//! 128 file
//! ```
//!
//! A new file is written into the slot that is not the fallback: erase, file, header last, so
//! power loss leaves a slot without a valid header. The header carries the state, which advances
//! without any erase: `Pending` (just written) → `Tried` (booted with it) → `Confirmed` (a full
//! polling cycle worked) or `Rejected`. A file that was `Tried` but never `Confirmed` made the
//! device reset, so at the next boot it is rejected and the previous file (or the one compiled
//! into the firmware) is used. The signature is checked again on every load.

use core::net::SocketAddr;
use core::str::FromStr;

use heapless::Vec;

use super::{Artifact, SIGNATURE_LEN};
use crate::configuration::Configuration;
use crate::registry_map::RegistryMap;

pub const SETTINGS_SLOT_SIZE: usize = 4096;
/// Two slots for the configuration, two for the registry map
pub const SETTINGS_REGION_SIZE: usize = 4 * SETTINGS_SLOT_SIZE;
const HEADER_LEN: usize = 128;
/// Largest settings file
pub const MAX_SETTINGS_SIZE: usize = SETTINGS_SLOT_SIZE - HEADER_LEN;

const MAGIC: [u8; 4] = *b"MMBS";
const REVISION_AT: usize = 4;
const LENGTH_AT: usize = 8;
const STATE_AT: usize = 12;
const SIGNATURE_AT: usize = 16;

const STATE_PENDING: u8 = 0xFF;
#[cfg(feature = "ota")]
const STATE_TRIED: u8 = 0x7F;
#[cfg(feature = "ota")]
const STATE_CONFIRMED: u8 = 0x0F;
#[cfg(feature = "ota")]
const STATE_REJECTED: u8 = 0x00;

pub type SlotImage = Vec<u8, SETTINGS_SLOT_SIZE>;

/// Offset of slot `index` (0 or 1) of a settings file inside the settings region
pub const fn slot_offset(artifact: Artifact, index: usize) -> Option<usize> {
    let file = match artifact {
        Artifact::Config => 0,
        Artifact::Registry => 1,
        Artifact::Firmware => return None,
    };
    if index > 1 {
        return None;
    }
    Some((file * 2 + index) * SETTINGS_SLOT_SIZE)
}

/// What a freshly signed file looks like in flash (header + file), ready to be written to
/// [`slot_offset`]. `None` if the file is empty or does not fit. Used to provision a device with a probe.
pub fn encode_slot(
    artifact: Artifact,
    revision: u32,
    signature: &[u8; SIGNATURE_LEN],
    data: &[u8],
) -> Option<SlotImage> {
    if data.is_empty() || data.len() > MAX_SETTINGS_SIZE || slot_offset(artifact, 0).is_none() {
        return None;
    }
    let mut image = SlotImage::new();
    image
        .extend_from_slice(&header(revision, data.len(), STATE_PENDING, signature))
        .ok()?;
    image.extend_from_slice(data).ok()?;
    Some(image)
}

fn header(
    revision: u32,
    len: usize,
    state: u8,
    signature: &[u8; SIGNATURE_LEN],
) -> [u8; HEADER_LEN] {
    let mut header = [0xFF; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[REVISION_AT..REVISION_AT + 4].copy_from_slice(&revision.to_le_bytes());
    header[LENGTH_AT..LENGTH_AT + 4].copy_from_slice(&(len as u32).to_le_bytes());
    header[STATE_AT] = state;
    header[SIGNATURE_AT..SIGNATURE_AT + SIGNATURE_LEN].copy_from_slice(signature);
    header
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SettingsInvalid {
    Empty,
    TooLarge,
    /// Not valid JSON for the device configuration (or not UTF-8)
    Config,
    /// `mqtt.endpoint` is not an `ip:port` address
    Endpoint,
    /// Not valid UTF-8
    NotUtf8,
    /// 1-based number of the first malformed line
    Registry(usize),
    /// The registry map has no entries
    NoEntries,
    /// The configuration is for another device
    WrongDevice,
}

impl SettingsInvalid {
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Empty => "empty file",
            Self::TooLarge => "file too large",
            Self::Config => "invalid configuration",
            Self::Endpoint => "invalid mqtt endpoint",
            Self::NotUtf8 => "not utf-8",
            Self::Registry(_) => "invalid registry map",
            Self::NoEntries => "registry map has no entries",
            Self::WrongDevice => "configuration is for another device",
        }
    }
}

fn check_size(data: &[u8]) -> Result<(), SettingsInvalid> {
    if data.is_empty() {
        Err(SettingsInvalid::Empty)
    } else if data.len() > MAX_SETTINGS_SIZE {
        Err(SettingsInvalid::TooLarge)
    } else {
        Ok(())
    }
}

/// Parses a device configuration the way the firmware will, and checks what the firmware would
/// otherwise panic on later
pub fn validate_config(data: &[u8]) -> Result<Configuration<'_>, SettingsInvalid> {
    check_size(data)?;
    let (config, _) =
        serde_json_core::from_slice::<Configuration>(data).map_err(|_| SettingsInvalid::Config)?;
    SocketAddr::from_str(config.mqtt.endpoint).map_err(|_| SettingsInvalid::Endpoint)?;
    Ok(config)
}

/// Checks that every line of a registry map parses. Returns the map as text.
pub fn validate_registry(data: &[u8]) -> Result<&str, SettingsInvalid> {
    check_size(data)?;
    let text = core::str::from_utf8(data).map_err(|_| SettingsInvalid::NotUtf8)?;
    match RegistryMap::validate(text) {
        Ok(0) => Err(SettingsInvalid::NoEntries),
        Ok(_) => Ok(text),
        Err(line) => Err(SettingsInvalid::Registry(line)),
    }
}

#[cfg(feature = "ota")]
pub use store::SettingsStore;
#[cfg(feature = "ota")]
pub(super) use store::verify_settings;

#[cfg(feature = "ota")]
mod store {
    use embedded_storage::nor_flash::NorFlash;

    #[cfg(feature = "defmt")]
    use defmt::{error, info, warn};
    #[cfg(feature = "log")]
    use log::{error, info, warn};

    use super::*;
    use crate::ota::{OtaError, settings_prefix};

    /// Checks `signature` over a settings file. `feed` hands the file to the hasher and returns
    /// `false` if it couldn't.
    pub(in crate::ota) fn verify_settings(
        public_key: &[u8; 32],
        artifact: Artifact,
        signature: &[u8; SIGNATURE_LEN],
        feed: impl FnOnce(&mut salty::Sha512) -> bool,
    ) -> bool {
        let mut hasher = salty::Sha512::new();
        hasher.update(&settings_prefix(artifact));
        if !feed(&mut hasher) {
            return false;
        }
        let Ok(public_key) = salty::PublicKey::try_from(public_key) else {
            return false;
        };
        public_key
            .verify(&hasher.finalize(), &salty::Signature::from(signature))
            .is_ok()
    }

    #[derive(Clone, Copy)]
    struct SlotInfo {
        index: usize,
        revision: u32,
        len: usize,
        state: u8,
    }

    impl SlotInfo {
        fn usable(&self) -> bool {
            matches!(self.state, STATE_PENDING | STATE_TRIED | STATE_CONFIRMED)
        }

        /// Which slot to give up first when a new file arrives: lower goes first
        fn overwrite_rank(&self) -> u8 {
            match self.state {
                STATE_CONFIRMED => 2,
                STATE_PENDING | STATE_TRIED => 1,
                _ => 0,
            }
        }
    }

    /// The file in use: the newest one that was not rejected
    fn current(slots: &[Option<SlotInfo>; 2]) -> Option<SlotInfo> {
        slots.iter().flatten().filter(|slot| slot.usable()).max_by_key(|slot| slot.revision).copied()
    }

    /// The settings flash region (a partition of exactly [`SETTINGS_REGION_SIZE`] bytes)
    pub struct SettingsStore<F: NorFlash> {
        flash: F,
        public_key: [u8; 32],
    }

    impl<F: NorFlash> SettingsStore<F> {
        const CHECKS: () = {
            assert!(F::READ_SIZE == 1);
            assert!(F::WRITE_SIZE <= 8);
            assert!(SETTINGS_SLOT_SIZE % F::ERASE_SIZE == 0);
            assert!(HEADER_LEN % F::WRITE_SIZE == 0);
        };

        /// `public_key` is what every stored file has to be signed with
        pub fn new(flash: F, public_key: [u8; 32]) -> Self {
            let () = Self::CHECKS;
            assert!(flash.capacity() >= SETTINGS_REGION_SIZE);
            Self { flash, public_key }
        }

        fn address(artifact: Artifact, index: usize) -> u32 {
            slot_offset(artifact, index).expect("not a settings file") as u32
        }

        /// The slot's header, `None` unless the file in it is intact and genuine
        fn inspect(&mut self, artifact: Artifact, index: usize) -> Option<SlotInfo> {
            let base = Self::address(artifact, index);
            let mut head = [0u8; HEADER_LEN];
            self.flash.read(base, &mut head).ok()?;
            if head[..4] != MAGIC {
                return None;
            }
            let field = |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
            let (revision, len) = (field(REVISION_AT), field(LENGTH_AT) as usize);
            if len == 0 || len > MAX_SETTINGS_SIZE {
                return None;
            }
            let signature: [u8; SIGNATURE_LEN] = head[SIGNATURE_AT..SIGNATURE_AT + SIGNATURE_LEN].try_into().ok()?;

            let flash = &mut self.flash;
            let genuine = verify_settings(&self.public_key, artifact, &signature, |hasher| {
                let mut block = [0u8; 256];
                for start in (0..len).step_by(block.len()) {
                    let block = &mut block[..(len - start).min(256)];
                    if flash.read(base + (HEADER_LEN + start) as u32, block).is_err() {
                        return false;
                    }
                    hasher.update(block);
                }
                true
            });
            if !genuine {
                warn!("Stored {} in slot {} fails its signature", artifact.name(), index);
                return None;
            }
            Some(SlotInfo { index, revision, len, state: head[STATE_AT] })
        }

        fn scan(&mut self, artifact: Artifact) -> [Option<SlotInfo>; 2] {
            [self.inspect(artifact, 0), self.inspect(artifact, 1)]
        }

        fn set_state(&mut self, artifact: Artifact, slot: &SlotInfo, state: u8) -> Result<(), OtaError> {
            let at = STATE_AT - STATE_AT % F::WRITE_SIZE;
            let mut bytes = [0xFF; 8];
            bytes[STATE_AT - at] = state;
            self.flash
                .write(Self::address(artifact, slot.index) + at as u32, &bytes[..F::WRITE_SIZE])
                .map_err(|_| OtaError::FlashWrite)
        }

        /// Highest revision of `artifact` that is stored, rejected files included: anything not
        /// newer than that is never accepted again
        pub fn latest_revision(&mut self, artifact: Artifact) -> u32 {
            self.scan(artifact).iter().flatten().map(|slot| slot.revision).max().unwrap_or(0)
        }

        /// Called once at boot. Copies the file to use into `out` and returns its length, `None`
        /// if there is none (use the one compiled into the firmware).
        ///
        /// A file that was tried before without ever being confirmed is rejected here and the
        /// previous one is used instead.
        pub fn load(&mut self, artifact: Artifact, out: &mut [u8]) -> Result<Option<usize>, OtaError> {
            loop {
                let Some(slot) = current(&self.scan(artifact)) else {
                    return Ok(None);
                };
                match slot.state {
                    STATE_PENDING => {
                        info!("Trying new {} (revision {})", artifact.name(), slot.revision);
                        self.set_state(artifact, &slot, STATE_TRIED)?;
                    }
                    STATE_TRIED => {
                        error!("New {} (revision {}) did not work, falling back", artifact.name(), slot.revision);
                        self.set_state(artifact, &slot, STATE_REJECTED)?;
                        continue;
                    }
                    _ => {}
                }
                let out = out.get_mut(..slot.len).ok_or(OtaError::ImageTooLarge)?;
                self.flash
                    .read(Self::address(artifact, slot.index) + HEADER_LEN as u32, out)
                    .map_err(|_| OtaError::FlashWrite)?;
                return Ok(Some(slot.len));
            }
        }

        /// [`load`](Self::load), rejecting for good the files `accept` doesn't like (e.g. ones a newer
        /// firmware can't parse anymore)
        pub fn load_accepted(
            &mut self,
            artifact: Artifact,
            out: &mut [u8],
            accept: impl Fn(&[u8]) -> bool,
        ) -> Result<Option<usize>, OtaError> {
            loop {
                let Some(len) = self.load(artifact, out)? else {
                    return Ok(None);
                };
                if accept(&out[..len]) {
                    return Ok(Some(len));
                }
                error!("Stored {} can't be used, rejecting it", artifact.name());
                if let Some(slot) = current(&self.scan(artifact)) {
                    self.set_state(artifact, &slot, STATE_REJECTED)?;
                }
            }
        }

        /// The device ran a full polling cycle with the files it loaded: they are good. Until
        /// then a reset makes the next boot give up on a new file.
        pub fn confirm(&mut self) {
            for artifact in [Artifact::Config, Artifact::Registry] {
                for slot in self.scan(artifact).iter().flatten().filter(|slot| slot.state == STATE_TRIED) {
                    match self.set_state(artifact, slot, STATE_CONFIRMED) {
                        Ok(()) => info!("{} revision {} confirmed", artifact.name(), slot.revision),
                        Err(_) => error!("Failed to confirm {}", artifact.name()),
                    }
                }
            }
        }

        /// Stores a new file as `Pending`. The caller has verified the signature and the content.
        pub fn store(
            &mut self,
            artifact: Artifact,
            revision: u32,
            signature: &[u8; SIGNATURE_LEN],
            data: &[u8],
        ) -> Result<(), OtaError> {
            if data.is_empty() || data.len() > MAX_SETTINGS_SIZE {
                return Err(OtaError::ImageTooLarge);
            }
            let slots = self.scan(artifact);
            if slots.iter().flatten().any(|slot| revision <= slot.revision) {
                return Err(OtaError::StaleRevision);
            }

            // An empty or broken slot first, then a rejected one, then an unproven one, and only
            // then the older of two proven files. The proven fallback survives whenever possible.
            let index = (0..2)
                .min_by_key(|&index| slots[index].map_or((0, 0), |slot| (slot.overwrite_rank(), slot.revision)))
                .unwrap_or(0);

            let base = Self::address(artifact, index);
            let failed = |_| OtaError::FlashWrite;
            self.flash.erase(base, base + SETTINGS_SLOT_SIZE as u32).map_err(failed)?;
            let whole = data.len() - data.len() % F::WRITE_SIZE;
            let mut tail = [0xFF; 8];
            tail[..data.len() - whole].copy_from_slice(&data[whole..]);
            let file = base + HEADER_LEN as u32;
            if whole > 0 {
                self.flash.write(file, &data[..whole]).map_err(failed)?;
            }
            if whole < data.len() {
                self.flash.write(file + whole as u32, &tail[..F::WRITE_SIZE]).map_err(failed)?;
            }
            // The header goes last, only now the slot exists
            self.flash
                .write(base, &header(revision, data.len(), STATE_PENDING, signature))
                .map_err(failed)?;
            info!("Stored {} revision {}", artifact.name(), revision);
            Ok(())
        }
    }
}
