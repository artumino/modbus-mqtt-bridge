//! The update procedure itself, independent of the MQTT client in use ([`OtaLink`]): the
//! firmware runs it over its MQTT connection, the tests over a simulated broker.

use core::time::Duration;

use embedded_storage::nor_flash::NorFlash;
use heapless::String;

#[cfg(feature = "defmt")]
use defmt::{error, info, warn};
#[cfg(feature = "log")]
use log::{error, info, warn};

use super::settings::{MAX_SETTINGS_SIZE, verify_settings};
use super::{
    Artifact, BootSlot, CHUNK_HEADER_LEN, CHUNK_SIZE, MANIFEST_LEN, Manifest, STATUS_DOWNLOADING, STATUS_ERROR,
    STATUS_READY, SettingsInvalid, SettingsStore, TOPIC_CHUNK, TOPIC_MANIFEST, TOPIC_REQUEST, TOPIC_STATUS,
    decode_chunk, encode_request, topic, topic_filter, validate_config, validate_registry,
};

/// How long to wait for the retained manifests after subscribing
const MANIFEST_WAIT: Duration = Duration::from_secs(2);
/// Once one manifest arrived the others follow right away, if there are any
const MANIFEST_FOLLOW_UP_WAIT: Duration = Duration::from_millis(300);
/// How long to wait for the answer to a chunk request
const CHUNK_WAIT: Duration = Duration::from_secs(3);
/// Times a chunk is requested before giving up
const CHUNK_ATTEMPTS: usize = 3;
/// Answers to earlier requests we skip over per attempt before asking again
const MAX_STALE_CHUNKS: usize = 4;
/// Flash is written in whole pages
const PAGE_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum OtaError {
    InvalidTopic,
    /// The connection to the broker failed
    Network,
    PublishFailed,
    MessageTooLarge,
    MalformedManifest,
    InvalidPublicKey,
    ManifestSignatureInvalid,
    /// Larger than the place it goes to (or empty)
    ImageTooLarge,
    /// The host stopped answering chunk requests
    ChunkTimeout,
    FlashWrite,
    /// The download does not match its signature, nothing was armed or stored
    ContentSignatureInvalid,
    /// The manifest was published for another kind of artifact than the topic says
    WrongArtifact,
    /// A settings file is signed but not usable (does not parse, other device, ...)
    InvalidSettings(SettingsInvalid),
    /// Not newer than the settings the device has (or had)
    StaleRevision,
}

impl OtaError {
    /// Short description, also what is reported to the host
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::InvalidTopic => "invalid topic",
            Self::Network => "network error",
            Self::PublishFailed => "publish failed",
            Self::MessageTooLarge => "message too large",
            Self::MalformedManifest => "malformed manifest",
            Self::InvalidPublicKey => "invalid public key",
            Self::ManifestSignatureInvalid => "manifest signature invalid",
            Self::ImageTooLarge => "does not fit",
            Self::ChunkTimeout => "chunk timeout",
            Self::FlashWrite => "flash write failed",
            Self::ContentSignatureInvalid => "content signature invalid",
            Self::WrongArtifact => "manifest is for another artifact",
            Self::InvalidSettings(invalid) => invalid.reason(),
            Self::StaleRevision => "revision is not newer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Outcome {
    /// Nothing newer on offer
    NoUpdate,
    /// A verified firmware image waits in the DFU partition or new settings were stored: reset to use them
    Ready,
}

pub struct OtaConfig<'a> {
    pub device_id: &'a str,
    /// ed25519 public key updates must be signed with
    pub public_key: &'a [u8; 32],
    /// Version of the running image, updates must be strictly newer
    pub current_version: u32,
}

/// The device's connection to the host tool (an MQTT client in production).
pub trait OtaLink {
    fn subscribe(&mut self, topic: &str) -> impl Future<Output = Result<(), OtaError>>;

    fn publish(&mut self, topic: &str, payload: &[u8]) -> impl Future<Output = Result<(), OtaError>>;

    /// Waits up to `wait` for a message on one of `topics` and copies it to `out`. Returns the index
    /// of the topic it arrived on and its length. Everything else that arrives in the meantime is
    /// ignored. `Ok(None)` means nothing arrived in time.
    fn receive(
        &mut self,
        topics: &[&str],
        wait: Duration,
        out: &mut [u8],
    ) -> impl Future<Output = Result<Option<(usize, usize)>, OtaError>>;
}

