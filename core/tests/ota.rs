//! Host tests for the over-the-air update procedure.
//!
//! The real `embassy-boot` bootloader and updater run on an in-memory NOR flash, the broker and the
//! host tool are simulated by [`HostLink`], which can lose, repeat, reorder and corrupt chunks.

use core::cell::RefCell;
use core::time::Duration;
use std::collections::VecDeque;

use embassy_boot::{
    AlignedBuffer, BlockingFirmwareUpdater, BootLoader, BootLoaderConfig, FirmwareUpdaterConfig,
    State,
};
use embassy_embedded_hal::flash::partition::BlockingPartition;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};
use futures::executor::block_on;
use modbus_mqtt_bridge_core::ota::{
    self, Artifact, BootSlot, CHUNK_HEADER_LEN, CHUNK_SIZE, MAX_SETTINGS_SIZE, Manifest, OtaConfig, OtaError, OtaLink,
    Outcome, SettingsInvalid, SettingsStore, check_and_update,
};

// ---------------------------------------------------------------- flash

const SECTOR: usize = 4096;
const STATE_SECTORS: usize = 1;
const ACTIVE_SECTORS: usize = 8;
const DFU_SECTORS: usize = ACTIVE_SECTORS + 1;
const STATE_START: usize = 0;
const ACTIVE_START: usize = STATE_SECTORS * SECTOR;
const DFU_START: usize = ACTIVE_START + ACTIVE_SECTORS * SECTOR;
const SETTINGS_START: usize = DFU_START + DFU_SECTORS * SECTOR;
const SETTINGS_SIZE: usize = ota::SETTINGS_REGION_SIZE;
const FLASH_SIZE: usize = SETTINGS_START + SETTINGS_SIZE;
const ACTIVE_SIZE: usize = ACTIVE_SECTORS * SECTOR;

#[derive(Debug)]
struct FlashFault;

impl NorFlashError for FlashFault {
    fn kind(&self) -> NorFlashErrorKind {
        NorFlashErrorKind::Other
    }
}

/// NOR flash: erase sets bytes to 0xFF, writing can only clear bits.
struct MemFlash {
    mem: Vec<u8>,
    /// Every erase or write counts as an operation
    operations: usize,
    /// Simulates a power cut: the n-th operation from now on (and all later ones) fail
    fail_after: Option<usize>,
}

impl MemFlash {
    fn new() -> Self {
        Self {
            mem: vec![0xFF; FLASH_SIZE],
            operations: 0,
            fail_after: None,
        }
    }

    fn operation(&mut self) -> Result<(), FlashFault> {
        if let Some(remaining) = &mut self.fail_after {
            if *remaining == 0 {
                return Err(FlashFault);
            }
            *remaining -= 1;
        }
        self.operations += 1;
        Ok(())
    }
}

impl ErrorType for MemFlash {
    type Error = FlashFault;
}

impl ReadNorFlash for MemFlash {
    const READ_SIZE: usize = 1;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        bytes.copy_from_slice(&self.mem[offset as usize..offset as usize + bytes.len()]);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.mem.len()
    }
}

impl NorFlash for MemFlash {
    const WRITE_SIZE: usize = 1;
    const ERASE_SIZE: usize = SECTOR;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        assert!(
            from as usize % SECTOR == 0 && to as usize % SECTOR == 0,
            "unaligned erase"
        );
        self.operation()?;
        self.mem[from as usize..to as usize].fill(0xFF);
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.operation()?;
        for (cell, byte) in self.mem[offset as usize..offset as usize + bytes.len()]
            .iter_mut()
            .zip(bytes)
        {
            *cell &= *byte;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- device

type Flash = Mutex<NoopRawMutex, RefCell<MemFlash>>;
type Part<'a> = BlockingPartition<'a, NoopRawMutex, MemFlash>;

/// A flash chip holding the bootloader state, the active and the DFU partition.
struct Device {
    flash: Flash,
}

impl Device {
    /// A device that was flashed with `image` as the running firmware
    fn new(image: &[u8]) -> Self {
        let mut flash = MemFlash::new();
        flash.mem[ACTIVE_START..ACTIVE_START + image.len()].copy_from_slice(image);
        Self {
            flash: Mutex::new(RefCell::new(flash)),
        }
    }

    fn with_flash<R>(&self, f: impl FnOnce(&mut MemFlash) -> R) -> R {
        self.flash.lock(|flash| f(&mut flash.borrow_mut()))
    }

    /// Resets the device: runs the bootloader, which swaps or reverts images as needed
    fn try_boot(&self) -> Result<State, embassy_boot::BootError> {
        let config = BootLoaderConfig {
            active: Part::new(&self.flash, ACTIVE_START as u32, ACTIVE_SIZE as u32),
            dfu: Part::new(&self.flash, DFU_START as u32, (DFU_SECTORS * SECTOR) as u32),
            state: Part::new(
                &self.flash,
                STATE_START as u32,
                (STATE_SECTORS * SECTOR) as u32,
            ),
        };
        let mut buffer = AlignedBuffer([0u8; SECTOR]);
        BootLoader::new(config).prepare_boot(buffer.as_mut())
    }

    fn boot(&self) -> State {
        self.try_boot().expect("bootloader failed")
    }

    /// The firmware that would run after a reset, `len` bytes
    fn running_image(&self, len: usize) -> Vec<u8> {
        self.with_flash(|flash| flash.mem[ACTIVE_START..ACTIVE_START + len].to_vec())
    }

    fn operations(&self) -> usize {
        self.with_flash(|flash| flash.operations)
    }

    fn settings(&self, key: [u8; 32]) -> SettingsStore<Part<'_>> {
        SettingsStore::new(Part::new(&self.flash, SETTINGS_START as u32, SETTINGS_SIZE as u32), key)
    }

    fn slot<'a>(&'a self, aligned: &'a mut [u8; 1]) -> BootSlot<'a, Part<'a>, Part<'a>> {
        let config = FirmwareUpdaterConfig {
            dfu: Part::new(&self.flash, DFU_START as u32, (DFU_SECTORS * SECTOR) as u32),
            state: Part::new(
                &self.flash,
                STATE_START as u32,
                (STATE_SECTORS * SECTOR) as u32,
            ),
        };
        BootSlot::new(BlockingFirmwareUpdater::new(config, aligned), ACTIVE_SIZE)
    }
}

// ---------------------------------------------------------------- host

const DEVICE_ID: &str = "device1";
const SEED: [u8; 32] = [7; 32];
const OTHER_SEED: [u8; 32] = [9; 32];
const CURRENT_VERSION: u32 = ota::pack_version(0, 1, 0);
const NEWER_VERSION: u32 = ota::pack_version(0, 2, 0);

fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    salty::Keypair::from(seed).public.to_bytes()
}

