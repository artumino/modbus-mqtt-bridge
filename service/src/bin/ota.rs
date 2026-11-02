//! Host-side tooling for over-the-air firmware updates.
//!
//! ```text
//! ota keygen --out <dir>
//! ota sign --elf <elf> --key <ota_secret.key> --version x.y.z --out <dir>
//! ota sign-config --file configuration.json --key <ota_secret.key> --out <dir>
//! ota sign-registry --file registry_map --key <ota_secret.key> --out <dir>
//! ota publish --config configuration.json --dir <dir>
//! ota factory --bootloader-elf <elf> --firmware <firmware.bin> --out factory.bin
//! ```

use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer, SigningKey, Verifier};
use embedded_io_adapters::tokio_1::FromTokio;
use log::{error, info, warn};
use modbus_mqtt_bridge_core::{
    configuration::Configuration,
    ota::{self, Artifact},
};
use rust_mqtt::{
    Bytes,
    buffer::AllocBuffer,
    client::{
        Client,
        event::Event,
        options::{ConnectOptions, DisconnectOptions, PublicationOptions, SubscriptionOptions, TopicReference},
    },
    types::{MqttBinary, MqttString, TopicFilter, TopicName},
};
use sha2::{Digest, Sha512};

/// Start of the ACTIVE partition, see `embedded/memory.x`
const ACTIVE_START: u32 = 0x1000_7000;
/// End of the ACTIVE partition (exclusive)
const ACTIVE_END: u32 = ACTIVE_START + 1000 * 1024;
/// Give up when the device stays silent for this long
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

fn ctx<E: std::fmt::Debug>(what: &'static str) -> impl FnOnce(E) -> anyhow::Error {
    move |e| anyhow!("{what}: {e:?}")
}

/// Start of the bootloader (BOOT2 included), the end is where ACTIVE starts
const FLASH_START: u32 = 0x1000_0000;
/// Start of the settings partition, see `embedded/memory.x`
const SETTINGS_START: u32 = 0x101F_C000;

/// `<dir>/<artifact>.bin` is what the device downloads, `<dir>/<artifact>.manifest` describes it
fn content_file(artifact: Artifact) -> String {
    format!("{}.bin", artifact.name())
}

fn manifest_file(artifact: Artifact) -> String {
    format!("{}.manifest", artifact.name())
}

