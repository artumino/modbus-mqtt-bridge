//! Hardware side of the over-the-air updates: the MQTT client and the flash as seen by the
//! update procedure in `modbus_mqtt_bridge_core::ota` (which is where the logic and its tests live).
//!
//! The host (`service/src/bin/ota.rs`) retains a signed manifest per artifact on
//! `<device_id>/ota/<firmware|config|registry>/manifest`. When it describes something newer than
//! what the device has, the content is pulled chunk by chunk. A firmware image goes to the DFU
//! partition, the configuration and the registry map to the settings partition. Only content whose
//! ed25519 signature verifies against [`OTA_PUBLIC_KEY`] is ever used. The bootloader reverts a
//! swap unless the new image calls `confirm_boot` after proving itself, and the settings
//! store does the same for new settings files.

use core::cell::RefCell;
use core::time::Duration;

use defmt::{error, info};
use embassy_boot_rp::{BlockingFirmwareUpdater, FirmwareUpdaterConfig};
use embassy_embedded_hal::flash::partition::BlockingPartition;
use embassy_net::tcp::client::TcpConnection;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Instant, with_timeout};
use embedded_storage::nor_flash::NorFlash;
use modbus_mqtt_bridge_core::mqtt::MqttSender;
use modbus_mqtt_bridge_core::ota::{
    self, Artifact, BootSlot, CHUNK_HEADER_LEN, CHUNK_SIZE, OtaConfig, OtaLink, SettingsStore,
};
use rust_mqtt::buffer::BumpBuffer;
use rust_mqtt::client::Client;
use rust_mqtt::client::event::Event;
use rust_mqtt::client::options::{SubscriptionOptions, TopicReference};
use rust_mqtt::types::{MqttString, TopicFilter};

pub use modbus_mqtt_bridge_core::ota::{OtaError, Outcome};

/// The only key firmware updates are accepted from. Created with `ota keygen`.
static OTA_PUBLIC_KEY: [u8; 32] = *include_bytes!("../keys/ota_public.key");

/// Version of the running image, taken from Cargo.toml. Updates must be strictly newer.
pub const FIRMWARE_VERSION: u32 = match ota::parse_version(env!("CARGO_PKG_VERSION")) {
    Some(version) => version,
    None => panic!("package version must look like major.minor.patch"),
};

/// MQTT client as used by the bridge: 2 concurrent subscriptions (manifests of all artifacts, chunks)
pub type MqttClient<'a, 'c> =
    Client<'a, 'c, TcpConnection<'a, 1, 1024, 1024>, BumpBuffer<'c>, 2, 1, 1, 1, 16, 0, 0>;

/// Largest packet the broker may send us, must fit the bump buffer with room to spare
pub const MAX_PACKET_SIZE: u32 = (CHUNK_HEADER_LEN + CHUNK_SIZE + 256) as u32;

/// Looks for newer signed firmware, configuration or registry map and, if there is one, downloads and verifies it.
/// `on_progress` is called whenever the transfer advanced (feeds the watchdog).
pub async fn check_and_update<F: NorFlash>(
    client: &mut MqttClient<'_, '_>,
    firmware: &mut Firmware<'_, F>,
    settings: &mut Settings<'_, F>,
    device_id: &str,
    on_progress: impl FnMut(),
) -> Result<Outcome, OtaError> {
    let config = OtaConfig {
        device_id,
        public_key: &OTA_PUBLIC_KEY,
        current_version: FIRMWARE_VERSION,
    };
    ota::check_and_update(&mut MqttLink(client), firmware, settings, &config, on_progress).await
}

/// [`OtaLink`] over the bridge's MQTT connection
struct MqttLink<'l, 'a, 'c>(&'l mut MqttClient<'a, 'c>);