/// Deterministic firmware-like bytes
fn image(len: usize, salt: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) as u8) ^ salt).collect()
}

/// What `ota sign` produces for a firmware image
fn sign(seed: &[u8; 32], version: u32, image: &[u8]) -> Manifest {
    let keypair = salty::Keypair::from(seed);
    let mut hasher = salty::Sha512::new();
    hasher.update(image);
    let digest = hasher.finalize();

    let mut manifest = Manifest {
        kind: Artifact::Firmware.kind(),
        version,
        size: image.len() as u32,
        content_signature: keypair.sign(&digest).to_bytes(),
        manifest_signature: [0; ota::SIGNATURE_LEN],
    };
    manifest.manifest_signature = keypair.sign(&manifest.signed_bytes()).to_bytes();
    manifest
}

/// What `ota sign-config` / `sign-registry` produce
fn sign_settings(seed: &[u8; 32], artifact: Artifact, revision: u32, data: &[u8]) -> Manifest {
    let keypair = salty::Keypair::from(seed);
    let prefix = ota::settings_prefix(artifact);
    let mut hasher = salty::Sha512::new();
    hasher.update(&prefix);
    hasher.update(data);
    let digest = hasher.finalize();

    let mut manifest = Manifest {
        kind: artifact.kind(),
        version: revision,
        size: data.len() as u32,
        content_signature: keypair.sign(&digest).to_bytes(),
        manifest_signature: [0; ota::SIGNATURE_LEN],
    };
    manifest.manifest_signature = keypair.sign(&manifest.signed_bytes()).to_bytes();
    manifest
}

/// The broker plus `ota publish`: serves chunks on request, with configurable misbehaviour.
#[derive(Default)]
struct HostLink {
    /// Retained manifests by artifact kind, as raw bytes so malformed ones can be offered too
    manifests: [Option<Vec<u8>>; 3],
    /// What the host serves per artifact kind, may differ from what was signed
    contents: [Vec<u8>; 3],
    /// Retained manifests still to be delivered in this session
    retained: VecDeque<(Artifact, Vec<u8>)>,
    subscribed: Vec<String>,
    inbox: VecDeque<Vec<u8>>,
    /// Status messages of every artifact, in order
    statuses: Vec<String>,
    /// Topics the statuses were published on
    status_topics: Vec<String>,
    requests: Vec<u32>,

    /// Ignore this many of the next requests
    lose_requests: usize,
    /// Ignore every request after this many were answered
    silent_after: Option<usize>,
    /// Answer every request twice
    duplicate_answers: bool,
    /// Before each answer repeat the previous chunk (a late answer to an earlier request)
    stale_first: bool,
    /// Answer with the wrong offset
    wrong_offset: bool,
    answered: usize,
    previous_answer: Option<Vec<u8>>,
}

impl HostLink {
    fn offering(manifest: &Manifest, image: Vec<u8>) -> Self {
        let mut link = Self::default();
        link.offer(manifest, image);
        link
    }

    /// Retains `manifest` for the artifact it says it is for and serves `content` for it
    fn offer(&mut self, manifest: &Manifest, content: Vec<u8>) -> &mut Self {
        let kind = manifest.kind as usize;
        self.manifests[kind] = Some(manifest.encode().to_vec());
        self.contents[kind] = content;
        self
    }

    fn topic(artifact: Artifact, leaf: &str) -> String {
        format!("{DEVICE_ID}/ota/{}/{leaf}", artifact.name())
    }

    fn manifest_filter() -> String {
        ota::topic_filter::<128>(DEVICE_ID, ota::TOPIC_MANIFEST).unwrap().to_string()
    }

    /// The artifact of `<device_id>/ota/<artifact>/<leaf>`
    fn artifact_of(topic: &str, leaf: &str) -> Option<Artifact> {
        ota::parse_topic(DEVICE_ID, topic).filter(|(_, l)| *l == leaf).map(|(artifact, _)| artifact)
    }

    fn handle_request(&mut self, artifact: Artifact, payload: &[u8]) {
        let offset = ota::decode_request(payload).expect("malformed request") as usize;
        self.requests.push(offset as u32);

        if self.lose_requests > 0 {
            self.lose_requests -= 1;
            return;
        }
        if self.silent_after.is_some_and(|limit| self.answered >= limit) {
            return;
        }
        self.answered += 1;

        let content = &self.contents[artifact.kind() as usize];
        let end = (offset + CHUNK_SIZE).min(content.len());
        let reported_offset = if self.wrong_offset { offset as u32 + 1 } else { offset as u32 };
        let mut message = reported_offset.to_le_bytes().to_vec();
        message.extend_from_slice(&content[offset..end]);

        if self.stale_first {
            if let Some(previous) = self.previous_answer.clone() {
                self.inbox.push_back(previous);
            }
        }
        self.inbox.push_back(message.clone());
        if self.duplicate_answers {
            self.inbox.push_back(message.clone());
        }
        self.previous_answer = Some(message);
    }
}

