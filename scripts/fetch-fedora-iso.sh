#!/usr/bin/env bash
# Downloads and verifies an official Fedora installation image for the UEFI boot
# path (docs/adr/0003-uefi-firmware.md), the Fedora sibling of
# scripts/fetch-ubuntu-iso.sh.
#
#   bash scripts/fetch-fedora-iso.sh              # Workstation Live, x86_64
#   bash scripts/fetch-fedora-iso.sh netinst      # Everything netinst (~800 MiB)
#   FEDORA_RELEASE=44 bash scripts/fetch-fedora-iso.sh
#
# Prints the path of the verified ISO on stdout (and nothing else), so a caller
# can do `iso=$(bash scripts/fetch-fedora-iso.sh)`.
#
# ## Trust chain
#
# Fedora publishes one file per compose — `Fedora-<Edition>-<rel>-<compose>-
# <arch>-CHECKSUM` — which is a *clearsigned* OpenPGP document holding the
# SHA-256 of every image in that directory. So the chain has one link fewer than
# Ubuntu's (no detached `.gpg` beside a plaintext sums file) and one trap more
# (a clearsigned file is mostly plaintext, and reading a digest out of the raw
# bytes would read an attacker's un-signed additions just as happily):
#
#   1. the *signing key is pinned by fingerprint* and committed to this
#      repository as an armored file — no keyserver lookup, no WKD, no
#      trust-on-first-use. Fedora rotates the primary key every release, so the
#      pin is per release ($KEY_FILE_NAME / $PINNED_FINGERPRINT below) and
#      bumping FEDORA_RELEASE without adding the next key is a hard error, not
#      a silent downgrade;
#   2. the key is imported into a throwaway `GNUPGHOME`, and the fingerprint of
#      what was actually imported is checked against the pin before it is used;
#   3. the CHECKSUM file is verified with `gpg --decrypt`, which writes out
#      *only the signed payload*. Everything downstream parses that file and
#      never the download — text outside the signed block cannot reach step 4;
#   4. the image name and its expected digest are read out of that payload, and
#      the download is checked against it with sha256sum.
#
# The compose number (`1.7` in `Fedora-Workstation-Live-44-1.7.x86_64.iso`) is
# *discovered*, never pinned — it changes with every respin and there is no
# stable URL that hides it — but discovery only ever picks which CHECKSUM file
# to fetch. The image name that is finally downloaded comes out of the signed
# payload, so a mirror that serves a doctored listing still cannot get an
# unverified byte onto the disk.
#
# Every step is fatal. There is no "continue unverified" path and no flag to ask
# for one: an ISO whose provenance we cannot prove is one we do not boot.
set -euo pipefail

# --- what to fetch -----------------------------------------------------------

# Pinned deliberately rather than resolved from the directory listing, for the
# same reason the Ubuntu script pins its release: the installer generation, the
# shim/GRUB versions inside the ESP and the kernel's driver set are all
# release-visible, and a boot test that silently follows "whatever is current"
# stops being a regression test. Bump it on purpose — and pin the matching key.
FEDORA_RELEASE="${FEDORA_RELEASE:-44}"

# `workstation` is the Live image: a GNOME session that runs Anaconda, and the
# thing that actually proves this VMM needs no guest additions. `netinst` is the
# Everything network installer — classic Anaconda, no live session, kickstart
# without the Live media caveats, but it downloads the whole system.
FEDORA_VARIANT="${1:-${FEDORA_VARIANT:-workstation}}"
case "$FEDORA_VARIANT" in
    workstation) EDITION="Workstation"; IMAGE_PREFIX="Fedora-Workstation-Live" ;;
    netinst)     EDITION="Everything";  IMAGE_PREFIX="Fedora-Everything-netinst" ;;
    *) echo "ERROR: unknown variant '$FEDORA_VARIANT' (use workstation or netinst)" >&2; exit 1 ;;
esac
FEDORA_ARCH="${FEDORA_ARCH:-x86_64}"
BASE_URL="${FEDORA_BASE_URL:-https://download.fedoraproject.org/pub/fedora/linux/releases}"

