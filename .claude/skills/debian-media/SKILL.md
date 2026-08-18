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

**Two roots, because Debian publishes two schemes.** The backlog assumes a
detached `SHA512SUMS.sign` next to every artifact; that is only true for the CD
images. The netboot installer directory
(`dists/stable/main/installer-amd64/current/images/`) publishes `MD5SUMS` and
`SHA256SUMS` and **no signature at all** — the archive `Release` file is its only
signed root, exactly as `apt` uses it.

```text
netinst ISO (cdimage.debian.org):
  SHA512SUMS.sign --[Debian CD keyring]--> SHA512SUMS --[sha512]--> ISO

netboot kernel/initrd (deb.debian.org):
  Release.gpg --[Debian archive keyring]--> Release
              --[sha256]--> images/SHA256SUMS --[sha256]--> linux, initrd.gz
```

Either way:

1. Download the signed root and its detached signature.
2. Verify the signature against the pinned keyring **first**. A correct digest
   with an unverified checksum file counts as FAILED.
3. Only then stream-verify the artifact's digest (`sha2`, hashed while
   downloading — netinst ISO is ~700 MB, never buffer it in RAM).
4. Write the `Manifest` (exact URL, discovered version, RFC 3339 UTC time,
   digest, `signature_verified: true`) — only after both checks pass.
5. On a **verification** failure: delete the partial/failed artifact from the
   active cache (acceptance criterion). On a merely *interrupted* transfer keep
   the `.part` file — that is what makes the next run resumable. The split lives
   in `MediaError::invalidates_cache()`.

The version is discovered, never pinned: from `Version:` in the signed `Release`
for netboot, and from the discovered `debian-<ver>-<arch>-netinst.iso` name for
the ISO.

Archive keys rotate per Debian release, so `keyring.rs` needs the next suite's
key pinned before it becomes `stable`; the CD keys are long-lived.

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
  prefix on resume. Downloads land in `<file>.part` and are renamed only after
  the digest check.
- Cache layout: `$XDG_CACHE_HOME/vmhost/media/<version>/<arch>-<variant>/<file>`
  plus `<file>.manifest.toml`. The `<arch>-<variant>` component is not
  decoration: text and GTK netboot both publish files called `linux` and
  `initrd.gz` with *different* initrds, so a flat per-version directory would
  serve the wrong image.
- Re-fetch is a no-op when the manifest verifies (`vmhost fetch` twice = one
  download, and in fact zero network access). `--refresh` forces revalidation
  against the signed root, `--offline` forbids the network entirely.
- Discovery must pick the plain `debian-<ver>-<arch>-netinst.iso`, not
  `debian-edu-*` or other flavors that share the suffix (test in `sums.rs`
  documents this trap).

## Testing

- Everything above the HTTP layer is pure and tested offline (sums parsing,
  URL building, manifest round-trip — tests exist, extend them).
- Put the HTTP client behind a small trait so integration tests can serve
  fixtures locally; real-network tests are opt-in
  (`#[ignore]`, run manually), CI must not hit debian.org.
- `crates/debian-media/tests/support/` has the scaffolding: a `Fixture`
  transport with a request log and knobs for dropped connections / servers that
  ignore `Range`, and a `TestKey` that generates a throwaway OpenPGP key so both
  the accept and reject signature paths are covered without committing a private
  key. `TrustRoot` carries its keyring, which is what lets tests inject one.
- The one network test is `tests/network.rs`:
  `cargo test -p debian-media --test network -- --ignored --nocapture`.