impl OtaLink for HostLink {
    async fn subscribe(&mut self, topic: &str) -> Result<(), OtaError> {
        self.subscribed.push(topic.to_owned());
        if topic == Self::manifest_filter() {
            // Retained messages arrive right after subscribing, once per session
            self.retained = Artifact::ALL
                .into_iter()
                .filter_map(|artifact| Some((artifact, self.manifests[artifact.kind() as usize].clone()?)))
                .collect();
        }
        Ok(())
    }

    async fn publish(&mut self, topic: &str, payload: &[u8]) -> Result<(), OtaError> {
        if let Some(artifact) = Self::artifact_of(topic, ota::TOPIC_REQUEST) {
            self.handle_request(artifact, payload);
        } else if Self::artifact_of(topic, ota::TOPIC_STATUS).is_some() {
            self.status_topics.push(topic.to_owned());
            self.statuses.push(String::from_utf8_lossy(payload).into_owned());
        } else {
            panic!("device published to unexpected topic {topic}");
        }
        Ok(())
    }

    async fn receive(
        &mut self,
        topics: &[&str],
        _wait: Duration,
        out: &mut [u8],
    ) -> Result<Option<(usize, usize)>, OtaError> {
        let (index, message) = if let Some(artifact) = Self::artifact_of(topics[0], ota::TOPIC_MANIFEST) {
            assert!(self.subscribed.contains(&Self::manifest_filter()), "receive without subscribing");
            let _ = artifact;
            let Some((artifact, manifest)) = self.retained.pop_front() else {
                return Ok(None);
            };
            let index = topics
                .iter()
                .position(|topic| *topic == Self::topic(artifact, ota::TOPIC_MANIFEST))
                .expect("device does not listen for this manifest");
            (index, manifest)
        } else if Self::artifact_of(topics[0], ota::TOPIC_CHUNK).is_some() {
            assert!(self.subscribed.iter().any(|t| t == topics[0]), "receive without subscribing to {}", topics[0]);
            let Some(chunk) = self.inbox.pop_front() else {
                return Ok(None);
            };
            (0, chunk)
        } else {
            panic!("device waits on unexpected topics {topics:?}");
        };

        let destination = out.get_mut(..message.len()).ok_or(OtaError::MessageTooLarge)?;
        destination.copy_from_slice(&message);
        Ok(Some((index, message.len())))
    }
}

// ---------------------------------------------------------------- helpers

/// Everything needed for one scenario: a device running v0.1.0 and a host offering `new_image`
struct Scenario {
    device: Device,
    old_image: Vec<u8>,
    new_image: Vec<u8>,
    key: [u8; 32],
}

impl Scenario {
    /// An update of `new_len` bytes (not a multiple of a page or a chunk) over a 6000 byte image
    fn new(new_len: usize) -> Self {
        let old_image = image(6000, 0x00);
        Self {
            device: Device::new(&old_image),
            new_image: image(new_len, 0x5A),
            old_image,
            key: public_key(&SEED),
        }
    }

    fn signed_offer(&self, version: u32) -> HostLink {
        HostLink::offering(
            &sign(&SEED, version, &self.new_image),
            self.new_image.clone(),
        )
    }

    fn config(&self) -> OtaConfig<'_> {
        OtaConfig {
            device_id: DEVICE_ID,
            public_key: &self.key,
            current_version: CURRENT_VERSION,
        }
    }

    fn run(&self, link: &mut HostLink) -> Result<Outcome, OtaError> {
        let mut aligned = [0u8; 1];
        let mut slot = self.device.slot(&mut aligned);
        let mut settings = self.device.settings(self.key);
        block_on(check_and_update(link, &mut slot, &mut settings, &self.config(), || {}))
    }

    fn assert_still_running_old_image(&self) {
        assert_eq!(self.device.boot(), State::Boot, "no swap may happen");
        assert_eq!(
            self.device.running_image(self.old_image.len()),
            self.old_image
        );
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn update_is_downloaded_verified_and_installed() {
    let scenario = Scenario::new(13_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);

    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(link.statuses, ["downloading", "ready"]);
    // 13000 bytes in 1024 byte chunks, each requested exactly once on a healthy link
    assert_eq!(
        link.requests,
        (0..13_000u32).step_by(CHUNK_SIZE).collect::<Vec<_>>()
    );

    // Reset: the bootloader swaps the new image in
    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(scenario.device.running_image(13_000), scenario.new_image);

    // The new image proves itself, from now on resets keep it
    let mut aligned = [0u8; 1];
    scenario.device.slot(&mut aligned).confirm_boot();
    assert_eq!(scenario.device.boot(), State::Boot);
    assert_eq!(scenario.device.running_image(13_000), scenario.new_image);
}

#[test]
fn image_that_never_confirms_itself_is_rolled_back() {
    let scenario = Scenario::new(13_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));

    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(scenario.device.running_image(13_000), scenario.new_image);

    // The new image crashes (or the watchdog fires) before confirming. The next reset reverts: the
    // bootloader reports the state it found (still `Swap`) and puts the previous image back.
    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(
        scenario.device.running_image(scenario.old_image.len()),
        scenario.old_image
    );
    // From then on the device reports `Revert` and keeps running the previous image
    assert_eq!(scenario.device.boot(), State::Revert);
    assert_eq!(
        scenario.device.running_image(scenario.old_image.len()),
        scenario.old_image
    );
}

