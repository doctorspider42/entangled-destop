#!/usr/bin/env bash
# Downloads and verifies an official Ubuntu Server (live-server) ISO for the
# UEFI boot path (backlog UEFI-1803, docs/adr/0003-uefi-firmware.md).
#
#   bash scripts/fetch-ubuntu-iso.sh            # pinned LTS, live-server, amd64
#   bash scripts/fetch-ubuntu-iso.sh desktop    # the Desktop ISO (~6 GiB), same
#                                               # trust chain, same cache
#   UBUNTU_RELEASE=24.04.4 bash scripts/fetch-ubuntu-iso.sh
#   UBUNTU_VARIANT=desktop bash scripts/fetch-ubuntu-iso.sh
#
# Prints the path of the verified ISO on stdout (and nothing else), so a caller
# can do `iso=$(bash scripts/fetch-ubuntu-iso.sh)`.
#
# ## Trust chain
#
# The same shape `crates/debian-media` uses for Debian media, which is the house
# standard (MVP-607, `crates/debian-media/src/keyring.rs`):
#
#   1. the *signing key is pinned by fingerprint* and committed to this
#      repository as an armored file — no keyserver lookup, no WKD, no
#      trust-on-first-use, no `--keyserver-options auto-key-retrieve`;
#   2. the key is imported into a throwaway `GNUPGHOME`, so this script can
#      never add anything to the caller's keyring, and the fingerprint of what
#      was actually imported is checked against the pin before it is used;
#   3. `SHA256SUMS.gpg` (a detached signature) is verified over `SHA256SUMS`;
#   4. only *then* is the ISO's expected digest read out of that verified file,
#      and the download is checked against it.
#
# Every step is fatal. There is no "continue unverified" path and no flag to ask
# for one: an ISO whose provenance we cannot prove is one we do not boot.
set -euo pipefail

# --- what to fetch -----------------------------------------------------------

# Pinned deliberately rather than resolved from the directory listing: the ISO
# layout, the shim/GRUB versions inside the ESP and the installer generation are
# all release-visible, and a boot test that silently follows "whatever is
# current" stops being a regression test. Bump it on purpose.
UBUNTU_RELEASE="${UBUNTU_RELEASE:-26.04}"
# `live-server` is the subiquity installer — the UEFI-1803 target. `desktop` is
# the same trust chain and the same ISO shape (isohybrid, GPT, an ESP with
# \EFI\BOOT\BOOTX64.EFI), just larger. The first argument (if any) wins over
# the environment: `fetch-ubuntu-iso.sh desktop`.
UBUNTU_VARIANT="${1:-${UBUNTU_VARIANT:-live-server}}"
case "$UBUNTU_VARIANT" in
    live-server|desktop) ;;
    *) echo "ERROR: unknown variant '$UBUNTU_VARIANT' (use live-server or desktop)" >&2; exit 1 ;;
esac
UBUNTU_ARCH="${UBUNTU_ARCH:-amd64}"
BASE_URL="${UBUNTU_BASE_URL:-https://releases.ubuntu.com}"

# Ubuntu CD Image Automatic Signing Key (2012) <cdimage@ubuntu.com>. This is the
# key that signs `SHA256SUMS.gpg` under releases.ubuntu.com; verified against
# the signature's own issuer-fingerprint subpacket for 26.04.
PINNED_FINGERPRINT="843938DF228D22F7B3742BC0D94AA3F0EFE21092"
KEY_FILE_NAME="ubuntu-cd-D94AA3F0EFE21092.asc"

repo="$(cd "$(dirname "$0")/.." && pwd)"
key_file="$repo/scripts/keys/$KEY_FILE_NAME"

# Big files never live on /mnt/d: it is a drvfs mount on the development host
# (slow, and it cannot do sparse files), and a 3 GiB ISO has no business in a
# worktree.
#
# The cache root is the one `debian_media::cache_root` resolves, in the same
# order, because `entangled install ubuntu` looks for the ISO there and two
# answers means two caches: $ENTANGLED_CACHE, then $XDG_CACHE_HOME/entangled,
# then — on Windows — %LOCALAPPDATA%\entangled, else $HOME/.cache/entangled.
# `$LOCALAPPDATA` set is the reliable "this bash runs on Windows" test: git-bash
# and MSYS export it, WSL does not (it is not in WSLENV by default), and it is
# there that a shell also has $HOME and would otherwise pick the unix layout.
if [ -n "${ENTANGLED_CACHE:-}" ]; then
    cache_root="$ENTANGLED_CACHE"
elif [ -n "${XDG_CACHE_HOME:-}" ]; then
    cache_root="$XDG_CACHE_HOME/entangled"
elif [ -n "${LOCALAPPDATA:-}" ]; then
    cache_root="$LOCALAPPDATA/entangled"
