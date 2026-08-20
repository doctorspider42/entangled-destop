---
name: debian-media
description: The Debian downloader — channel/version resolution, OpenPGP + SHA-512 verification chain, cache and provenance manifests (backlog EPIC 6, crate debian-media). Load before working on entangled fetch or media verification.
---

# Debian media handling

Scope: backlog EPIC 6 (MVP-601…611), crate `crates/debian-media`, surfaced as
`entangled fetch`.

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
- Cache layout: `$XDG_CACHE_HOME/entangled/media/<version>/<arch>-<variant>/<file>`
  plus `<file>.manifest.toml`. The `<arch>-<variant>` component is not
  decoration: text and GTK netboot both publish files called `linux` and
  `initrd.gz` with *different* initrds, so a flat per-version directory would
  serve the wrong image.
- Re-fetch is a no-op when the manifest verifies (`entangled fetch` twice = one
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

## Ubuntu and Fedora media (shell scripts, not this crate)

`crates/debian-media` owns Debian. The other two distributions are fetched by
shell scripts that follow the *same* trust discipline into the *same* cache
(`scripts/fetch-ubuntu-iso.sh`, `scripts/fetch-fedora-iso.sh`; the cache root
resolution is copied from `debian_media::cache_root` in both, and
`apps/entangled/src/paths.rs` is the CLI's single answer to the same question).
The house rule is unchanged: **pinned key by fingerprint, signature first,
digest second, no "continue unverified" path**.

### Fedora's chain — clearsigned, which is the trap

```text
Fedora-<Edition>-<rel>-<compose>-<arch>-CHECKSUM   (a clearsigned document)
  --[pinned Fedora <rel> primary key]--> its signed payload
  --[sha256]--> Fedora-Workstation-Live-<rel>-<compose>.x86_64.iso
```

One link fewer than Ubuntu (no detached `.gpg` beside a plaintext sums file)
and one trap more: a clearsigned file is *mostly plaintext*, so
`gpg --verify` followed by grepping the download would read text an attacker
appended outside the signed block just as happily. The script uses
`gpg --decrypt --output`, which writes out only the payload the signature
covers, and everything downstream parses that file and never the download.
As with Ubuntu, `VALIDSIG <fingerprint>` is asserted on the status-fd, not just
gpg's exit code.

Two things are *discovered*, and the distinction matters:

- the **compose** (`1.7`) comes from the mirror's directory listing, because it
  changes with every respin and no stable URL hides it. It only ever decides
  *which signed file to fetch*;
- the **image name** comes out of the verified payload, so a mirror serving a
  doctored listing still cannot get an unverified byte onto the disk.

The release itself is pinned (`FEDORA_RELEASE`, default 44), and **Fedora
rotates its primary signing key every release** — so the pin is per release and
bumping the release without adding `scripts/keys/fedora-<rel>-primary-*.asc`
plus its fingerprint is a hard error, never a silent downgrade. Cross-check a
new fingerprint against <https://fedoraproject.org/security/> before trusting
it. Fedora 44 is `36F6 12DC F27F 7D1A 48A8 35E4 DBFC F71C 6D9F 90A6`.

### The two Fedora images are not interchangeable

`fetch-fedora-iso.sh` takes a variant, and picking the wrong one wastes an hour:

| Variant | Image | What it is good for |
|---|---|---|
| `workstation` (default) | `Fedora-Workstation-Live-<rel>-<compose>.x86_64.iso`, ~2.7 GiB | Booting. `entangled run --cdrom` reaches the GNOME live desktop with **no VMM changes** — the no-guest-additions proof. |
| `netinst` | `Fedora-Everything-netinst-x86_64-<rel>-<compose>.iso`, ~1.2 GiB | Installing. `entangled install fedora --auto`. |

The Live image **cannot be kickstarted at all**, and this is not a policy
choice — its initramfs contains no anaconda dracut module (no
`parse-kickstart`, no `fetch-kickstart-disk`, no OEMDRV udev rule), so nothing
in it can *find* a kickstart by any route, and on Live media `%packages` is
ignored anyway because the install is a copy of the live filesystem.
`install_fedora::InstallerMedia` reads the ISO's volume label and refuses a
`*-Live-*` one in the first second, with the alternative in the message.

### Kickstart delivery (`assets/kickstart/`, `apps/entangled/src/seed.rs`)

Fedora's automation is kickstart. The delivery is the Ubuntu seed volume with a
different label and a different file — `seed::write_kickstart` writes the same
hand-built ISO9660 image labelled `OEMDRV` holding `KS.CFG;1`, which `isofs`
presents as `ks.cfg`. The verified media is never repacked.

It is named on the kernel command line **as well** as carried on an
auto-detected label, deliberately. Reading `50-kickstart-genrules.sh` in the
installer initramfs: an explicit `inst.ks=hd:LABEL=OEMDRV:/ks.cfg` makes the
initqueue call `wait_for_kickstart`, so a kickstart that never arrives stalls
visibly with `Can't get kickstart from ...`. With no `inst.ks=` the auto-detect
branch waits a few seconds and then falls through to an *interactive* Anaconda —
a machine that looks alive for an hour and installs nothing. The label is what
still makes the pair work for a person booting it by hand.

Typing it costs nothing extra: `console=ttyS0,115200n8` has to be typed anyway,
because Fedora's own `grub.cfg` sets no `console=`. The netinst's menu entry
also needs `inst.stage2=hd:LABEL=<the ISO's volume label>`, and that label
carries the compose number — so it is read out of the image
(`InstallerMedia::read`, ECMA-119 §8.4.6: sector 16, offset 40, 32 bytes) rather
than pinned.

### Quirks worth remembering

- Unlike the Ubuntu install, the Fedora one is **online**: a netinst downloads
  ~2 GiB of RPMs, so `--network none` is refused and the wall-clock time is the
  mirror's, not the VMM's.
- Fedora's default layout roots on **btrfs**, so `find_uefi_install` returns no
  `root_uuid` — that is expected, not a failure. The ESP and the NVRAM
  `Boot####` entry are what make the disk bootable.
- No sudo and no `zstd`/`unsquashfs` on the WSL host: to look inside an ISO or
  an initramfs, read ISO9660 directly (the primary volume descriptor is at
  sector 16) and decompress with `xz -dc` (netinst) or python `zstandard`
  (Live). That is how the "no anaconda module in the Live initramfs" fact above
  was established rather than guessed.