#[test]
fn a_reverted_device_can_be_updated_again() {
    let scenario = Scenario::new(9_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    scenario.device.boot();
    scenario.device.boot(); // revert

    let mut retry = scenario.signed_offer(NEWER_VERSION);
    assert_eq!(scenario.run(&mut retry), Ok(Outcome::Ready));
    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(scenario.device.running_image(9_000), scenario.new_image);
}

#[test]
fn trial_image_does_not_start_another_update() {
    let scenario = Scenario::new(9_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(scenario.device.boot(), State::Swap);

    // Still on trial: even a genuine newer offer is left alone
    let mut next = scenario.signed_offer(ota::pack_version(0, 3, 0));
    assert_eq!(scenario.run(&mut next), Ok(Outcome::NoUpdate));
    assert!(next.subscribed.is_empty());
}

#[test]
fn no_manifest_means_no_update() {
    let scenario = Scenario::new(9_000);
    let mut link = HostLink::default();

    assert_eq!(scenario.run(&mut link), Ok(Outcome::NoUpdate));
    assert!(link.requests.is_empty());
    assert_eq!(scenario.device.operations(), 0, "flash must not be touched");
}

#[test]
fn same_or_older_versions_are_ignored() {
    for version in [CURRENT_VERSION, ota::pack_version(0, 0, 9)] {
        let scenario = Scenario::new(9_000);
        let mut link = scenario.signed_offer(version);

        assert_eq!(scenario.run(&mut link), Ok(Outcome::NoUpdate));
        assert!(link.requests.is_empty());
        assert_eq!(scenario.device.operations(), 0);
        scenario.assert_still_running_old_image();
    }
}

#[test]
fn manifest_signed_with_another_key_is_rejected() {
    let scenario = Scenario::new(9_000);
    let forged = sign(&OTHER_SEED, NEWER_VERSION, &scenario.new_image);
    let mut link = HostLink::offering(&forged, scenario.new_image.clone());

    assert_eq!(
        scenario.run(&mut link),
        Err(OtaError::ManifestSignatureInvalid)
    );
    assert_eq!(link.statuses, ["error: manifest signature invalid"]);
    assert!(link.requests.is_empty());
    assert_eq!(scenario.device.operations(), 0);
    scenario.assert_still_running_old_image();
}

#[test]
fn raising_the_version_of_a_genuine_manifest_is_detected() {
    // Replaying an old release as a "newer" one: the version is covered by the signature
    let scenario = Scenario::new(9_000);
    let mut manifest = sign(&SEED, ota::pack_version(0, 0, 5), &scenario.new_image);
    manifest.version = ota::pack_version(9, 9, 9);
    let mut link = HostLink::offering(&manifest, scenario.new_image.clone());

    assert_eq!(
        scenario.run(&mut link),
        Err(OtaError::ManifestSignatureInvalid)
    );
    assert_eq!(scenario.device.operations(), 0);
}

#[test]
fn malformed_manifests_are_rejected() {
    let scenario = Scenario::new(9_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.manifests[0].as_mut().unwrap().truncate(100);

    assert_eq!(scenario.run(&mut link), Err(OtaError::MalformedManifest));
    assert_eq!(scenario.device.operations(), 0);

    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.manifests[0].as_mut().unwrap().extend_from_slice(&[0; 40]);
    assert_eq!(scenario.run(&mut link), Err(OtaError::MessageTooLarge));
    assert_eq!(scenario.device.operations(), 0);
}

#[test]
fn image_larger_than_the_partition_is_refused_before_downloading() {
    let scenario = Scenario::new(ACTIVE_SIZE + 4);
    let mut link = scenario.signed_offer(NEWER_VERSION);

    assert_eq!(scenario.run(&mut link), Err(OtaError::ImageTooLarge));
    assert!(link.requests.is_empty());
    assert_eq!(scenario.device.operations(), 0);
}

#[test]
fn image_exactly_filling_the_partition_is_accepted() {
    let scenario = Scenario::new(ACTIVE_SIZE);
    let mut link = scenario.signed_offer(NEWER_VERSION);

    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(
        scenario.device.running_image(ACTIVE_SIZE),
        scenario.new_image
    );
}

#[test]
fn tampered_image_is_never_installed() {
    // The manifest is genuine but the bytes served differ from what was signed
    let scenario = Scenario::new(13_000);
    let mut tampered = scenario.new_image.clone();
    tampered[9_999] ^= 1;
    let mut link = HostLink::offering(&sign(&SEED, NEWER_VERSION, &scenario.new_image), tampered);

    assert_eq!(
        scenario.run(&mut link),
        Err(OtaError::ContentSignatureInvalid)
    );
    assert_eq!(
        link.statuses,
        ["downloading", "error: content signature invalid"]
    );
    scenario.assert_still_running_old_image();
}

#[test]
fn lost_chunk_requests_are_repeated() {
    let scenario = Scenario::new(5_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.lose_requests = 2; // the first chunk needs the third attempt

    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(&link.requests[..3], [0, 0, 0]);
    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(scenario.device.running_image(5_000), scenario.new_image);
}

#[test]
fn giving_up_after_three_unanswered_requests() {
    let scenario = Scenario::new(5_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.lose_requests = 3;

    assert_eq!(scenario.run(&mut link), Err(OtaError::ChunkTimeout));
    assert_eq!(link.requests, [0, 0, 0]);
    assert_eq!(link.statuses, ["downloading", "error: chunk timeout"]);
    scenario.assert_still_running_old_image();
}

#[test]
fn duplicated_answers_do_not_corrupt_the_image() {
    let scenario = Scenario::new(9_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.duplicate_answers = true;

    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(scenario.device.running_image(9_000), scenario.new_image);
}

#[test]
fn late_answers_to_earlier_requests_are_skipped() {
    let scenario = Scenario::new(9_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.stale_first = true;

    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(scenario.device.running_image(9_000), scenario.new_image);
}

#[test]
fn answers_for_the_wrong_offset_are_never_accepted() {
    let scenario = Scenario::new(9_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.wrong_offset = true;

    assert_eq!(scenario.run(&mut link), Err(OtaError::ChunkTimeout));
    scenario.assert_still_running_old_image();
}

#[test]
fn host_going_silent_midway_leaves_the_running_image_alone() {
    let scenario = Scenario::new(13_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.silent_after = Some(6);

    assert_eq!(scenario.run(&mut link), Err(OtaError::ChunkTimeout));
    scenario.assert_still_running_old_image();
}

#[test]
fn download_can_be_retried_after_it_was_aborted() {
    // The device keeps one updater for its whole life. Abort right after the first page was
    // written (5 chunks answered), then download a *different* release with the same updater:
    // sector 0 holds the first release's bytes and must be erased again, otherwise the new bytes are
    // ANDed onto the old ones and the signature check fails.
    let scenario = Scenario::new(13_000);
    let mut aligned = [0u8; 1];
    let mut slot = scenario.device.slot(&mut aligned);
    let mut settings = scenario.device.settings(scenario.key);

    let mut broken = scenario.signed_offer(NEWER_VERSION);
    broken.silent_after = Some(5);
    assert_eq!(
        block_on(check_and_update(
            &mut broken,
            &mut slot,
            &mut settings,
            &scenario.config(),
            || {}
        )),
        Err(OtaError::ChunkTimeout)
    );

    let fixed_release = image(13_000, 0xA5);
    let fixed_version = ota::pack_version(0, 2, 1);
    let mut healthy = HostLink::offering(
        &sign(&SEED, fixed_version, &fixed_release),
        fixed_release.clone(),
    );
    assert_eq!(
        block_on(check_and_update(
            &mut healthy,
            &mut slot,
            &mut settings,
            &scenario.config(),
            || {}
        )),
        Ok(Outcome::Ready)
    );
    drop(slot);
    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(scenario.device.running_image(13_000), fixed_release);
}

#[test]
fn power_cut_while_downloading_leaves_the_running_image_alone() {
    let scenario = Scenario::new(13_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    // Flash operations: the erase+write pairs of the download, cut somewhere in the middle
    scenario
        .device
        .with_flash(|flash| flash.fail_after = Some(4));

    assert_eq!(scenario.run(&mut link), Err(OtaError::FlashWrite));
    scenario.device.with_flash(|flash| flash.fail_after = None);
    scenario.assert_still_running_old_image();
}

#[test]
fn power_cut_while_the_bootloader_swaps_is_recovered() {
    let scenario = Scenario::new(13_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));

    // Cut the power after a few flash operations of the swap, then boot again
    scenario
        .device
        .with_flash(|flash| flash.fail_after = Some(12));
    assert!(scenario.device.try_boot().is_err());
    scenario.device.with_flash(|flash| flash.fail_after = None);

    assert_eq!(scenario.device.boot(), State::Swap);
    assert_eq!(scenario.device.running_image(13_000), scenario.new_image);
}

#[test]
fn progress_is_reported_for_every_chunk() {
    let scenario = Scenario::new(13_000);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    let mut aligned = [0u8; 1];
    let mut slot = scenario.device.slot(&mut aligned);
    let mut settings = scenario.device.settings(scenario.key);

    let mut ticks = 0;
    block_on(check_and_update(
        &mut link,
        &mut slot,
        &mut settings,
        &scenario.config(),
        || ticks += 1,
    ))
    .unwrap();
    assert_eq!(ticks, 13_000usize.div_ceil(CHUNK_SIZE));
}

#[test]
fn chunk_messages_have_the_documented_layout() {
    // Guards the wire format shared with `ota publish`
    assert_eq!(CHUNK_HEADER_LEN, 4);
    assert_eq!(ota::MANIFEST_LEN, 137);
    assert_eq!(ota::encode_request(0x0102_0304), [4, 3, 2, 1]);
}

// ---------------------------------------------------------------- settings

fn config_json(device_id: &str, endpoint: &str) -> Vec<u8> {
    format!(
        r#"{{"mqtt":{{"endpoint":"{endpoint}","device_id":"{device_id}","username":"user","password":"pass"}},
        "polling_interval_ms":5000,"reconnect_delay_ms":5000,
        "network":{{"name":"Network","password":"secret"}},
        "serial":{{"baud_rate":9600,"data_bits":8,"parity":"N","stop_bits":1}}}}"#
    )
    .into_bytes()
}

/// A configuration that needs several chunks
fn big_config(endpoint: &str) -> Vec<u8> {
    let mut config = config_json(DEVICE_ID, endpoint);
    config.extend(std::iter::repeat_n(b' ', 2_500));
    config
}

fn registry(lines: usize) -> Vec<u8> {
    (0..lines).map(|i| format!("1,{},i,f32,value_{i}\n", i * 2)).collect::<String>().into_bytes()
}

const ENDPOINT: &str = "192.168.1.10:1883";
const BUFFER: usize = MAX_SETTINGS_SIZE;

impl Scenario {
    /// A device with nothing but the old firmware, offered `data` as `artifact` at `revision`
    fn settings_offer(&self, artifact: Artifact, revision: u32, data: &[u8]) -> HostLink {
        HostLink::offering(&sign_settings(&SEED, artifact, revision, data), data.to_vec())
    }

    /// What the firmware does at boot: load the file (or learn there is none)
    fn load(&self, artifact: Artifact) -> Option<Vec<u8>> {
        let mut buffer = [0u8; BUFFER];
        let len = self.device.settings(self.key).load(artifact, &mut buffer).unwrap()?;
        Some(buffer[..len].to_vec())
    }

    /// The first polling cycle after boot worked
    fn confirm(&self) {
        self.device.settings(self.key).confirm();
    }

    /// Install `data`, boot, confirm: the device now runs on it
    fn install_confirmed(&self, artifact: Artifact, revision: u32, data: &[u8]) {
        let mut link = self.settings_offer(artifact, revision, data);
        assert_eq!(self.run(&mut link), Ok(Outcome::Ready));
        assert_eq!(self.load(artifact).as_deref(), Some(data));
        self.confirm();
    }
}

#[test]
fn a_config_is_downloaded_verified_and_stored() {
    let scenario = Scenario::new(9_000);
    let config = big_config(ENDPOINT);
    let mut link = scenario.settings_offer(Artifact::Config, 5, &config);

    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(link.statuses, ["downloading", "ready"]);
    assert_eq!(link.status_topics, ["device1/ota/config/status"; 2]);
    assert_eq!(link.requests, [0, 1024, 2048]);

    // The firmware is left alone, only the settings changed
    scenario.assert_still_running_old_image();
    assert_eq!(scenario.load(Artifact::Config), Some(config));
    assert_eq!(scenario.load(Artifact::Registry), None);
}

#[test]
fn a_registry_map_is_stored_the_same_way() {
    let scenario = Scenario::new(9_000);
    let map = registry(120);
    let mut link = scenario.settings_offer(Artifact::Registry, 1, &map);

    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(link.status_topics, ["device1/ota/registry/status"; 2]);
    assert_eq!(scenario.load(Artifact::Registry), Some(map));
    assert_eq!(scenario.load(Artifact::Config), None);
}

#[test]
fn settings_signed_with_another_key_are_rejected() {
    let scenario = Scenario::new(9_000);
    let config = config_json(DEVICE_ID, ENDPOINT);
    let mut link = HostLink::offering(&sign_settings(&OTHER_SEED, Artifact::Config, 5, &config), config);

    assert_eq!(scenario.run(&mut link), Err(OtaError::ManifestSignatureInvalid));
    assert_eq!(link.statuses, ["error: manifest signature invalid"]);
    assert!(link.requests.is_empty());
    assert_eq!(scenario.device.operations(), 0);
}

#[test]
fn a_signed_manifest_cannot_be_replayed_for_another_artifact() {
    let scenario = Scenario::new(9_000);
    let config = config_json(DEVICE_ID, ENDPOINT);
    let genuine = sign_settings(&SEED, Artifact::Config, 5, &config).encode().to_vec();

    // Offered as the registry map, then as firmware
    for artifact in [Artifact::Registry, Artifact::Firmware] {
        let mut link = HostLink::default();
        link.manifests[artifact.kind() as usize] = Some(genuine.clone());
        link.contents[artifact.kind() as usize] = config.clone();

        assert_eq!(scenario.run(&mut link), Err(OtaError::WrongArtifact));
        assert!(link.requests.is_empty());
        assert_eq!(link.status_topics, [format!("device1/ota/{}/status", artifact.name())]);
    }
    assert_eq!(scenario.device.operations(), 0);
}

#[test]
fn a_firmware_signature_is_not_a_settings_signature() {
    // Same bytes, signed as a firmware image: the settings check must not accept it
    let scenario = Scenario::new(9_000);
    let config = config_json(DEVICE_ID, ENDPOINT);
    let mut manifest = sign(&SEED, 5, &config);
    manifest.kind = Artifact::Config.kind();
    manifest.manifest_signature = salty::Keypair::from(&SEED).sign(&manifest.signed_bytes()).to_bytes();
    let mut link = HostLink::offering(&manifest, config);

    assert_eq!(scenario.run(&mut link), Err(OtaError::ContentSignatureInvalid));
    assert_eq!(scenario.load(Artifact::Config), None);
}

#[test]
fn tampered_settings_are_never_stored() {
    let scenario = Scenario::new(9_000);
    let config = big_config(ENDPOINT);
    let mut tampered = config.clone();
    tampered[100] ^= 1;
    let mut link = HostLink::offering(&sign_settings(&SEED, Artifact::Config, 5, &config), tampered);

    assert_eq!(scenario.run(&mut link), Err(OtaError::ContentSignatureInvalid));
    assert_eq!(link.statuses, ["downloading", "error: content signature invalid"]);
    assert_eq!(scenario.load(Artifact::Config), None);
    assert_eq!(scenario.device.operations(), 0);
}

#[test]
fn signed_but_unusable_settings_are_refused() {
    let scenario = Scenario::new(9_000);
    let cases: Vec<(Artifact, Vec<u8>, SettingsInvalid)> = vec![
        (Artifact::Config, b"{ not json".to_vec(), SettingsInvalid::Config),
        (Artifact::Config, config_json(DEVICE_ID, "localhost:1883"), SettingsInvalid::Endpoint),
        (Artifact::Config, config_json("another-device", ENDPOINT), SettingsInvalid::WrongDevice),
        (Artifact::Registry, b"1,0,x,f32,topic\n".to_vec(), SettingsInvalid::Registry(1)),
        (Artifact::Registry, b"1,0,i,f32,a\n\nnonsense\n".to_vec(), SettingsInvalid::Registry(3)),
        (Artifact::Registry, b"\n\n".to_vec(), SettingsInvalid::NoEntries),
    ];
    for (artifact, data, why) in cases {
        let mut link = scenario.settings_offer(artifact, 5, &data);
        assert_eq!(scenario.run(&mut link), Err(OtaError::InvalidSettings(why)), "{why:?}");
        assert_eq!(scenario.load(artifact), None);
        assert_eq!(scenario.device.operations(), 0, "flash must stay untouched ({why:?})");
    }
}

#[test]
fn settings_that_do_not_fit_are_refused_before_downloading() {
    let scenario = Scenario::new(9_000);
    for size in [0, MAX_SETTINGS_SIZE + 1] {
        let data = vec![b'x'; size];
        let mut link = scenario.settings_offer(Artifact::Registry, 5, &data);
        assert_eq!(scenario.run(&mut link), Err(OtaError::ImageTooLarge));
        assert!(link.requests.is_empty());
    }
    // The largest one that fits (spaces are not a map, so it fails *validation*, after downloading)
    let data = vec![b' '; MAX_SETTINGS_SIZE];
    let mut link = scenario.settings_offer(Artifact::Registry, 5, &data);
    assert_eq!(
        scenario.run(&mut link),
        Err(OtaError::InvalidSettings(SettingsInvalid::NoEntries))
    );
}

#[test]
fn only_newer_revisions_are_accepted() {
    let scenario = Scenario::new(9_000);
    let first = big_config(ENDPOINT);
    scenario.install_confirmed(Artifact::Config, 5, &first);

    for revision in [5, 4, 1] {
        let mut link = scenario.settings_offer(Artifact::Config, revision, &config_json(DEVICE_ID, "10.0.0.1:1883"));
        assert_eq!(scenario.run(&mut link), Ok(Outcome::NoUpdate), "revision {revision}");
        assert!(link.requests.is_empty());
    }
    assert_eq!(scenario.load(Artifact::Config), Some(first));

    let second = config_json(DEVICE_ID, "10.0.0.2:1883");
    let mut link = scenario.settings_offer(Artifact::Config, 6, &second);
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(scenario.load(Artifact::Config), Some(second));
}

#[test]
fn settings_that_never_proved_themselves_fall_back_to_the_previous_ones() {
    let scenario = Scenario::new(9_000);
    let (v1, v2, v3) = (
        config_json(DEVICE_ID, "10.0.0.1:1883"),
        config_json(DEVICE_ID, "10.0.0.2:1883"),
        config_json(DEVICE_ID, "10.0.0.3:1883"),
    );
    scenario.install_confirmed(Artifact::Config, 1, &v1);
    scenario.install_confirmed(Artifact::Config, 2, &v2);

    // Revision 3 is tried, but the device resets (bad Wi-Fi password, watchdog, ...) before confirming
    let mut link = scenario.settings_offer(Artifact::Config, 3, &v3);
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(scenario.load(Artifact::Config), Some(v3.clone()));
    assert_eq!(scenario.load(Artifact::Config), Some(v2.clone()), "next boot gives up on revision 3");
    assert_eq!(scenario.load(Artifact::Config), Some(v2.clone()));

    // The rejected revision doesn't come back, a newer one is fine
    let mut again = scenario.settings_offer(Artifact::Config, 3, &v3);
    assert_eq!(scenario.run(&mut again), Ok(Outcome::NoUpdate));
    let mut newer = scenario.settings_offer(Artifact::Config, 4, &v3);
    assert_eq!(scenario.run(&mut newer), Ok(Outcome::Ready));
    assert_eq!(scenario.load(Artifact::Config), Some(v3));
}

#[test]
fn without_a_previous_file_the_compiled_in_one_is_used() {
    let scenario = Scenario::new(9_000);
    let mut link = scenario.settings_offer(Artifact::Config, 1, &config_json(DEVICE_ID, ENDPOINT));
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));

    assert!(scenario.load(Artifact::Config).is_some());
    assert_eq!(scenario.load(Artifact::Config), None);
    assert_eq!(scenario.load(Artifact::Config), None);
}

#[test]
fn confirmed_settings_survive_any_number_of_resets() {
    let scenario = Scenario::new(9_000);
    let config = config_json(DEVICE_ID, ENDPOINT);
    scenario.install_confirmed(Artifact::Config, 1, &config);
    for _ in 0..5 {
        assert_eq!(scenario.load(Artifact::Config).as_deref(), Some(&config[..]));
    }
}

#[test]
fn a_file_the_firmware_cannot_use_is_rejected_and_the_previous_one_comes_back() {
    // E.g. a firmware update changed the configuration format
    let scenario = Scenario::new(9_000);
    let (v1, v2) = (config_json(DEVICE_ID, "10.0.0.1:1883"), config_json(DEVICE_ID, "10.0.0.2:1883"));
    scenario.install_confirmed(Artifact::Config, 1, &v1);
    scenario.install_confirmed(Artifact::Config, 2, &v2);

    let mut buffer = [0u8; BUFFER];
    let mut store = scenario.device.settings(scenario.key);
    let len = store
        .load_accepted(Artifact::Config, &mut buffer, |data| data.windows(8).all(|w| w != b"10.0.0.2"))
        .unwrap();
    assert_eq!(&buffer[..len.unwrap()], &v1[..]);
    // And it stays that way
    assert_eq!(scenario.load(Artifact::Config), Some(v1));
}

#[test]
fn power_cut_while_storing_keeps_the_previous_settings() {
    // The store is erase, file, header: cut after each of the three operations
    for cut in 0..3 {
        let scenario = Scenario::new(9_000);
        let old = config_json(DEVICE_ID, "10.0.0.1:1883");
        scenario.install_confirmed(Artifact::Config, 1, &old);

        let mut link = scenario.settings_offer(Artifact::Config, 2, &config_json(DEVICE_ID, "10.0.0.2:1883"));
        scenario.device.with_flash(|flash| flash.fail_after = Some(cut));
        assert_eq!(scenario.run(&mut link), Err(OtaError::FlashWrite), "cut after {cut} operations");
        scenario.device.with_flash(|flash| flash.fail_after = None);

        assert_eq!(scenario.load(Artifact::Config), Some(old), "cut after {cut} operations");
    }
}

#[test]
fn a_corrupted_slot_is_ignored() {
    let scenario = Scenario::new(9_000);
    let (v1, v2) = (config_json(DEVICE_ID, "10.0.0.1:1883"), config_json(DEVICE_ID, "10.0.0.2:1883"));
    scenario.install_confirmed(Artifact::Config, 1, &v1);
    scenario.install_confirmed(Artifact::Config, 2, &v2);

    // Flip a bit in the file of revision 2 (slot B): the signature check catches it
    scenario.device.with_flash(|flash| {
        let slot_b = SETTINGS_START + ota::slot_offset(Artifact::Config, 1).unwrap();
        flash.mem[slot_b + 200] &= !1;
    });
    assert_eq!(scenario.load(Artifact::Config), Some(v1));
}

#[test]
fn firmware_is_installed_before_settings() {
    let scenario = Scenario::new(9_000);
    let config = config_json(DEVICE_ID, ENDPOINT);
    let map = registry(10);
    let mut link = scenario.signed_offer(NEWER_VERSION);
    link.offer(&sign_settings(&SEED, Artifact::Config, 5, &config), config.clone());
    link.offer(&sign_settings(&SEED, Artifact::Registry, 6, &map), map.clone());

    // One artifact per session, the firmware first
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert!(link.status_topics.iter().all(|topic| topic == "device1/ota/firmware/status"));
    assert_eq!(scenario.load(Artifact::Config), None);

    // The host keeps offering the rest. Once the new firmware runs and proved itself, they follow.
    assert_eq!(scenario.device.boot(), State::Swap);
    let mut aligned = [0u8; 1];
    scenario.device.slot(&mut aligned).confirm_boot();
    link.manifests[Artifact::Firmware.kind() as usize] = None;
    link.statuses.clear();
    link.status_topics.clear();
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert!(link.status_topics.iter().all(|topic| topic == "device1/ota/config/status"));
    scenario.confirm();

    link.manifests[Artifact::Config.kind() as usize] = None;
    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(scenario.load(Artifact::Config), Some(config));
    assert_eq!(scenario.load(Artifact::Registry), Some(map));
}

#[test]
fn a_bad_artifact_does_not_block_the_others() {
    let scenario = Scenario::new(9_000);
    let map = registry(10);
    let mut link = HostLink::default();
    link.offer(&sign_settings(&OTHER_SEED, Artifact::Config, 5, b"{}"), b"{}".to_vec());
    link.offer(&sign_settings(&SEED, Artifact::Registry, 6, &map), map.clone());

    assert_eq!(scenario.run(&mut link), Ok(Outcome::Ready));
    assert_eq!(link.statuses[0], "error: manifest signature invalid");
    assert_eq!(link.status_topics[0], "device1/ota/config/status");
    assert_eq!(scenario.load(Artifact::Registry), Some(map));
}

#[test]
fn a_slot_image_made_for_probe_rs_is_picked_up_by_a_fresh_device() {
    let scenario = Scenario::new(9_000);
    let config = config_json(DEVICE_ID, ENDPOINT);
    let manifest = sign_settings(&SEED, Artifact::Config, 7, &config);
    let image = ota::encode_slot(Artifact::Config, 7, &manifest.content_signature, &config).unwrap();

    scenario.device.with_flash(|flash| {
        let at = SETTINGS_START + ota::slot_offset(Artifact::Config, 0).unwrap();
        flash.mem[at..at + image.len()].copy_from_slice(&image);
    });
    assert_eq!(scenario.load(Artifact::Config), Some(config.clone()));
    scenario.confirm();
    assert_eq!(scenario.load(Artifact::Config), Some(config));

    // Anything signed by someone else is ignored
    let other = Scenario::new(9_000);
    let forged = sign_settings(&OTHER_SEED, Artifact::Config, 7, b"{}");
    let image = ota::encode_slot(Artifact::Config, 7, &forged.content_signature, b"{}").unwrap();
    other.device.with_flash(|flash| {
        let at = SETTINGS_START + ota::slot_offset(Artifact::Config, 0).unwrap();
        flash.mem[at..at + image.len()].copy_from_slice(&image);
    });
    assert_eq!(other.load(Artifact::Config), None);
}

#[test]
fn slot_layout_and_validation_helpers() {
    assert_eq!(ota::slot_offset(Artifact::Config, 0), Some(0));
    assert_eq!(ota::slot_offset(Artifact::Config, 1), Some(4096));
    assert_eq!(ota::slot_offset(Artifact::Registry, 0), Some(8192));
    assert_eq!(ota::slot_offset(Artifact::Registry, 1), Some(12288));
    assert_eq!(ota::slot_offset(Artifact::Firmware, 0), None);
    assert_eq!(ota::slot_offset(Artifact::Config, 2), None);
    assert_eq!(ota::SETTINGS_REGION_SIZE, 16 * 1024);

    assert!(ota::encode_slot(Artifact::Firmware, 1, &[0; 64], b"x").is_none());
    assert!(ota::encode_slot(Artifact::Config, 1, &[0; 64], &[]).is_none());
    assert!(ota::encode_slot(Artifact::Config, 1, &[0; 64], &vec![0; MAX_SETTINGS_SIZE + 1]).is_none());

    assert!(ota::validate_registry(b"1,0,i,f32,a\n1,2,h,u16,b").is_ok());
    assert_eq!(ota::validate_registry(b"1,0,i,f32\n"), Err(SettingsInvalid::Registry(1)));
    assert_eq!(ota::validate_registry(&[0xFF, 0xFE]), Err(SettingsInvalid::NotUtf8));
    assert!(ota::validate_config(&config_json(DEVICE_ID, ENDPOINT)).is_ok());
}
