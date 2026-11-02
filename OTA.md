# Over-the-air updates

The bridge firmware, its configuration (`configuration.json`) and its registry map can be updated
through the MQTT broker it already talks to. Only files signed with your private key are used, older
ones are refused, and anything that cannot complete a polling cycle is rolled back automatically.

```
bootloader/   embassy-boot-rp bootloader (swaps / reverts images), flashed once with a probe
embedded/     the application, runs from the ACTIVE partition (src/ota.rs holds the update logic)
service/      `ota` tool: keygen, sign, sign-config, sign-registry, publish, factory
core/src/ota.rs   wire format shared by firmware and tool (core/src/ota/ has the update logic)
```

Flash layout (2 MB, `bootloader/memory.x` and `embedded/memory.x` must stay in sync):

| Region | Address | Size |
|---|---|---|
| BOOT2 + bootloader | 0x10000000 | 24K |
| Bootloader state | 0x10006000 | 4K |
| ACTIVE (running image) | 0x10007000 | 1000K |
| DFU (downloaded image) | 0x10101000 | 1004K |
| Settings (configuration and registry map, two slots each) | 0x101FC000 | 16K |

## One time setup

1. The key pair is created automatically by the first `cargo build` in `embedded/`, as
   `embedded/keys/ota_secret.key` and `ota_public.key` (both git ignored). Back up the secret key: whoever has
   it can flash your devices, and losing it means reflashing every device with a probe. Deleting the public
   key makes the next build generate a new pair, so only do that for a fresh fleet.
   `ota keygen --out <dir>` still exists if you want a pair somewhere else.
   In CI the build refuses to generate a key, see [Releases from CI](#releases-from-ci).
2. Erase the chip (leftovers of an older firmware would be read as bootloader state), then flash the
   bootloader and a signed application with a probe. Either take `factory.bin` from a GitHub release
   (bootloader and firmware in one file, built with the example configuration compiled in):
   ```
   probe-rs erase --chip RP2040
   probe-rs download --chip RP2040 --binary-format bin --base-address 0x10000000 factory.bin
   ```
   or build it yourself:
   ```
   probe-rs erase --chip RP2040
   (cd bootloader && cargo build --release)
   (cd embedded && cargo build --release)
   cd service
   cargo run --bin ota -- sign --out /tmp/release --version 0.1.0 \
       --elf ../embedded/target/thumbv6m-none-eabi/release/modbus-mqtt-bridge-embedded \
       --key ../embedded/keys/ota_secret.key
   cargo run --bin ota -- factory --out /tmp/release/factory.bin --firmware /tmp/release/firmware.bin \
       --bootloader-elf ../bootloader/target/thumbv6m-none-eabi/release/modbus-mqtt-bridge-bootloader
   probe-rs download --chip RP2040 --binary-format bin --base-address 0x10000000 /tmp/release/factory.bin
   ```
   Flash the application from a `.bin`, not from the ELF: the ELF also carries a copy of BOOT2 that
   would overwrite the start of the bootloader's sector. (`cargo embed` is fine, `Embed.toml` restores
   the unwritten bytes.) The release also has `bootloader.elf` and `firmware.elf`, for probe-rs with
   symbols (defmt logs).
