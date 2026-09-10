#!/usr/bin/env bash
# The Linux half of the fresh-install acceptance: the PUBLISHED
# `entangled-linux-x86_64`, run with an empty HOME and no checkout.
#
# This binary is not a curiosity. It is what `entangled wsl install-engine`
# downloads into a WSL distribution, so it is the exact program that used to be
# absent when the WSL (KVM) backend died with `execvpe entangled failed 2`. If
# it cannot find its own firmware, or cannot fetch one, then fixing that
# regression only moved it.
#
# What a stranger is here:
#
#   * the binary comes off GitHub Releases over plain HTTPS — no `gh`, no
#     token, no checkout;
#   * HOME, XDG_CACHE_HOME and XDG_CONFIG_HOME point at a scratch tree, so
#     there is no verified media cache, no manager settings and no
#     ~/entangled-vms;
#   * every ENTANGLED_*, CARGO_*, RUST* and *_TOKEN variable is stripped;
#   * the working directory is empty, with no Cargo.toml and no artifacts/ at
#     or above it — asserted, not assumed.
#
# What it cannot cover: a GitHub runner has no /dev/kvm, so `doctor` exits
# non-zero and nothing boots. That is precisely why `doctor` prints the install
# inventory before it fails on the hypervisor; the assertions below are on the
# inventory. Booting a guest is the developer machine's job — the WSL side of
# this repository, or scripts/fresh-install-acceptance.ps1 -Stages guest.
#
# Usage:
#   bash scripts/fresh-install-acceptance.sh [--tag v0.2.44] [--root DIR] [--keep]
#
# Exit code 0 when every check passed, 1 otherwise.

set -uo pipefail

REPO="doctorspider42/entangled-destop"
ASSET="entangled-linux-x86_64"
TAG="latest"
ROOT=""
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --tag) TAG="${2:-latest}"; shift 2 ;;
    --root) ROOT="${2:?--root needs a directory}"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$TAG" ] || TAG=latest
ROOT="${ROOT:-${TMPDIR:-/tmp}/entangled-fresh-${TAG//[^A-Za-z0-9._-]/_}}"
mkdir -p "$ROOT"
ROOT=$(cd "$ROOT" && pwd)

HOME_DIR="$ROOT/home"        # the throwaway HOME
CWD_DIR="$ROOT/cwd"          # a working directory with no repository above it
BIN_DIR="$ROOT/bin"
LOG_DIR="$ROOT/logs"
mkdir -p "$HOME_DIR" "$CWD_DIR" "$BIN_DIR" "$LOG_DIR"

PASS=0
FAIL=0
declare -a FAILED=()

say() { printf '\n\033[36m== %s\033[0m\n' "$1"; }
check() { # check <ok:0|1> <name> [detail]
  if [ "$1" -eq 0 ]; then
    PASS=$((PASS + 1)); printf '  \033[32m[PASS]\033[0m %s\n' "$2"
  else
    FAIL=$((FAIL + 1)); FAILED+=("$2"); printf '  \033[31m[FAIL]\033[0m %s\n' "$2"
  fi
  [ -n "${3:-}" ] && printf '         %s\n' "$3"
  return 0
}
note() { printf '  \033[90m[INFO]\033[0m %s\n' "$1"; [ -n "${2:-}" ] && printf '         %s\n' "$2"; return 0; }

# The stranger's environment. `env -i` would be truer still, but it would also
# take away PATH and the CA bundle location, which every Linux user does have.
# So: a clean HOME plus an explicit list of removals.
stranger() { # stranger <logfile> <timeout-secs> -- <args...>
  local log="$1" timeout_s="$2"; shift 2; [ "$1" = "--" ] && shift
  ( cd "$CWD_DIR" || exit 127
    unset $(env | sed -n 's/^\(ENTANGLED_[A-Z0-9_]*\)=.*/\1/p') 2>/dev/null
    unset $(env | sed -n 's/^\(CARGO[A-Z0-9_]*\)=.*/\1/p') 2>/dev/null
    unset $(env | sed -n 's/^\(RUST[A-Z0-9_]*\)=.*/\1/p') 2>/dev/null
    unset GITHUB_TOKEN GH_TOKEN GH_CONFIG_DIR 2>/dev/null
    export HOME="$HOME_DIR"
    export XDG_CACHE_HOME="$HOME_DIR/.cache"
    export XDG_CONFIG_HOME="$HOME_DIR/.config"
    export XDG_DATA_HOME="$HOME_DIR/.local/share"
    # A stranger has no `gh`; scrubbing HOME hides its config, and taking its
    # directory off PATH covers a system-wide install too.
    if command -v gh >/dev/null 2>&1; then
      local ghdir; ghdir=$(dirname "$(command -v gh)")
      PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -vxF "$ghdir" | paste -sd: -)
      export PATH
    fi
    timeout "${timeout_s}s" "$@" ) >"$log" 2>&1
  return $?
}

