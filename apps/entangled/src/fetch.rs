//! `entangled fetch` — download and verify installer media (backlog EPIC 6/12),
//! and the guest bootstrap artifacts.
//!
//! All the work lives in the `debian-media` crate (or [`crate::bootstrap`]);
//! this module only turns a report into something readable on a terminal. The
//! important thing it prints is *why* the artifact is trusted — and the two
//! answers are genuinely different, so they are printed differently rather than
//! flattened into one reassuring "verified":
//!
//! * Debian media: a pinned OpenPGP key signed the checksum root, which names
//!   the digest. Signature first, digest second.
//! * bootstrap artifacts and the UEFI firmware: a SHA-256 compiled into *this
//!   binary*. No signature exists, and the output says so in as many words.

use debian_media::{FetchOptions, FetchReport, MediaKind, Provenance};

use crate::FetchArgs;

/// The spelling `fetch` accepts for the guest artifacts. Also the name `doctor`
/// and every failure hint print, so there is exactly one command to copy.
pub const BOOTSTRAP_TARGET: &str = "bootstrap-kernel";

pub fn run(args: &FetchArgs) -> Result<(), String> {
    if args.distro.eq_ignore_ascii_case(BOOTSTRAP_TARGET)
        || args.distro.eq_ignore_ascii_case("bootstrap")
    {
        return run_bootstrap(args);
    }
    if args
        .distro
        .eq_ignore_ascii_case(crate::firmware::FETCH_TARGET)
        || args.distro.eq_ignore_ascii_case("uefi")
    {
        return run_firmware(args);
    }
    let report = debian_media::fetch_debian(
        &args.distro,
        &args.channel,
        &args.arch,
        &args.variant,
        FetchOptions {
            refresh: args.refresh,
            offline: args.offline,
        },
    )
    .map_err(|e| e.to_string())?;

    print_report(&report);
    Ok(())
}

fn print_report(report: &FetchReport) {
    let origin = match &report.provenance {
        Provenance::Verified { .. } => "resolved from signed metadata",
        Provenance::Cache => "from the verified cache (no network access)",
    };
    println!(
        "debian {} ({}, {}) — {origin}",
        report.version,
        report.arch.as_str(),
        report.variant.as_str()
    );

    match &report.provenance {
        Provenance::Verified {
            sums_url,
            signed_by,
            keyring,
        } => {
            println!("  checksum root : {sums_url}");
            println!(
                "  signature     : OK — {} keyring, key {}",
                keyring, signed_by.signing_fingerprint
            );
        }
        Provenance::Cache => {
            // The digests were re-checked against the manifests just now; the
            // signature was verified when the manifests were written.
            let keyring = report
                .artifacts
                .first()
                .and_then(|a| a.manifest.keyring.as_deref())
                .unwrap_or("pinned");
            println!("  signature     : OK — verified earlier against the {keyring} keyring");
            println!("  digests       : re-checked against the provenance manifests");
        }
    }

    for artifact in &report.artifacts {
        println!();
        println!(
            "  {} [{}]",
            kind_label(artifact.kind),
            artifact.status.as_str()
        );
        println!("    path     : {}", artifact.path.display());
        println!("    url      : {}", artifact.manifest.url);
        println!(
            "    {:<9}: {}",
            artifact
                .manifest
                .algo()
                .map(|a| a.as_str())
                .unwrap_or("digest"),
            artifact.manifest.sha512_hex
        );
        println!("    manifest : {}", artifact.manifest_path.display());
        println!("    fetched  : {}", artifact.manifest.fetched_at);
    }
}

/// `entangled fetch bootstrap-kernel` — the guest kernel and initramfs.
///
/// The whole reason this exists: `install debian` needs a kernel that Debian's
/// installer kernel cannot be, this project builds one, and that build is a
/// Linux kernel build with no cross-compile. A Windows host had no way to get it
/// at all before this command.
fn run_bootstrap(args: &FetchArgs) -> Result<(), String> {
    let report = crate::bootstrap::fetch(crate::bootstrap::FetchOptions {
        refresh: args.refresh,
        offline: args.offline,
    })?;

    println!(
        "guest bootstrap artifacts — Linux {} ({})",
        report.kernel_version, report.tag
    );
    // Not "signature: OK". There is none, and the line that would say so is the
    // line somebody would quote in a security review.
    println!("  trust         : SHA-256 pinned in this build (guest/bootstrap-kernel/pinned.toml)");
    println!(
        "                  no signature — see the pin file for what that does and does not buy"
    );
    println!(
        "  kernel source : {}   (GPL-2.0-only; published with the binary)",
        report.source
    );
    println!("  cache         : {}", report.dir.display());
    for asset in &report.assets {
        println!();
        println!("  {} [{}]", asset.name, asset.status.as_str());
        println!("    path     : {}", asset.path.display());
        println!("    url      : {}", asset.url);
        println!("    sha256   : {}", asset.sha256);
    }
    println!();
    println!("`entangled install debian` will find these; `entangled doctor` reports them.");
    Ok(())
}

/// `entangled fetch firmware` — the UEFI firmware every UEFI guest boots.
///
/// The whole reason this exists: `install ubuntu` and `install fedora` need
/// EDK2's CloudHv build, that build only runs on Linux, and a person who
/// installed Entangled Desktop on Windows has no Linux checkout to copy one
/// from. The installer ships a copy for exactly that reason; this command is
/// how a *source checkout* on such a host gets one, and how a stale or deleted
/// copy is replaced.
fn run_firmware(args: &FetchArgs) -> Result<(), String> {
    let report = crate::firmware::fetch(crate::firmware::FetchOptions {
        refresh: args.refresh,
        offline: args.offline,
    })?;

    println!(
        "UEFI firmware — EDK2 CloudHvX64, {} ({})",
        report.edk2_tag, report.tag
    );
    // Not "signature: OK". There is none, and the line that would say so is the
    // line somebody would quote in a security review.
    println!("  trust         : SHA-256 pinned in this build (guest/firmware/pinned.toml)");
    println!(
        "                  no signature — see the pin file for what that does and does not buy"
    );
    println!("  licence       : BSD-2-Clause-Patent (EDK2); see THIRD-PARTY-NOTICES.txt");
    println!("  source        : {}", report.source);
    println!("  cache         : {}", report.dir.display());
    println!();
    println!("  {} [{}]", report.asset.name, report.asset.status.as_str());
    println!("    path     : {}", report.asset.path.display());
    println!("    url      : {}", report.asset.url);
    println!("    sha256   : {}", report.asset.sha256);
    println!();
    println!("`entangled install ubuntu` and `install fedora` will find this; `entangled doctor` reports it.");
    Ok(())
}

fn kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Kernel => "kernel",
        MediaKind::Initrd => "initrd",
        MediaKind::Iso => "netinst ISO",
        MediaKind::Sha512Sums => "checksum file",
        MediaKind::Sha512SumsSignature => "checksum signature",
    }
}
