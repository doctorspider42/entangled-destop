---
name: debian-media
description: The Debian downloader — channel/version resolution, OpenPGP + SHA-512 verification chain, cache and provenance manifests (backlog EPIC 6, crate debian-media). Load before working on vmhost fetch or media verification.
---

# Debian media handling

Scope: backlog EPIC 6 (MVP-601…611), crate `crates/debian-media`, surfaced as
`vmhost fetch`.

## Existing pieces

- `DebianStableSource` (in `source.rs`) resolves directory URLs for
  text/GTK netboot and the netinst ISO from the `stable`/`current` channels.
  **Never pin release numbers** — the ISO file name (and thus version) is
  discovered from `SHA512SUMS` at fetch time.
- `parse_sums` / `SumsEntry::is_netinst_iso` / `version_from_iso_name`
  (in `sums.rs`) handle checksum-file parsing and discovery.
- `Manifest` (in `manifest.rs`) is the provenance record written next to
  every verified artifact; an artifact without one is treated as unverified.

## Trust chain (order is the point — MVP-607/608)

1. Download `SHA512SUMS` and `SHA512SUMS.sign` from the same directory as
   the artifact.
2. Verify the signature against the **Debian CD signing keyring** first.
   A correct SHA-512 with an unverified sums file counts as FAILED.
3. Only then stream-verify the artifact's SHA-512 (`sha2` crate, hash while
   downloading — netinst ISO is ~700 MB, never buffer it in RAM).
4. Write the `Manifest` (exact URL, discovered version, RFC 3339 UTC time,
   digest, `signature_verified: true`) — only after both checks pass.
5. On any failure: delete the partial/failed artifact from the active cache
   (acceptance criterion).

## Library constraints (license gate!)

- OpenPGP: use **`rpgp`** (the `pgp` crate — MIT/Apache-2.0).
  **Do NOT use `sequoia-openpgp` — it is LGPL and `cargo deny` will block
  it** (per the no-copyleft hard rule).
- HTTP: rustls-based client only (`ureq` with rustls, or `reqwest` with
  `default-features = false, features = ["rustls-tls"]`) — avoids the
  OpenSSL/GPL-adjacent linkage question entirely and keeps builds static.
- Debian keyring: ship the CD-signing public keys as pinned files under
  `crates/debian-media/keys/` with their fingerprints in code, sourced from
  <https://www.debian.org/CD/verify>. Do not fetch keys from keyservers at
  runtime.

## Behavior requirements

- Resumable downloads (HTTP Range) — MVP acceptance requires resume after
  interruption; hash state cannot resume across runs, so re-hash the existing
  prefix on resume.
- Cache layout: `~/.cache/vmhost/media/<version>/<file>` + manifest;
  re-fetch is a no-op when the manifest verifies (`vmhost fetch` twice = one
  download).
- Discovery must pick the plain `debian-<ver>-<arch>-netinst.iso`, not
  `debian-edu-*` or other flavors that share the suffix (test in `sums.rs`
  documents this trap).

## Testing

- Everything above the HTTP layer is pure and tested offline (sums parsing,
  URL building, manifest round-trip — tests exist, extend them).
- Put the HTTP client behind a small trait so integration tests can serve
  fixtures locally; real-network tests are opt-in
  (`#[ignore]`, run manually), CI must not hit debian.org.