printf '\nEntangled Desktop — fresh-install acceptance (Linux engine)\n'
printf '  repository : %s\n  tag        : %s\n  scratch    : %s\n' "$REPO" "$TAG" "$ROOT"

# ---------------------------------------------------------------------------
say "download — the artifact a stranger gets"
# ---------------------------------------------------------------------------

api="https://api.github.com/repos/$REPO/releases/latest"
[ "$TAG" != "latest" ] && api="https://api.github.com/repos/$REPO/releases/tags/$TAG"

release_json="$LOG_DIR/release.json"
if curl -fsSL -H 'Accept: application/vnd.github+json' \
     -H 'User-Agent: entangled-fresh-install-acceptance' "$api" -o "$release_json"; then
  check 0 "the release resolves without credentials" "$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$release_json" | head -1)"
else
  check 1 "the release resolves without credentials" "$api"
  echo "cannot continue without a release" >&2; exit 1
fi

resolved_tag=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$release_json" | head -1)
version="${resolved_tag#v}"
url="https://github.com/$REPO/releases/download/$resolved_tag/$ASSET"

engine="$BIN_DIR/entangled"
if curl -fsSL "$url" -o "$engine"; then
  chmod +x "$engine"
  check 0 "the Linux engine downloads unauthenticated" "$url ($(stat -c%s "$engine") bytes)"
else
  check 1 "the Linux engine downloads unauthenticated" \
    "$url — the WSL backend installs THIS asset; without it \`entangled wsl install-engine\` has nothing to fetch"
fi

# ---------------------------------------------------------------------------
say "environment — is this really a stranger?"
# ---------------------------------------------------------------------------

probe="$CWD_DIR"
repo_above=""
while [ "$probe" != "/" ]; do
  if [ -f "$probe/Cargo.toml" ] || [ -d "$probe/artifacts" ]; then repo_above="$probe"; break; fi
  probe=$(dirname "$probe")
done
if [ -z "$repo_above" ]; then
  check 0 "the working directory has no repository at or above it" "$CWD_DIR"
else
  check 1 "the working directory has no repository at or above it" \
    "found a checkout at $repo_above — its artifacts/ could answer for the installation"
fi

# ---------------------------------------------------------------------------
say "doctor — what a newcomer is told"
# ---------------------------------------------------------------------------

if [ ! -x "$engine" ]; then
  check 1 "the engine runs" "nothing downloaded"