#[derive(Parser)]
#[command(about = "Sign and distribute firmware updates for the modbus-mqtt bridge")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate an ed25519 signing key pair
    Keygen {
        /// Directory receiving `ota_secret.key` (keep private) and `ota_public.key` (embed in the firmware)
        #[arg(long)]
        out: PathBuf,
    },
    /// Turn a release ELF into a signed `firmware.bin` + `firmware.manifest`
    Sign {
        /// ELF of the embedded application (`target/thumbv6m-none-eabi/release/...`)
        #[arg(long)]
        elf: PathBuf,
        /// `ota_secret.key` created by `keygen`
        #[arg(long)]
        key: PathBuf,
        /// Version of the image, must match the version in the firmware's Cargo.toml (`major.minor.patch`)
        #[arg(long)]
        version: String,
        /// Output directory
        #[arg(long)]
        out: PathBuf,
    },
    /// Sign a device configuration (`configuration.json`) into `config.bin` + `config.manifest`
    /// (+ `config.slot`, to flash with a probe)
    SignConfig {
        /// The file to sign, same format the firmware has compiled in
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        key: PathBuf,
        /// Must be higher than the revision the device has. Defaults to the current unix time.
        #[arg(long)]
        revision: Option<u32>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Sign a registry map into `registry.bin` + `registry.manifest` (+ `registry.slot`)
    SignRegistry {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        revision: Option<u32>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Offer the signed artifacts found in a directory (firmware, config, registry) to the device
    /// through the MQTT broker and serve its chunk requests
    Publish {
        /// Same `configuration.json` the bridge uses
        #[arg(long)]
        config: PathBuf,
        /// Directory containing the output of `sign`, `sign-config` and/or `sign-registry`
        #[arg(long)]
        dir: PathBuf,
    },
    /// Join the bootloader and a signed firmware image into one file for a blank chip
    Factory {
        /// ELF of the bootloader
        #[arg(long)]
        bootloader_elf: PathBuf,
        /// `firmware.bin` made by `sign`
        #[arg(long)]
        firmware: PathBuf,
        /// Written to flash at 0x10000000: `probe-rs download --chip RP2040 --binary-format bin --base-address 0x10000000 <file>`
        #[arg(long)]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match Cli::parse().command {
        Command::Keygen { out } => keygen(&out),
        Command::Sign {
            elf,
            key,
            version,
            out,
        } => sign(&elf, &key, &version, &out),
        Command::SignConfig { file, key, revision, out } => {
            sign_settings(Artifact::Config, &file, &key, revision, &out)
        }
        Command::SignRegistry { file, key, revision, out } => {
            sign_settings(Artifact::Registry, &file, &key, revision, &out)
        }
        Command::Publish { config, dir } => publish(&config, &dir).await,
        Command::Factory { bootloader_elf, firmware, out } => factory(&bootloader_elf, &firmware, &out),
    }
}

fn keygen(out: &Path) -> Result<()> {
    fs::create_dir_all(out)?;
    let secret_path = out.join("ota_secret.key");
    let public_path = out.join("ota_public.key");

    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow!("no randomness available: {e}"))?;
    let signing_key = SigningKey::from_bytes(&seed);

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(&secret_path)
        .with_context(|| format!("refusing to overwrite {}", secret_path.display()))?
        .write_all(&seed)?;
    fs::write(&public_path, signing_key.verifying_key().to_bytes())?;

    info!("Secret key: {} (never commit this)", secret_path.display());
    info!("Public key: {} (copy to embedded/keys/ota_public.key)", public_path.display());
    Ok(())
}

fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let seed: [u8; 32] = fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?
        .try_into()
        .map_err(|_| anyhow!("{} is not a 32 byte ed25519 secret key", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Flattens the loadable segments of a 32 bit little endian ELF that live in `start..end` into a
/// flat binary image starting at `start`. There has to be a segment at `start`.
fn flatten_elf(elf: &[u8], start: u32, end: u32) -> Result<Vec<u8>> {
    fn u16_at(data: &[u8], at: usize) -> Result<u16> {
        Ok(u16::from_le_bytes(data.get(at..at + 2).context("truncated ELF")?.try_into()?))
    }
    fn u32_at(data: &[u8], at: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(data.get(at..at + 4).context("truncated ELF")?.try_into()?))
    }

    if elf.get(..4) != Some(b"\x7fELF") || elf.get(4) != Some(&1) || elf.get(5) != Some(&1) {
        bail!("not a 32 bit little endian ELF file");
    }
    let ph_offset = u32_at(elf, 28)? as usize;
    let ph_size = u16_at(elf, 42)? as usize;
    let ph_count = u16_at(elf, 44)? as usize;

    let mut image: Vec<u8> = Vec::new();
    let mut found_start = false;
    for i in 0..ph_count {
        let ph = ph_offset + i * ph_size;
        const PT_LOAD: u32 = 1;
        if u32_at(elf, ph)? != PT_LOAD {
            continue;
        }
        let offset = u32_at(elf, ph + 4)? as usize;
        let load_address = u32_at(elf, ph + 12)?;
        let file_size = u32_at(elf, ph + 16)? as usize;
        if file_size == 0 || !(start..end).contains(&load_address) {
            continue;
        }
        let segment_end = (load_address - start) as usize + file_size;
        if load_address + file_size as u32 > end {
            bail!("segment at {load_address:#x} does not fit in {start:#x}..{end:#x}");
        }
        found_start |= load_address == start;
        if image.len() < segment_end {
            image.resize(segment_end, 0xFF);
        }
        let data = elf.get(offset..offset + file_size).context("truncated ELF segment")?;
        image[(load_address - start) as usize..segment_end].copy_from_slice(data);
    }

    if image.is_empty() || !found_start {
        bail!("no segment loaded at {start:#x}: was the ELF linked with the memory layout of this project?");
    }
    Ok(image)
}

fn sign(elf_path: &Path, key_path: &Path, version: &str, out: &Path) -> Result<()> {
    let version = ota::parse_version(version).context("version must look like major.minor.patch")?;
    let mut image = flatten_elf(
        &fs::read(elf_path).with_context(|| format!("reading {}", elf_path.display()))?,
        ACTIVE_START,
        ACTIVE_END,
    )?;
    // The bootloader and the updater work on whole words
    image.resize(image.len().next_multiple_of(4), 0xFF);

    sign_and_write(&load_signing_key(key_path)?, Artifact::Firmware, version, &image, out)?;
    info!(
        "Signed {} bytes as version {}.{}.{} into {}",
        image.len(),
        version >> 16,
        (version >> 8) & 0xFF,
        version & 0xFF,
        out.display()
    );
    Ok(())
}

/// Signs a configuration or registry map: validates it exactly like the device will, then writes
/// the content, its manifest and the slot image for a probe
fn sign_settings(artifact: Artifact, file: &Path, key_path: &Path, revision: Option<u32>, out: &Path) -> Result<()> {
    let data = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let invalid = |e: ota::SettingsInvalid| match e {
        ota::SettingsInvalid::Registry(line) => anyhow!("{}: malformed line {line}", file.display()),
        other => anyhow!("{}: {}", file.display(), other.reason()),
    };
    if artifact == Artifact::Config {
        let config = ota::validate_config(&data).map_err(invalid)?;
        info!("Configuration of device `{}`", config.mqtt.device_id);
        warn!(
            "The configuration travels through the MQTT broker signed but not encrypted: \
             the Wi-Fi and MQTT credentials in it are readable by anyone who can subscribe to the topic"
        );
    } else {
        ota::validate_registry(&data).map_err(invalid)?;
    }

    let revision = match revision {
        Some(revision) => revision,
        None => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs().try_into()?,
    };
    let manifest = sign_and_write(&load_signing_key(key_path)?, artifact, revision, &data, out)?;

    let slot = ota::encode_slot(artifact, revision, &manifest.content_signature, &data).context("file too large")?;
    let slot_file = out.join(format!("{}.slot", artifact.name()));
    fs::write(&slot_file, &slot)?;
    info!(
        "Signed {} ({} bytes) as revision {revision} into {}. To provision a device with a probe: \
         probe-rs download --chip RP2040 --binary-format bin --base-address {:#x} {}",
        artifact.name(),
        data.len(),
        out.display(),
        SETTINGS_START + ota::slot_offset(artifact, 0).context("not a settings file")? as u32,
        slot_file.display()
    );
    Ok(())
}

/// Signs `content` as `artifact`, checks the signatures the way the device will (catches a corrupted
/// key file before anything is shipped) and writes `<artifact>.bin` and `<artifact>.manifest` to `out`
fn sign_and_write(
    key: &SigningKey,
    artifact: Artifact,
    version: u32,
    content: &[u8],
    out: &Path,
) -> Result<ota::Manifest> {
    let digest = match artifact {
        Artifact::Firmware => Sha512::digest(content),
        _ => Sha512::new().chain_update(ota::settings_prefix(artifact)).chain_update(content).finalize(),
    };
    let mut manifest = ota::Manifest {
        kind: artifact.kind(),
        version,
        size: content.len() as u32,
        content_signature: key.sign(&digest).to_bytes(),
        manifest_signature: [0; ota::SIGNATURE_LEN],
    };
    manifest.manifest_signature = key.sign(&manifest.signed_bytes()).to_bytes();

    let verifying_key = key.verifying_key();
    verifying_key.verify(&digest, &ed25519_dalek::Signature::from_bytes(&manifest.content_signature))?;
    verifying_key.verify(&manifest.signed_bytes(), &ed25519_dalek::Signature::from_bytes(&manifest.manifest_signature))?;

    fs::create_dir_all(out)?;
    fs::write(out.join(content_file(artifact)), content)?;
    fs::write(out.join(manifest_file(artifact)), manifest.encode())?;
    Ok(manifest)
}

/// Bootloader + firmware in one file that goes to the start of the flash of a blank chip
fn factory(bootloader_elf: &Path, firmware: &Path, out: &Path) -> Result<()> {
    let mut image = flatten_elf(
        &fs::read(bootloader_elf).with_context(|| format!("reading {}", bootloader_elf.display()))?,
        FLASH_START,
        ACTIVE_START,
    )?;
    image.resize((ACTIVE_START - FLASH_START) as usize, 0xFF);
    let firmware = fs::read(firmware).with_context(|| format!("reading {}", firmware.display()))?;
    if firmware.len() > (ACTIVE_END - ACTIVE_START) as usize {
        bail!("the firmware does not fit in the ACTIVE partition");
    }
    image.extend_from_slice(&firmware);
    fs::write(out, &image)?;
    info!("Wrote {} bytes (bootloader and firmware) to {}", image.len(), out.display());
    Ok(())
}

/// One signed thing to offer to the device
struct Offer {
    artifact: Artifact,
    content: Vec<u8>,
    manifest: ota::Manifest,
}

/// Reads the output of `sign`, `sign-config` and `sign-registry` and checks it is self-consistent
/// before it goes anywhere near a device
fn load_offers(dir: &Path) -> Result<Vec<Offer>> {
    let mut offers = Vec::new();
    for artifact in Artifact::ALL {
        let manifest_path = dir.join(manifest_file(artifact));
        if !manifest_path.exists() {
            continue;
        }
        let content = fs::read(dir.join(content_file(artifact)))
            .with_context(|| format!("{} exists but {} does not", manifest_path.display(), content_file(artifact)))?;
        let manifest = ota::Manifest::decode(&fs::read(&manifest_path)?).context("invalid manifest file")?;
        if manifest.kind != artifact.kind() || manifest.size as usize != content.len() {
            bail!("{} does not describe {}", manifest_path.display(), content_file(artifact));
        }
        offers.push(Offer { artifact, content, manifest });
    }
    if offers.is_empty() {
        bail!("nothing to publish in {}: run `sign`, `sign-config` or `sign-registry` first", dir.display());
    }
    Ok(offers)
}

fn mqtt_topic(topic: Option<impl std::ops::Deref<Target = str>>) -> Result<MqttString<'static>> {
    MqttString::try_from(topic.context("topic too long")?.to_owned()).map_err(ctx("topic"))
}

