//! Injects the release version into the binary.
//!
//! CI (`.github/workflows/release.yml`) computes the full `MAJOR.MINOR.PATCH`
//! (the patch is the workflow run number) and passes it through the
//! `ENTANGLED_VERSION` environment variable; local builds fall back to the
//! workspace `CARGO_PKG_VERSION`. The value is re-exported as a compile-time
//! env so `env!("ENTANGLED_VERSION")` works in the crate.

fn main() {
    println!("cargo:rerun-if-env-changed=ENTANGLED_VERSION");
    let version = std::env::var("ENTANGLED_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("cargo sets this"));
    println!("cargo:rustc-env=ENTANGLED_VERSION={version}");

    // SHA-256 of the Linux `entangled` asset published beside this release —
    // the manager's whole trust anchor for downloading an engine into WSL
    // (src/wslengine.rs). The release workflow builds and hashes the Linux
    // binary *before* it builds this one, so the digest is fixed at compile
    // time and nobody can point the download at a different build. Empty in
    // every build the pipeline did not make, and an empty pin means the
    // manager refuses to download rather than fetching something unverified.
    println!("cargo:rerun-if-env-changed=ENTANGLED_LINUX_ENGINE_SHA256");
    let engine_sha = std::env::var("ENTANGLED_LINUX_ENGINE_SHA256").unwrap_or_default();
    println!(
        "cargo:rustc-env=ENTANGLED_LINUX_ENGINE_SHA256={}",
        engine_sha.trim()
    );
}