# The Fedora <release> primary signing key. Published at
# <https://fedoraproject.org/security/> and shipped in fedora-repos; this repo
# keeps a copy so nothing is fetched at run time. One entry per release, because
# Fedora issues a new primary key for each one.
case "$FEDORA_RELEASE" in
    44)
        PINNED_FINGERPRINT="36F612DCF27F7D1A48A835E4DBFCF71C6D9F90A6"
        KEY_FILE_NAME="fedora-44-primary-DBFCF71C6D9F90A6.asc"
        ;;
    *)
        echo "ERROR: no signing key is pinned for Fedora $FEDORA_RELEASE." >&2
        echo "  Fedora rotates its primary key every release. Add the armored key to" >&2
        echo "  scripts/keys/ and its fingerprint to this script — check the value" >&2
        echo "  against https://fedoraproject.org/security/ before trusting it." >&2
        exit 1
        ;;
esac

repo="$(cd "$(dirname "$0")/.." && pwd)"
key_file="$repo/scripts/keys/$KEY_FILE_NAME"

# Big files never live on /mnt/d: it is a drvfs mount on the development host
# (slow, and it cannot do sparse files), and a 2.8 GiB ISO has no business in a
# worktree. Same resolution order as `debian_media::cache_root` and
# fetch-ubuntu-iso.sh, because `entangled install fedora` looks for the ISO
# there and two answers means two caches.
if [ -n "${ENTANGLED_CACHE:-}" ]; then
    cache_root="$ENTANGLED_CACHE"
elif [ -n "${XDG_CACHE_HOME:-}" ]; then
    cache_root="$XDG_CACHE_HOME/entangled"
elif [ -n "${LOCALAPPDATA:-}" ]; then
    cache_root="$LOCALAPPDATA/entangled"
else
    cache_root="$HOME/.cache/entangled"
fi
cache="$cache_root/fedora/$FEDORA_RELEASE"

iso_dir_url="$BASE_URL/$FEDORA_RELEASE/$EDITION/$FEDORA_ARCH/iso"

# Everything except the final path goes to stderr, so stdout stays a clean
# machine-readable answer.
log() { echo ">> $*" >&2; }
die() { echo "ERROR: $*" >&2; exit 1; }

for tool in curl gpg sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool '$tool'"
done
[ -f "$key_file" ] || die "pinned key missing: $key_file"

mkdir -p "$cache"

fetch() {
    # --proto '=https' refuses a redirect to plain http; -f makes a 404 an error
    # rather than an HTML file that then fails a digest check confusingly.
    # download.fedoraproject.org is a redirector, so following hosts is normal.
    curl -fL --proto '=https' --retry 3 --retry-delay 2 "$@"
}

# --- 1/5: the pinned key, in a keyring of our own ----------------------------

# `mktemp -d` gives 0700, which gpg insists on. Removed on every exit path.
gnupg_home="$(mktemp -d)"
cleanup() { rm -rf "$gnupg_home"; }
trap cleanup EXIT
chmod 700 "$gnupg_home"

log "importing the pinned Fedora $FEDORA_RELEASE signing key"
gpg --homedir "$gnupg_home" --batch --quiet --no-default-keyring \
    --import "$key_file" \
    || die "cannot import $key_file"

# What landed in the keyring must be exactly what we pinned. A key file that has
# been swapped, or that carries a second certificate, fails here rather than
# quietly widening who may sign our media.
imported="$(gpg --homedir "$gnupg_home" --batch --with-colons --fingerprint \
    --list-keys | awk -F: '/^fpr:/ { print $10 }' | sort -u)"
if [ "$imported" != "$PINNED_FINGERPRINT" ]; then
    die "keyring fingerprint mismatch
  expected: $PINNED_FINGERPRINT
  imported: ${imported:-<nothing>}
$key_file is not the key this script pins."
fi

# --- 2/5: which compose is current -------------------------------------------

# The one place a directory listing is consulted. It decides *which signed file*
# to fetch and nothing else: if it lies, step 3 fails on the signature or step 4
# fails to name an image, and neither outcome puts an unverified byte on disk.
log "resolving the current Fedora $FEDORA_RELEASE $EDITION compose"
listing="$(fetch -s "$iso_dir_url/")" \
    || die "cannot list $iso_dir_url/"
checksum_name="$(printf '%s' "$listing" \
    | grep -oE "Fedora-$EDITION-$FEDORA_RELEASE-[0-9.]+-$FEDORA_ARCH-CHECKSUM" \
    | sort -uV | tail -1)"
[ -n "$checksum_name" ] || die "no Fedora-$EDITION-$FEDORA_RELEASE-*-$FEDORA_ARCH-CHECKSUM \
in $iso_dir_url/ — has release $FEDORA_RELEASE moved to the archive?"
log "compose: $checksum_name"