/// Where an artifact's `leaf` is published to
fn publication(device_id: &str, artifact: Artifact, leaf: &str) -> Result<PublicationOptions<'static>> {
    let name = TopicName::new(mqtt_topic(ota::topic::<128>(device_id, artifact, leaf))?).context("invalid topic name")?;
    Ok(PublicationOptions::new(TopicReference::Name(name)))
}

async fn publish(config_path: &Path, dir: &Path) -> Result<()> {
    let offers = load_offers(dir)?;
    let config_string = fs::read_to_string(config_path)?;
    let config: Configuration = serde_json::from_str(&config_string)?;
    let device_id = config.mqtt.device_id;

    let mut buffer = AllocBuffer;
    let mut client = Client::<'_, '_, _, _, 2, 1, 1, 1, 16, 0, 0>::new(&mut buffer);
    let connection = FromTokio::new(
        tokio::net::TcpStream::connect(SocketAddr::from_str(config.mqtt.endpoint)?).await?,
    );

    // A different client id than the device: the broker would kick the device off otherwise
    let client_id = format!("{device_id}-ota");
    client
        .connect(
            connection,
            &ConnectOptions::new()
                .clean_start()
                .user_name(MqttString::try_from(config.mqtt.username).map_err(ctx("username"))?)
                .password(MqttBinary::try_from(config.mqtt.password).map_err(ctx("password"))?),
            Some(MqttString::try_from(client_id.as_str()).map_err(ctx("client id"))?),
        )
        .await
        .map_err(ctx("connecting to the broker"))?;
    info!("Connected to the broker");

    // Requests and statuses of all artifacts
    for leaf in [ota::TOPIC_REQUEST, ota::TOPIC_STATUS] {
        let filter = TopicFilter::new(mqtt_topic(ota::topic_filter::<128>(device_id, leaf))?).context("invalid topic filter")?;
        client
            .subscribe(filter, &SubscriptionOptions::new())
            .await
            .map_err(ctx("subscribing"))?;
    }

    let mut retained = Vec::new();
    for offer in &offers {
        let options = publication(device_id, offer.artifact, ota::TOPIC_MANIFEST)?.retain();
        client
            .publish(&options, Bytes::from(&offer.manifest.encode()[..]))
            .await
            .map_err(ctx("publishing the manifest"))?;
        info!("Offered {} ({} bytes)", offer.artifact.name(), offer.content.len());
        retained.push(options);
    }
    info!("Waiting for the device to pick it up...");

    let result = serve(&mut client, device_id, &offers).await;

    // Whatever happened, stop offering (an empty retained message clears it). Once installed, the
    // device would ignore the manifest anyway: it is not newer than what it has.
    for options in &retained {
        if let Err(e) = client.publish(options, Bytes::from(&[][..])).await {
            error!("Failed to clear a retained manifest: {e:?}");
        }
    }
    let _ = client.disconnect(&DisconnectOptions::new()).await;
    result
}

