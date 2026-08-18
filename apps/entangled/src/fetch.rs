//! `entangled fetch` — download and verify installer media (backlog EPIC 6/12).
//!
//! All the work lives in the `debian-media` crate; this module only turns its
//! [`FetchReport`] into something readable on a terminal. The important thing it
//! prints is *why* the media is trusted: which key signed the checksum root and
//! where the provenance manifest went.

use debian_media::{FetchOptions, FetchReport, MediaKind, Provenance};

use crate::FetchArgs;

pub fn run(args: &FetchArgs) -> Result<(), String> {
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

fn kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Kernel => "kernel",
        MediaKind::Initrd => "initrd",
        MediaKind::Iso => "netinst ISO",
        MediaKind::Sha512Sums => "checksum file",
        MediaKind::Sha512SumsSignature => "checksum signature",
    }
}
