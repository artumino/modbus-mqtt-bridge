//! Wire format shared by the firmware and the host-side `ota` tool.
//!
//! Three artifacts can be updated: the firmware image and the two settings files (the device
//! configuration and the registry map). All of them are signed with the same ed25519 key.
//! - `content_signature` signs the SHA-512 digest of the content. For firmware that is the image
//!   (this is what `embassy-boot` verifies against the DFU partition before arming the swap), for
//!   settings it is the digest of [`SETTINGS_DOMAIN`] `| kind | file`.
//! - `manifest_signature` signs `kind | version | size | content_signature`, binding the artifact
//!   type and the version (used for downgrade protection) to the exact content. For settings the
//!   "version" is a revision number that only ever grows.

#[cfg(feature = "ota")]
mod boot;
mod settings;
#[cfg(feature = "ota")]
mod update;

#[cfg(feature = "ota")]
pub use boot::BootSlot;
pub use settings::{
    MAX_SETTINGS_SIZE, SETTINGS_REGION_SIZE, SETTINGS_SLOT_SIZE, SettingsInvalid, SlotImage,
    encode_slot, slot_offset, validate_config, validate_registry,
};
#[cfg(feature = "ota")]
pub use settings::SettingsStore;
#[cfg(feature = "ota")]
pub use update::{OtaConfig, OtaError, OtaLink, Outcome, check_and_update};

use heapless::String;

pub const SIGNATURE_LEN: usize = 64;
/// Payload size of a single firmware chunk
pub const CHUNK_SIZE: usize = 1024;
/// kind (1) + version (4) + size (4) + content signature (64) + manifest signature (64)
pub const MANIFEST_LEN: usize = 1 + 4 + 4 + SIGNATURE_LEN + SIGNATURE_LEN;
/// Length of the part of the manifest covered by `manifest_signature`
pub const MANIFEST_SIGNED_LEN: usize = MANIFEST_LEN - SIGNATURE_LEN;
/// Length of a chunk message header (the offset of the chunk)
pub const CHUNK_HEADER_LEN: usize = 4;

// Topics are `<device_id>/ota/<artifact>/<leaf>`
/// Retained, published by the host: describes the available update
pub const TOPIC_MANIFEST: &str = "manifest";
/// Device -> host: asks for the chunk at the given offset
pub const TOPIC_REQUEST: &str = "request";
/// Host -> device: the requested chunk
pub const TOPIC_CHUNK: &str = "chunk";
/// Device -> host: textual progress/outcome
pub const TOPIC_STATUS: &str = "status";

/// Prefix hashed in front of settings files before they are signed, so a signature made for one
/// kind of content can never be mistaken for another
pub const SETTINGS_DOMAIN: &[u8] = b"modbus-mqtt-bridge/settings/v1";

/// What can be updated over the air. The discriminant is the `kind` byte of the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u8)]
pub enum Artifact {
    Firmware = 0,
    Config = 1,
    Registry = 2,
}

impl Artifact {
    /// In the order the device serves them when several are on offer
    pub const ALL: [Artifact; 3] = [Artifact::Firmware, Artifact::Config, Artifact::Registry];

    pub const fn kind(self) -> u8 {
        self as u8
    }

    /// Name used in topics and file names
    pub const fn name(self) -> &'static str {
        match self {
            Artifact::Firmware => "firmware",
            Artifact::Config => "config",
            Artifact::Registry => "registry",
        }
    }
}

pub const STATUS_DOWNLOADING: &str = "downloading";
pub const STATUS_READY: &str = "ready";
pub const STATUS_ERROR: &str = "error";

fn join<const N: usize>(parts: &[&str]) -> Option<String<N>> {
    let mut topic = String::new();
    for part in parts {
        topic.push_str(part).ok()?;
    }
    Some(topic)
}

/// Builds `<device_id>/ota/<artifact>/<leaf>`, `None` if it does not fit in `N` bytes
pub fn topic<const N: usize>(device_id: &str, artifact: Artifact, leaf: &str) -> Option<String<N>> {
    join(&[device_id, "/ota/", artifact.name(), "/", leaf])
}

/// Subscription matching `leaf` of every artifact, `<device_id>/ota/+/<leaf>`
pub fn topic_filter<const N: usize>(device_id: &str, leaf: &str) -> Option<String<N>> {
    join(&[device_id, "/ota/+/", leaf])
}

/// Splits `<device_id>/ota/<artifact>/<leaf>` into the artifact and the leaf
pub fn parse_topic<'t>(device_id: &str, topic: &'t str) -> Option<(Artifact, &'t str)> {
    let (name, leaf) = topic.strip_prefix(device_id)?.strip_prefix("/ota/")?.split_once('/')?;
    let artifact = Artifact::ALL.into_iter().find(|artifact| artifact.name() == name)?;
    Some((artifact, leaf))
}

/// Packs a semantic version as `major << 16 | minor << 8 | patch`
pub const fn pack_version(major: u8, minor: u8, patch: u8) -> u32 {
    ((major as u32) << 16) | ((minor as u32) << 8) | patch as u32
}