impl OtaLink for MqttLink<'_, '_, '_> {
    async fn subscribe(&mut self, topic: &str) -> Result<(), OtaError> {
        let filter = MqttString::try_from(topic)
            .ok()
            .and_then(TopicFilter::new)
            .ok_or(OtaError::InvalidTopic)?;
        self.0.subscribe(filter, &SubscriptionOptions::new()).await.map_err(|err| {
            error!("Subscribe failed: {:?}", err);
            OtaError::Network
        })?;
        Ok(())
    }

    async fn publish(&mut self, topic: &str, payload: &[u8]) -> Result<(), OtaError> {
        self.0.send(topic, payload).await.map_err(|_| OtaError::PublishFailed)
    }

    async fn receive(
        &mut self,
        topics: &[&str],
        wait: Duration,
        out: &mut [u8],
    ) -> Result<Option<(usize, usize)>, OtaError> {
        let deadline = Instant::now() + embassy_time::Duration::from_micros(wait.as_micros() as u64);
        loop {
            // SAFETY: nothing handed out by a previous iteration is alive anymore, every event is
            // dropped before the next one and callers only get copies.
            unsafe { self.0.buffer_mut().reset() };

            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            let header = match with_timeout(remaining, self.0.poll_header()).await {
                Ok(Ok(header)) => header,
                Ok(Err(err)) => {
                    error!("MQTT receive failed: {:?}", err);
                    return Err(OtaError::Network);
                }
                Err(_) => return Ok(None),
            };
            let event = self.0.poll_body(header).await.map_err(|err| {
                error!("MQTT receive failed: {:?}", err);
                OtaError::Network
            })?;

            if let Event::Publish(publication) = event
                && let TopicReference::Name(name) = &publication.topic
                && let Some(index) = topics.iter().position(|topic| *topic == name.as_ref().as_str())
            {
                let message = publication.message.as_bytes();
                let Some(destination) = out.get_mut(..message.len()) else {
                    return Err(OtaError::MessageTooLarge);
                };
                destination.copy_from_slice(message);
                return Ok(Some((index, message.len())));
            }
        }
    }
}

unsafe extern "C" {
    static __bootloader_active_start: u32;
    static __bootloader_active_end: u32;
    static __settings_start: u32;
    static __settings_end: u32;
}

/// Offset and length of a partition laid out in memory.x, from its linker symbols (their address
/// is their value)
fn partition(start: *const u32, end: *const u32) -> (u32, u32) {
    (start as u32, end as u32 - start as u32)
}

/// The firmware's DFU/state partitions behind the bootloader
pub type Firmware<'d, F> = BootSlot<'d, BlockingPartition<'d, NoopRawMutex, F>, BlockingPartition<'d, NoopRawMutex, F>>;

/// `aligned` must be `max(WRITE_SIZE, READ_SIZE)` of the flash
pub fn firmware_slot<'d, F: NorFlash>(flash: &'d Mutex<NoopRawMutex, RefCell<F>>, aligned: &'d mut [u8]) -> Firmware<'d, F> {
    let (_, capacity) = partition(&raw const __bootloader_active_start, &raw const __bootloader_active_end);
    let updater = BlockingFirmwareUpdater::new(FirmwareUpdaterConfig::from_linkerfile_blocking(flash, flash), aligned);
    BootSlot::new(updater, capacity as usize).verify_with(with_watchdog_paused)
}

/// The settings files (configuration, registry map) kept in flash
pub type Settings<'d, F> = SettingsStore<BlockingPartition<'d, NoopRawMutex, F>>;

/// The settings partition of the flash chip, trusting files signed with [`OTA_PUBLIC_KEY`] only
pub fn settings_store<F: NorFlash>(flash: &Mutex<NoopRawMutex, RefCell<F>>) -> Settings<'_, F> {
    let (start, len) = partition(&raw const __settings_start, &raw const __settings_end);
    SettingsStore::new(BlockingPartition::new(flash, start, len), OTA_PUBLIC_KEY)
}

/// The settings file to use: the newest signed one received over the air that `accept` likes,
/// `default` (compiled into the firmware) otherwise
pub fn load_settings<'b, F: NorFlash>(
    settings: &mut Settings<'_, F>,
    artifact: Artifact,
    buffer: &'b mut [u8],
    default: &'b [u8],
    accept: fn(&[u8]) -> bool,
) -> &'b [u8] {
    match settings.load_accepted(artifact, buffer, accept) {
        Ok(Some(len)) => {
            info!("Using the {} received over the air", artifact.name());
            &buffer[..len]
        }
        Ok(None) => default,
        Err(_) => {
            error!("Cannot read the stored {}", artifact.name());
            default
        }
    }
}

/// Hashing a whole image is one blocking call, on a big image it can outlast the watchdog period
/// (the executor can't run the task that feeds it meanwhile). The counter holds its value while the
/// watchdog is disabled, so pausing costs no margin and the call is bounded by the image size.
fn with_watchdog_paused(f: &mut dyn FnMut()) {
    let control = embassy_rp::pac::WATCHDOG.ctrl();
    control.modify(|w| w.set_enable(false));
    f();
    control.modify(|w| w.set_enable(true));
}
