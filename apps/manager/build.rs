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

    embed_windows_icon();
}

/// Gives `entangled-manager.exe` its icon — the one Explorer, a pinned taskbar
/// button and Alt+Tab show before any window exists. The window sets its own
/// at run time (`logo::app_icon`); this is the file's.
///
/// No crate: `rc.exe` from the Windows SDK compiles a one-line resource script
/// and the MSVC linker takes the `.res` as an input. Without the SDK the build
/// still succeeds, with a warning and the generic icon — a missing icon is
/// never worth a failed build.
fn embed_windows_icon() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }
    let manifest =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let icon = manifest.join("../../installer/entangled.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-env-changed=RC");
    let Ok(icon) = icon.canonicalize() else {
        println!(
            "cargo:warning=installer/entangled.ico is missing; the exe keeps the generic icon"
        );
        return;
    };
    let Some(rc) = find_rc() else {
        println!("cargo:warning=no rc.exe (Windows SDK) found; the exe keeps the generic icon");
        return;
    };
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets this"));
    let script = out.join("manager.rc");
    let res = out.join("manager.res");
    // `\\?\` paths confuse rc.exe; the plain form is fine inside OUT_DIR.
    let icon_path = icon.display().to_string();
    let icon_path = icon_path.trim_start_matches(r"\\?\").replace('\\', "\\\\");
    if let Err(e) = std::fs::write(&script, format!("1 ICON \"{icon_path}\"\n")) {
        println!("cargo:warning=cannot write the icon resource script: {e}");
        return;
    }
    let status = std::process::Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&script)
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        Ok(status) => {
            println!("cargo:warning=rc.exe failed ({status}); the exe keeps the generic icon")
        }
        Err(e) => println!("cargo:warning=cannot run rc.exe: {e}; the exe keeps the generic icon"),
    }
}

/// `RC` if set, else the newest x64 `rc.exe` in the Windows 10/11 SDK.
fn find_rc() -> Option<std::path::PathBuf> {
    if let Ok(rc) = std::env::var("RC") {
        return Some(std::path::PathBuf::from(rc));
    }
    let root = std::env::var("ProgramFiles(x86)")
        .unwrap_or_else(|_| r"C:\Program Files (x86)".to_string());
    let bin = std::path::Path::new(&root).join(r"Windows Kits\10\bin");
    let mut versions: Vec<std::path::PathBuf> = std::fs::read_dir(&bin)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.join(r"x64\rc.exe").is_file())
        .collect();
    // Directory names are SDK versions ("10.0.26100.0"); compare them as
    // numbers so 10.0.9 does not sort after 10.0.26100.
    versions.sort_by_key(|path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                n.split('.')
                    .map(|p| p.parse::<u32>().unwrap_or(0))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    versions.pop().map(|path| path.join(r"x64\rc.exe"))
}
