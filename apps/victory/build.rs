//! D3D12 Agility SDK opt-in.
//!
//! Windows 10's inbox D3D12 runtime only exposes shader model 6.5, while the
//! translated title shaders need 6.6 (heap-direct descriptors). Putting an
//! Agility SDK `D3D12Core.dll` next to the built executable lets the host
//! loader pick it up, but only when the executable exports the loader's
//! opt-in symbols. Those exports are build-time, so this build script enables
//! them exactly when a runtime is present next to the output binary:
//!
//! ```text
//! target/release/D3D12/D3D12Core.dll
//! ```
//!
//! Without it the binary keeps using the system runtime, unchanged.

use std::path::PathBuf;

/// Agility SDK version the exports below announce, matching `1.614.1`.
const SDK_VERSION: u32 = 614;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(victory_agility)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    // `target/<profile>/build/victory-<hash>/out`: the binary lands three
    // directories above `OUT_DIR`.
    let Some(profile_dir) = PathBuf::from(&out_dir)
        .ancestors()
        .nth(3)
        .map(PathBuf::from)
    else {
        return;
    };
    let runtime = profile_dir.join("D3D12").join("D3D12Core.dll");
    println!("cargo:rerun-if-changed={}", runtime.display());
    if !runtime.is_file() {
        return;
    }
    println!("cargo:rustc-cfg=victory_agility");
    println!("cargo:rustc-link-arg=/EXPORT:D3D12SDKVersion");
    println!("cargo:rustc-link-arg=/EXPORT:D3D12SDKPath");
    println!(
        "cargo:warning=opting the host D3D12 runtime into Agility SDK {SDK_VERSION} ({})",
        runtime.display()
    );
}