/// Looks for newer signed artifacts and, if there is one, downloads and verifies it. When several
/// are on offer the firmware goes first, then the configuration, then the registry map: one per
/// call, the host keeps offering the others.
/// `on_progress` is called whenever the transfer advanced (feeds the watchdog).
///
/// Failures are reported to the host on the status topic of the artifact before they are returned.
pub async fn check_and_update<L: OtaLink, DFU: NorFlash, STATE: NorFlash, F: NorFlash>(
    link: &mut L,
    firmware: &mut BootSlot<'_, DFU, STATE>,
    settings: &mut SettingsStore<F>,
    config: &OtaConfig<'_>,
    mut on_progress: impl FnMut(),
) -> Result<Outcome, OtaError> {
    // A swap that was not confirmed yet means we are a trial image, don't stack another update on it
    if !firmware.accepts_update() {
        return Ok(Outcome::NoUpdate);
    }

    let offers = receive_manifests(link, config.device_id).await?;
    let mut first_error = None;
    for (artifact, offer) in Artifact::ALL.into_iter().zip(offers) {
        let Some(offer) = offer else {
            continue;
        };
        let manifest = match offer.and_then(|manifest| evaluate(artifact, manifest, firmware, settings, config)) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => continue,
            Err(err) => {
                // A bad offer doesn't keep the next one from being considered
                report_error(link, config.device_id, artifact, err).await;
                first_error.get_or_insert(err);
                continue;
            }
        };
        let result = install(link, artifact, &manifest, firmware, settings, config, &mut on_progress).await;
        if let Err(err) = result {
            report_error(link, config.device_id, artifact, err).await;
        }
        return result;
    }
    first_error.map_or(Ok(Outcome::NoUpdate), Err)
}

/// The retained manifests on offer, indexed like [`Artifact::ALL`]
async fn receive_manifests<L: OtaLink>(
    link: &mut L,
    device_id: &str,
) -> Result<[Option<Result<Manifest, OtaError>>; 3], OtaError> {
    let filter = topic_filter::<128>(device_id, TOPIC_MANIFEST).ok_or(OtaError::InvalidTopic)?;
    let topics = Artifact::ALL.map(|artifact| topic::<128>(device_id, artifact, TOPIC_MANIFEST));
    let [Some(firmware), Some(config), Some(registry)] = &topics else {
        return Err(OtaError::InvalidTopic);
    };
    link.subscribe(&filter).await?;

    let mut offers = [None, None, None];
    let mut wait = MANIFEST_WAIT;
    let mut message = [0u8; MANIFEST_LEN];
    while offers.iter().any(Option::is_none) {
        let topics = [firmware.as_str(), config.as_str(), registry.as_str()];
        let Some((index, len)) = link.receive(&topics, wait, &mut message).await? else {
            break;
        };
        offers[index] = Some(Manifest::decode(&message[..len]).ok_or(OtaError::MalformedManifest));
        wait = MANIFEST_FOLLOW_UP_WAIT;
    }
    Ok(offers)
}

/// Is the manifest genuine, for this artifact, newer than what we have and small enough?
/// Checked before anything is written to flash. `Ok(None)`: nothing to do.
fn evaluate<DFU: NorFlash, STATE: NorFlash, F: NorFlash>(
    artifact: Artifact,
    manifest: Manifest,
    firmware: &BootSlot<'_, DFU, STATE>,
    settings: &mut SettingsStore<F>,
    config: &OtaConfig<'_>,
) -> Result<Option<Manifest>, OtaError> {
    let public_key = salty::PublicKey::try_from(config.public_key).map_err(|_| OtaError::InvalidPublicKey)?;
    public_key
        .verify(&manifest.signed_bytes(), &salty::Signature::from(&manifest.manifest_signature))
        .map_err(|_| OtaError::ManifestSignatureInvalid)?;
    if manifest.kind != artifact.kind() {
        return Err(OtaError::WrongArtifact);
    }

    let (current, capacity) = match artifact {
        Artifact::Firmware => (config.current_version, firmware.capacity()),
        _ => (settings.latest_revision(artifact), MAX_SETTINGS_SIZE),
    };
    if manifest.version <= current {
        info!("Offered {} {:#x} is not newer than {:#x}, ignoring", artifact.name(), manifest.version, current);
        return Ok(None);
    }
    if manifest.size == 0 || manifest.size as usize > capacity {
        return Err(OtaError::ImageTooLarge);
    }
    Ok(Some(manifest))
}

async fn install<L: OtaLink, DFU: NorFlash, STATE: NorFlash, F: NorFlash>(
    link: &mut L,
    artifact: Artifact,
    manifest: &Manifest,
    firmware: &mut BootSlot<'_, DFU, STATE>,
    settings: &mut SettingsStore<F>,
    config: &OtaConfig<'_>,
    on_progress: &mut impl FnMut(),
) -> Result<Outcome, OtaError> {
    info!("Downloading {} {:#x} ({} bytes)", artifact.name(), manifest.version, manifest.size);
    let status_topic = topic::<128>(config.device_id, artifact, TOPIC_STATUS).ok_or(OtaError::InvalidTopic)?;
    link.publish(&status_topic, STATUS_DOWNLOADING.as_bytes()).await?;

    // Only once the whole content matches its signature it is armed or stored
    if artifact == Artifact::Firmware {
        firmware.begin_download()?;
        download(link, artifact, manifest, config.device_id, on_progress, |offset, page| {
            firmware.write(offset, page)
        })
        .await?;
        firmware.verify_and_arm(config.public_key, &manifest.content_signature, manifest.size)?;
    } else {
        // A settings file is smaller than a page: it arrives in one piece
        download(link, artifact, manifest, config.device_id, on_progress, |_, data| {
            store_settings(artifact, manifest, data, settings, config)
        })
        .await?;
    }

    link.publish(&status_topic, STATUS_READY.as_bytes()).await?;
    info!("{} verified, restart to use version {:#x}", artifact.name(), manifest.version);
    Ok(Outcome::Ready)
}