3. Give the device its configuration. `embedded/assets/configuration.json` is compiled in, a signed
   configuration from flash takes precedence. To provision a device that is not on the network yet, write
   the slot file with a probe, see [below](#updating-the-configuration-and-the-registry-map).

## Releasing an update

1. Raise `version` in `embedded/Cargo.toml` (it must be `major.minor.patch`, and strictly higher than
   what the device runs) and build: `cd embedded && cargo build --release`.
2. Sign it with the same `--version`:
   ```
   cd service
   cargo run --bin ota -- sign --elf ../embedded/target/thumbv6m-none-eabi/release/modbus-mqtt-bridge-embedded \
       --key ../embedded/keys/ota_secret.key --version 0.1.1 --out /tmp/release
   ```
3. Offer it. The tool publishes a retained manifest, answers the device's chunk requests, and
   removes the manifest again when the device reports back:
   ```
   cargo run --bin ota -- publish --config ../configuration.json --dir /tmp/release
   ```
   The device checks for a manifest at the end of each polling cycle (so within `polling_interval_ms`),
   downloads the image, verifies the signature, and restarts into it.

### Releases from CI

Pushing a tag `vX.Y.Z` (it has to match `version` in `embedded/Cargo.toml`) runs
`.github/workflows/release.yml`. It needs the key pair as base64: the repository variable
`OTA_PUBLIC_KEY_B64` and the secret `OTA_SECRET_KEY_B64` (`base64 -w0 embedded/keys/ota_*.key`).
The release gets:

| File | For |
|---|---|
| `firmware.bin`, `firmware.manifest` | `ota publish` (put both in a directory) |
| `factory.bin` | blank chip: `probe-rs download --chip RP2040 --binary-format bin --base-address 0x10000000 factory.bin` |
| `bootloader.elf`, `firmware.elf` | probe-rs with symbols, e.g. `probe-rs run --chip RP2040 firmware.elf` |

The configuration and the registry map are never built in CI, they hold credentials.

## Updating the configuration and the registry map

Both are plain files, signed with the same key, offered the same way and checked before they are used.
They are kept in their own flash region, two slots each, so an update never touches the firmware.

```
cd service
cargo run --bin ota -- sign-config   --file ../configuration.json --key ../embedded/keys/ota_secret.key --out /tmp/release
cargo run --bin ota -- sign-registry --file ../embedded/assets/registry_map --key ../embedded/keys/ota_secret.key --out /tmp/release
cargo run --bin ota -- publish --config ../configuration.json --dir /tmp/release
```

`publish` offers everything it finds in the directory (`firmware.*`, `config.*`, `registry.*`). The device
takes one artifact per session, firmware first, resets to use it, and then picks up the next one. `sign-*`
checks the file the way the device will (it parses, `mqtt.endpoint` is an `ip:port`, every registry line is
valid) and refuses to sign anything else. The device checks again, and also insists that a configuration is
for its own `mqtt.device_id`.

- **Revisions:** every signed file carries a revision (default: the unix time of signing, or `--revision N`).
  The device only accepts a revision higher than every one it has, rejected ones included, so an old file
  can't be replayed.
- **Rollback:** a new file is on trial. It only counts once a full polling cycle succeeded with it. Otherwise
  (wrong Wi-Fi password, unreachable broker, a crash, the watchdog) the next boot drops it for good and goes
  back to the previous one, or to the file compiled into the firmware. To try again, sign a new revision.
  A power cut during that first cycle counts as a failure too.
- **Limits:** each file is at most 3968 bytes.
- **Credentials:** the configuration contains the Wi-Fi and MQTT passwords. Updates are signed, not
  encrypted: anyone who can read `<device_id>/ota/config/chunk` on the broker can read them. Keep the broker
  private, or provision the configuration with a probe.
- **Provisioning with a probe:** `sign-config` and `sign-registry` also write `config.slot` and
  `registry.slot`, the exact flash contents of the first slot of each file:
  ```
  probe-rs download --chip RP2040 --binary-format bin --base-address 0x101fc000 /tmp/release/config.slot
  probe-rs download --chip RP2040 --binary-format bin --base-address 0x101fe000 /tmp/release/registry.slot
  ```
  They are tried on the next boot like any other update.

## How it stays safe

- Nothing is armed for swapping unless the whole image verifies against the embedded public key
  (`FirmwareUpdater::verify_and_mark_updated`, ed25519 over the SHA-512 of the image). The manifest
  (version and size) is signed too, so an old but genuine image can't be replayed as an "update".
- The manifest also says what kind of artifact it is (firmware, configuration, registry map), and it is
  signed. A manifest or file signed for one artifact is refused as another. Settings are signed over a
  domain prefix and the kind, so a firmware signature can't pass as a settings signature either.
- After the swap the new image is on trial. It confirms itself (`mark_booted`) only after a full
  polling cycle succeeded. If it crashes, hangs or cannot reach the broker the watchdog resets the
  board and the bootloader restores the previous image.
- A power cut during the download leaves the running image untouched; a power cut during the swap is
  resumed by the bootloader.

## Device status

The device reports on `<device_id>/ota/<firmware|config|registry>/status`: `downloading`, `ready`, or
`error: <reason>`. The topics are `<device_id>/ota/<artifact>/{manifest,request,chunk,status}`.

## Tests

The update procedure lives in `core/src/ota/` and runs on the host against a simulated broker and an
in-memory flash, with the real `embassy-boot` bootloader and updater on top:

```
cd core && cargo test --features ota
```

They cover: the happy path and the swap, rollback of an image that never confirms itself, forged or
replayed manifests (also across artifacts), tampered images, downgrades, oversized images, lost/duplicated/late/misnumbered
chunks, a host that goes silent, retrying after an aborted download, and power cuts during the
download and during the swap, and for the settings files: revisions, validation, trial and fallback, a
corrupted slot, and power cuts while storing. What they can't cover is flash timing, the watchdog and the Wi-Fi chip,
that still needs a board.