else
    cache_root="$HOME/.cache/entangled"
fi
cache="$cache_root/ubuntu/$UBUNTU_RELEASE"

iso_name="ubuntu-${UBUNTU_RELEASE}-${UBUNTU_VARIANT}-${UBUNTU_ARCH}.iso"
release_url="$BASE_URL/$UBUNTU_RELEASE"

# Everything except the final path goes to stderr, so stdout stays a clean
# machine-readable answer.
log() { echo ">> $*" >&2; }
die() { echo "ERROR: $*" >&2; exit 1; }

for tool in curl gpg sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool '$tool'"
done
[ -f "$key_file" ] || die "pinned key missing: $key_file"

mkdir -p "$cache"

# --- 1/4: the pinned key, in a keyring of our own ----------------------------

# `mktemp -d` gives 0700, which gpg insists on. Removed on every exit path.
gnupg_home="$(mktemp -d)"
cleanup() { rm -rf "$gnupg_home"; }
trap cleanup EXIT
chmod 700 "$gnupg_home"

log "importing the pinned Ubuntu CD signing key"
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

# --- 2/4: SHA256SUMS and its detached signature ------------------------------

fetch() {
    # --proto '=https' refuses a redirect to plain http; -f makes a 404 an error
    # rather than an HTML file that then fails a digest check confusingly.
    curl -fL --proto '=https' --retry 3 --retry-delay 2 "$@"
}

log "fetching SHA256SUMS + SHA256SUMS.gpg for Ubuntu $UBUNTU_RELEASE"
fetch -o "$cache/SHA256SUMS" "$release_url/SHA256SUMS" \
    || die "cannot download $release_url/SHA256SUMS"
fetch -o "$cache/SHA256SUMS.gpg" "$release_url/SHA256SUMS.gpg" \
    || die "cannot download $release_url/SHA256SUMS.gpg"

log "verifying the SHA256SUMS signature"
if ! gpg --homedir "$gnupg_home" --batch --status-fd 3 \
        --verify "$cache/SHA256SUMS.gpg" "$cache/SHA256SUMS" 3>"$cache/.gpgstatus" 2>/dev/null
then
    sed 's/^/  /' "$cache/.gpgstatus" >&2 || true
    die "SHA256SUMS is not signed by $PINNED_FINGERPRINT"
fi
# `--verify` succeeding is not quite enough on its own: assert the machine
# readable status line names *our* key, so a future keyring with more than one
# certificate in it cannot widen the check by accident.
grep -q "VALIDSIG $PINNED_FINGERPRINT" "$cache/.gpgstatus" \
    || die "the signature verified, but not against the pinned key; status:
$(sed 's/^/  /' "$cache/.gpgstatus")"
rm -f "$cache/.gpgstatus"
log "signature OK ($PINNED_FINGERPRINT)"

# --- 3/4: the expected digest, out of the now-trusted file -------------------

# Lines look like `<64 hex> *ubuntu-26.04-live-server-amd64.iso`.
expected="$(awk -v want="*$iso_name" '$2 == want { print $1 }' "$cache/SHA256SUMS")"
[ -n "$expected" ] || die "$iso_name is not listed in the signed SHA256SUMS:
$(sed 's/^/  /' "$cache/SHA256SUMS")"
case "$expected" in
    [0-9a-f][0-9a-f]*) [ "${#expected}" -eq 64 ] || die "malformed digest '$expected'" ;;
    *) die "malformed digest '$expected'" ;;
esac

# --- 4/4: the ISO itself -----------------------------------------------------

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
        # Resume rather than restart: 3 GiB is worth one `-C -`. A partial file
        # that is *not* a prefix of the real one still fails the digest check
        # below, so resuming cannot weaken the verification.
        log "resuming download of $iso_name"
    else
        log "downloading $iso_name (~2.5-3 GiB)"
    fi
    fetch -C - -o "$iso" "$release_url/$iso_name" \
        || die "cannot download $release_url/$iso_name"
    verify_iso || die "SHA-256 mismatch for $iso
  expected: $expected
  actual:   $(sha256sum "$iso" | cut -d' ' -f1)
The download is corrupt or the mirror served something else. Delete it and retry."
fi
log "sha256 OK ($expected)"

{
    echo "source:      $release_url/$iso_name"
    echo "release:     $UBUNTU_RELEASE"
    echo "variant:     $UBUNTU_VARIANT"
    echo "arch:        $UBUNTU_ARCH"
    echo "size:        $(stat -c%s "$iso") bytes"
    echo "sha256:      $expected"
    echo "sums:        $release_url/SHA256SUMS"
    echo "signed-by:   $PINNED_FINGERPRINT (Ubuntu CD Image Automatic Signing Key 2012)"
    echo "verified:    $(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$iso.provenance"

log "wrote $iso.provenance"
echo "$iso"
