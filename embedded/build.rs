//! This build script copies the `memory.x` file from the crate root into
//! a directory where the linker can always find it at build time.
//! For many projects this is optional, as the linker always searches the
//! project root directory -- wherever `Cargo.toml` is. However, if you
//! are using a workspace or have a more complicated build setup, this
//! build script becomes required. Additionally, by requesting that
//! Cargo re-run the build script whenever `memory.x` is changed,
//! updating `memory.x` ensures a rebuild of the application with the
//! new memory settings.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

/// Creates the OTA signing key pair the first time the firmware is built.
/// Both files are git ignored: the public key is embedded in the firmware, the secret one signs releases.
fn ensure_ota_keys() {
    let dir = PathBuf::from("keys");
    let secret_path = dir.join("ota_secret.key");
    let public_path = dir.join("ota_public.key");
    if public_path.exists() {
        return;
    }
    if env::var_os("CI").is_some() {
        panic!(
            "keys/ota_public.key is missing in CI: a generated key would produce firmware that deployed devices reject. \
             Provide the real key pair from the repository secrets."
        );
    }
    std::fs::create_dir_all(&dir).unwrap();

    let seed: [u8; 32] = match std::fs::read(&secret_path) {
        Ok(bytes) => bytes.try_into().expect("keys/ota_secret.key is not 32 bytes"),
        Err(_) => {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).expect("no randomness available");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            options.open(&secret_path).unwrap().write_all(&seed).unwrap();
            seed
        }
    };
    let public = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
    std::fs::write(&public_path, public.to_bytes()).unwrap();
    println!(
        "cargo:warning=Generated a new OTA key pair in embedded/keys/. Back up ota_secret.key: without it devices running this firmware can only be updated with a probe."
    );
}

fn main() {
    ensure_ota_keys();
    // Create the compiled-in configuration and registry map if missing (they are git ignored so
    // local changes don't get committed, the defaults come from the repository root).
    if !PathBuf::from("assets/configuration.json").exists() {
        File::create("assets/configuration.json")
            .unwrap()
            .write_all(include_bytes!("../configuration.example.json"))
            .unwrap();
    }
    if !PathBuf::from("assets/registry_map").exists() {
        File::create("assets/registry_map")
            .unwrap()
            .write_all(include_bytes!("../registry_map"))
            .unwrap();
    }
    // Put `memory.x` in our output directory and ensure it's
    // on the linker search path.
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());

    // By default, Cargo will re-run a build script whenever
    // any file in the project changes. By specifying `memory.x`
    // here, we ensure the build script is only re-run when
    // `memory.x` is changed.
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=keys/ota_public.key");
    println!("cargo:rerun-if-changed=\"assets/configuration.json\"");
    println!("cargo:rerun-if-changed=\"../configuration.example.json\"");
    println!("cargo:rerun-if-changed=../registry_map");

    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
}