/// Parses `major.minor.patch` (e.g. `env!("CARGO_PKG_VERSION")`) into a packed version.
/// Returns `None` for anything else, pre-release suffixes included.
pub const fn parse_version(version: &str) -> Option<u32> {
    let bytes = version.as_bytes();
    let mut parts = [0u32; 3];
    let mut part = 0;
    let mut digits = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'.' {
            if digits == 0 || part == 2 {
                return None;
            }
            part += 1;
            digits = 0;
        } else if b >= b'0' && b <= b'9' {
            parts[part] = parts[part] * 10 + (b - b'0') as u32;
            if parts[part] > 255 {
                return None;
            }
            digits += 1;
        } else {
            return None;
        }
        i += 1;
    }
    if part != 2 || digits == 0 {
        return None;
    }
    Some(pack_version(parts[0] as u8, parts[1] as u8, parts[2] as u8))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// [`Artifact::kind`]
    pub kind: u8,
    /// Firmware: packed version. Settings: revision.
    pub version: u32,
    /// Size of the content in bytes
    pub size: u32,
    pub content_signature: [u8; SIGNATURE_LEN],
    pub manifest_signature: [u8; SIGNATURE_LEN],
}

impl Manifest {
    pub fn encode(&self) -> [u8; MANIFEST_LEN] {
        let mut out = [0u8; MANIFEST_LEN];
        out[..MANIFEST_SIGNED_LEN].copy_from_slice(&self.signed_bytes());
        out[MANIFEST_SIGNED_LEN..].copy_from_slice(&self.manifest_signature);
        out
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() != MANIFEST_LEN {
            return None;
        }
        Some(Self {
            kind: data[0],
            version: u32::from_le_bytes(data[1..5].try_into().ok()?),
            size: u32::from_le_bytes(data[5..9].try_into().ok()?),
            content_signature: data[9..9 + SIGNATURE_LEN].try_into().ok()?,
            manifest_signature: data[MANIFEST_SIGNED_LEN..].try_into().ok()?,
        })
    }

    /// The bytes covered by `manifest_signature`
    pub fn signed_bytes(&self) -> [u8; MANIFEST_SIGNED_LEN] {
        let mut out = [0u8; MANIFEST_SIGNED_LEN];
        out[0] = self.kind;
        out[1..5].copy_from_slice(&self.version.to_le_bytes());
        out[5..9].copy_from_slice(&self.size.to_le_bytes());
        out[9..].copy_from_slice(&self.content_signature);
        out
    }
}

/// A settings file is signed as SHA-512(`domain | kind | file`), this is the part in front of the file
pub fn settings_prefix(artifact: Artifact) -> [u8; SETTINGS_DOMAIN.len() + 1] {
    let mut prefix = [artifact.kind(); SETTINGS_DOMAIN.len() + 1];
    prefix[..SETTINGS_DOMAIN.len()].copy_from_slice(SETTINGS_DOMAIN);
    prefix
}

pub fn encode_request(offset: u32) -> [u8; 4] {
    offset.to_le_bytes()
}

pub fn decode_request(data: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(data.try_into().ok()?))
}

/// Splits a chunk message into its offset and payload
pub fn decode_chunk(data: &[u8]) -> Option<(u32, &[u8])> {
    if data.len() < CHUNK_HEADER_LEN {
        return None;
    }
    let (header, payload) = data.split_at(CHUNK_HEADER_LEN);
    Some((u32::from_le_bytes(header.try_into().ok()?), payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("0.1.0"), Some(0x000100));
        assert_eq!(parse_version("1.2.3"), Some(0x010203));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.3-rc1"), None);
        assert_eq!(parse_version("1..3"), None);
        assert_eq!(parse_version("256.0.0"), None);
    }

    #[test]
    fn manifest_roundtrip() {
        let manifest = Manifest {
            kind: Artifact::Config.kind(),
            version: 0x000102,
            size: 123_456,
            content_signature: [7; SIGNATURE_LEN],
            manifest_signature: [9; SIGNATURE_LEN],
        };
        let encoded = manifest.encode();
        assert_eq!(Manifest::decode(&encoded), Some(manifest));
        assert_eq!(Manifest::decode(&encoded[1..]), None);
    }

    #[test]
    fn chunk_and_request() {
        assert_eq!(decode_request(&encode_request(4096)), Some(4096));
        assert_eq!(decode_request(&[1, 2]), None);
        let msg = [0, 4, 0, 0, 0xAA, 0xBB];
        assert_eq!(decode_chunk(&msg), Some((1024, &[0xAA, 0xBB][..])));
        assert_eq!(decode_chunk(&[1, 2]), None);
    }

    #[test]
    fn topic_building() {
        assert_eq!(
            topic::<32>("dev", Artifact::Config, TOPIC_STATUS).as_deref(),
            Some("dev/ota/config/status")
        );
        assert_eq!(topic::<4>("dev", Artifact::Config, TOPIC_STATUS), None);
        assert_eq!(topic_filter::<32>("dev", TOPIC_MANIFEST).as_deref(), Some("dev/ota/+/manifest"));
        assert_eq!(parse_topic("dev", "dev/ota/registry/request"), Some((Artifact::Registry, "request")));
        assert_eq!(parse_topic("dev", "dev/ota/nothing/request"), None);
        assert_eq!(parse_topic("dev", "other/ota/config/request"), None);
    }
}