else
  stranger "$LOG_DIR/version.log" 60 -- "$engine" --version
  ver_rc=$?
  ver=$(cat "$LOG_DIR/version.log")
  check $ver_rc "the downloaded engine runs on this host" "$ver"
  case "$ver" in
    *"$version"*) check 0 "it reports the published version" "$ver" ;;
    *) check 1 "it reports the published version" "expected $version, got $ver" ;;
  esac

  stranger "$LOG_DIR/doctor-before.log" 240 -- "$engine" doctor
  doctor_rc=$?
  before=$(cat "$LOG_DIR/doctor-before.log")
  sed 's/^/  /' "$LOG_DIR/doctor-before.log"

  if [ $doctor_rc -eq 0 ]; then
    note "this host can run VMs" "doctor exited 0"
  else
    note "this host cannot run VMs (a GitHub runner never can)" \
         "the inventory below must be reported anyway"
  fi

  # doctor must answer the inventory questions whether or not KVM is usable —
  # the host that cannot run a VM is exactly the host whose owner needs to know
  # what they have. See apps/entangled/src/doctor.rs report().
  if printf '%s' "$before" | grep -qE '^[[:space:]]*install[[:space:]]+:'; then
    check 0 "doctor reports the install inventory even without a hypervisor"
  else
    check 1 "doctor reports the install inventory even without a hypervisor" \
      "a release older than the doctor change that prints what the host HAS before failing on what it cannot do"
  fi

  # A fresh Linux engine has no firmware anywhere: no installer laid one down,
  # the cache is empty and there is no checkout. "MISSING" is the correct
  # answer — what matters is that the next line is a command.
  fw=$(printf '%s\n' "$before" | grep -E '^[[:space:]]*firmware' | head -1)
  if printf '%s' "$fw" | grep -q MISSING; then
    if printf '%s' "$before" | grep -q 'entangled fetch firmware'; then
      check 0 "no firmware, and doctor names the command that gets one" "$(printf '%s' "$fw" | sed 's/^ *//')"
    else
      check 1 "no firmware, and doctor names the command that gets one" \
        "the dead end this whole exercise exists to prevent: MISSING with no way forward"
    fi
  elif [ -n "$fw" ]; then
    check 0 "doctor resolved a firmware" "$(printf '%s' "$fw" | sed 's/^ *//')"
  else
    check 1 "doctor reports on the firmware at all" "no firmware line in the output"
  fi

  # ---------------------------------------------------------------------------
  say "fetch — and the command works, with no token"
  # ---------------------------------------------------------------------------

  stranger "$LOG_DIR/fetch.log" 300 -- "$engine" fetch firmware
  fetch_rc=$?
  tail -4 "$LOG_DIR/fetch.log" | sed 's/^/         /'
  check $fetch_rc "a newcomer with no GitHub credentials can fetch the firmware" \
    "$(tail -1 "$LOG_DIR/fetch.log")"

  cached=$(find "$HOME_DIR/.cache/entangled/firmware" -name CLOUDHV.fd 2>/dev/null | head -1)
  if [ -n "$cached" ]; then
    check 0 "the firmware lands in the (empty) verified cache" "$cached"
  else
    check 1 "the firmware lands in the (empty) verified cache" "$HOME_DIR/.cache/entangled/firmware"
  fi

  stranger "$LOG_DIR/doctor-after.log" 240 -- "$engine" doctor
  after=$(cat "$LOG_DIR/doctor-after.log")
  fw2=$(printf '%s\n' "$after" | grep -E '^[[:space:]]*firmware' | head -1)
  if printf '%s' "$fw2" | grep -q 'the verified cache'; then
    check 0 "doctor now resolves the fetched firmware, from the verified cache" \
      "$(printf '%s' "$fw2" | sed 's/^ *//')"
  else
    check 1 "doctor now resolves the fetched firmware, from the verified cache" \
      "${fw2:-no firmware line} — a fetch that doctor cannot then find is a fetch nobody can use"
  fi

  # ---------------------------------------------------------------------------
  say "engine — the surface the Windows installer drives"
  # ---------------------------------------------------------------------------

  # On Linux `entangled wsl install-engine` is the wrong host and must say so
  # rather than pretending; the Windows side is where it does the work. What
  # matters here is that the subcommand exists in the published binary at all,
  # because the manager and the installer both shell out to it by name.
  stranger "$LOG_DIR/wsl-help.log" 60 -- "$engine" wsl install-engine --help
  if grep -q -- '--distro' "$LOG_DIR/wsl-help.log"; then
    check 0 "the published engine carries the wsl install-engine surface"
  else
    check 1 "the published engine carries the wsl install-engine surface" \
      "$(head -3 "$LOG_DIR/wsl-help.log")"
  fi
fi

# ---------------------------------------------------------------------------
say "result"
# ---------------------------------------------------------------------------

if [ "$KEEP" -eq 0 ]; then
  rm -rf "$HOME_DIR" "$CWD_DIR" "$BIN_DIR"
  note "scratch removed" "kept: $LOG_DIR"
fi

printf '%d passed, %d failed\n' "$PASS" "$FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '\033[31mFAILED:\033[0m\n'
  for f in "${FAILED[@]}"; do printf '  %s\n' "$f"; done
  exit 1
fi
printf '\033[32mfresh-install acceptance passed\033[0m\n'