# --- 3/5: the clearsigned CHECKSUM, and only its signed payload --------------

fetch -o "$cache/$checksum_name" "$iso_dir_url/$checksum_name" \
    || die "cannot download $iso_dir_url/$checksum_name"

# `--decrypt` on a clearsigned document verifies the signature AND writes out
# the payload it covers. That is the important half: `--verify` alone would
# leave us parsing the downloaded file, which may carry any amount of text
# outside the signed block.
verified="$cache/.checksum-verified"
if ! gpg --homedir "$gnupg_home" --batch --yes --status-fd 3 \
        --output "$verified" --decrypt "$cache/$checksum_name" \
        3>"$cache/.gpgstatus" 2>/dev/null
then
    sed 's/^/  /' "$cache/.gpgstatus" >&2 || true
    rm -f "$verified" "$cache/.gpgstatus"
    die "$checksum_name is not signed by $PINNED_FINGERPRINT"
fi
# `--decrypt` succeeding is not quite enough on its own: assert the machine
# readable status line names *our* key, so a future keyring with more than one
# certificate in it cannot widen the check by accident.
grep -q "VALIDSIG $PINNED_FINGERPRINT" "$cache/.gpgstatus" \
    || die "the signature verified, but not against the pinned key; status:
$(sed 's/^/  /' "$cache/.gpgstatus")"
rm -f "$cache/.gpgstatus"
log "signature OK ($PINNED_FINGERPRINT)"

# --- 4/5: the image name and digest, out of the now-trusted payload ----------

# Lines look like `SHA256 (Fedora-Workstation-Live-44-1.7.x86_64.iso) = <64 hex>`
# (BSD-style tags, not the `<hex>  <name>` shape Debian and Ubuntu use).
entry="$(grep -E "^SHA256 \($IMAGE_PREFIX-[^)]*\.iso\) = [0-9a-f]{64}$" "$verified" \
    | sort -V | tail -1)" \
    || true
[ -n "$entry" ] || die "the signed $checksum_name lists no $IMAGE_PREFIX image:
$(sed 's/^/  /' "$verified")"
iso_name="${entry#SHA256 (}"
iso_name="${iso_name%%)*}"
expected="${entry##* }"
case "$iso_name" in
    */*|*..*) die "refusing a path-like image name from the checksum file: '$iso_name'" ;;
esac
[ "${#expected}" -eq 64 ] || die "malformed digest '$expected'"
log "image: $iso_name"

# --- 5/5: the ISO itself -----------------------------------------------------

iso="$cache/$iso_name"
verify_iso() {
    [ -f "$iso" ] || return 1
    local actual
    actual="$(sha256sum "$iso" | cut -d' ' -f1)"
    [ "$actual" = "$expected" ]
}

if verify_iso; then
    log "$iso_name already present and verified"
else
    if [ -f "$iso" ]; then
        # Resume rather than restart: 2.8 GiB is worth one `-C -`. A partial file
        # that is *not* a prefix of the real one still fails the digest check
        # below, so resuming cannot weaken the verification.
        log "resuming download of $iso_name"
    else
        log "downloading $iso_name (Workstation Live is ~2.7 GiB, netinst ~800 MiB)"
    fi
    fetch -C - -o "$iso" "$iso_dir_url/$iso_name" \
        || die "cannot download $iso_dir_url/$iso_name"
    verify_iso || die "SHA-256 mismatch for $iso
  expected: $expected
  actual:   $(sha256sum "$iso" | cut -d' ' -f1)
The download is corrupt or the mirror served something else. Delete it and retry."
fi
log "sha256 OK ($expected)"

{
    echo "source:      $iso_dir_url/$iso_name"
    echo "release:     $FEDORA_RELEASE"
    echo "variant:     $FEDORA_VARIANT ($EDITION)"
    echo "arch:        $FEDORA_ARCH"
    echo "size:        $(stat -c%s "$iso") bytes"
    echo "sha256:      $expected"
    echo "sums:        $iso_dir_url/$checksum_name (clearsigned)"
    echo "signed-by:   $PINNED_FINGERPRINT (Fedora ($FEDORA_RELEASE) <fedora-$FEDORA_RELEASE-primary@fedoraproject.org>)"
    echo "verified:    $(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$iso.provenance"

log "wrote $iso.provenance"
echo "$iso"