fn store_settings<F: NorFlash>(
    artifact: Artifact,
    manifest: &Manifest,
    data: &[u8],
    settings: &mut SettingsStore<F>,
    config: &OtaConfig<'_>,
) -> Result<(), OtaError> {
    let genuine = verify_settings(config.public_key, artifact, &manifest.content_signature, |hasher| {
        hasher.update(data);
        true
    });
    if !genuine {
        return Err(OtaError::ContentSignatureInvalid);
    }
    // Signed does not mean usable: the device would have trouble with a file it can't parse
    if artifact == Artifact::Config {
        if validate_config(data).map_err(OtaError::InvalidSettings)?.mqtt.device_id != config.device_id {
            return Err(OtaError::InvalidSettings(SettingsInvalid::WrongDevice));
        }
    } else {
        validate_registry(data).map_err(OtaError::InvalidSettings)?;
    }
    settings.store(artifact, manifest.version, &manifest.content_signature, data)
}

/// Pulls the content chunk by chunk and hands it to `on_page` one page (or the last, partial page)
/// at a time
async fn download<L: OtaLink>(
    link: &mut L,
    artifact: Artifact,
    manifest: &Manifest,
    device_id: &str,
    on_progress: &mut impl FnMut(),
    mut on_page: impl FnMut(usize, &[u8]) -> Result<(), OtaError>,
) -> Result<(), OtaError> {
    let chunk_topic = topic::<128>(device_id, artifact, TOPIC_CHUNK).ok_or(OtaError::InvalidTopic)?;
    let request_topic = topic::<128>(device_id, artifact, TOPIC_REQUEST).ok_or(OtaError::InvalidTopic)?;
    link.subscribe(&chunk_topic).await?;

    let size = manifest.size as usize;
    let mut page = [0u8; PAGE_SIZE];
    let mut message = [0u8; CHUNK_HEADER_LEN + CHUNK_SIZE];
    let mut offset = 0;
    while offset < size {
        let length = (size - offset).min(CHUNK_SIZE);
        fetch_chunk(link, &request_topic, &chunk_topic, offset, length, &mut message).await?;

        let in_page = offset % PAGE_SIZE;
        page[in_page..in_page + length].copy_from_slice(&message[CHUNK_HEADER_LEN..CHUNK_HEADER_LEN + length]);
        offset += length;

        if offset % PAGE_SIZE == 0 || offset == size {
            let filled = in_page + length;
            on_page(offset - filled, &page[..filled])?;
        }
        on_progress();
    }
    Ok(())
}

async fn report_error<L: OtaLink>(link: &mut L, device_id: &str, artifact: Artifact, err: OtaError) {
    error!("Update of {} failed: {}", artifact.name(), err.reason());
    let mut status = String::<64>::new();
    let _ = status.push_str(STATUS_ERROR);
    let _ = status.push_str(": ");
    let _ = status.push_str(err.reason());
    if let Some(status_topic) = topic::<128>(device_id, artifact, TOPIC_STATUS) {
        let _ = link.publish(&status_topic, status.as_bytes()).await;
    }
}

/// Asks the host for the chunk at `offset` and waits for it, retrying a few times.
/// On success the message (offset header + payload) is in `message`.
async fn fetch_chunk<L: OtaLink>(
    link: &mut L,
    request_topic: &str,
    chunk_topic: &str,
    offset: usize,
    length: usize,
    message: &mut [u8],
) -> Result<(), OtaError> {
    for attempt in 0..CHUNK_ATTEMPTS {
        if attempt > 0 {
            warn!("Chunk at {} timed out, asking again", offset);
        }
        link.publish(request_topic, &encode_request(offset as u32))
            .await?;

        for _ in 0..=MAX_STALE_CHUNKS {
            let Some((_, received)) = link.receive(&[chunk_topic], CHUNK_WAIT, message).await?
            else {
                break;
            };
            // Answers to earlier (repeated) requests may still trickle in, skip those
            if let Some((chunk_offset, payload)) = decode_chunk(&message[..received])
                && chunk_offset as usize == offset
                && payload.len() == length
            {
                return Ok(());
            }
        }
    }
    Err(OtaError::ChunkTimeout)
}
