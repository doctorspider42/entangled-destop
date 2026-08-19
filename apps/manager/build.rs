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
}