/// Answers chunk requests until the device reported every offer as installed
async fn serve<const N: usize>(
    client: &mut Client<'_, '_, impl rust_mqtt::io::Transport, AllocBuffer, 2, 1, 1, 1, 16, 0, N>,
    device_id: &str,
    offers: &[Offer],
) -> Result<()> {
    let mut pending: Vec<Artifact> = offers.iter().map(|offer| offer.artifact).collect();
    let mut last_progress = usize::MAX;
    while !pending.is_empty() {
        let event = tokio::time::timeout(IDLE_TIMEOUT, client.poll())
            .await
            .context("the device went silent")?
            .map_err(|e| anyhow!("polling the broker: {e:?}"))?;
        let Event::Publish(received) = event else {
            continue;
        };
        let TopicReference::Name(topic) = &received.topic else {
            continue;
        };
        let message = received.message.as_bytes();
        let Some((artifact, leaf)) = ota::parse_topic(device_id, topic.as_ref().as_str()) else {
            continue;
        };
        let Some(offer) = offers.iter().find(|offer| offer.artifact == artifact) else {
            continue;
        };
        let name = artifact.name();

        if leaf == ota::TOPIC_REQUEST {
            let content = &offer.content;
            let Some(offset) = ota::decode_request(message).map(|offset| offset as usize) else {
                continue;
            };
            if offset >= content.len() {
                continue;
            }
            let end = (offset + ota::CHUNK_SIZE).min(content.len());
            let mut payload = ota::encode_request(offset as u32).to_vec();
            payload.extend_from_slice(&content[offset..end]);
            client
                .publish(&publication(device_id, artifact, ota::TOPIC_CHUNK)?, Bytes::from(payload))
                .await
                .map_err(|e| anyhow!("publishing a chunk: {e:?}"))?;

            let progress = end * 100 / content.len();
            if progress / 10 != last_progress / 10 {
                info!("Sent {name}: {progress}%");
                last_progress = progress;
            }
        } else if leaf == ota::TOPIC_STATUS {
            let status = String::from_utf8_lossy(message);
            info!("Device status ({name}): {status}");
            if status.starts_with(ota::STATUS_READY) {
                info!("The device verified the {name} and will restart to use it");
                pending.retain(|pending| *pending != artifact);
                last_progress = usize::MAX;
            } else if status.starts_with(ota::STATUS_ERROR) {
                bail!("the device rejected the {name}: {status}");
            }
        }
    }
    Ok(())
}
