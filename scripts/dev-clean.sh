#!/usr/bin/env bash
# dev-clean.sh — sweep disposable per-task cargo target dirs on BOTH hosts.
#
# The dev-environment skill's rule: every task's target dir
# ($HOME/entangled-target-<branch> in WSL, %LOCALAPPDATA%\entangled-target-<branch>
# on Windows) is disposable and must be deleted when the task ends. This script
# finds the strays and deletes everything not on the keep list. It never touches
# VM disks (~/entangled-vms), media caches (~/.cache/entangled*), or the repo.
#
# Usage (from Git Bash on Windows, or from inside WSL):
#   scripts/dev-clean.sh                       # dry run: list candidates
#   scripts/dev-clean.sh --mine virgl2         # delete YOUR task's dirs only
#   scripts/dev-clean.sh --delete --all        # sweep every stray (see below)
#   scripts/dev-clean.sh --delete --all --keep virgl2   # ... but spare one
#   scripts/dev-clean.sh --sizes               # also measure sizes (slow on
#                                              # Windows: MSYS du over NTFS
#                                              # takes minutes per GB)
#
# --mine is what an agent finishing a task should run: it removes exactly
# entangled-target-<suffix> on both hosts and nothing else. Sweeping other
# tasks' dirs needs the explicit --all, because a dir that looks stray may
# belong to a build running right now.
#
# This script only ever removes cargo target dirs. It never stops WSL, never
# touches Docker, never compacts the VHDX and never kills a process: those are
# machine-wide operations that interrupt the user's work, and they are the
# user's call (see the dev-environment skill).
#
# "main" and the bare "entangled-target" are kept by default: they are the
# long-lived build caches for the checkout itself. Pass --keep <suffix> for
# every branch dir still in use by a running task.

set -uo pipefail

DELETE=0
SIZES=0
ALL=0
MINE=""
KEEP=("main")
while [[ $# -gt 0 ]]; do
    case "$1" in
        --delete) DELETE=1 ;;
        --all)    ALL=1 ;;
        --mine)   shift; MINE="${1:?--mine needs a branch suffix}"; DELETE=1 ;;
        --sizes)  SIZES=1 ;;
        --keep)   shift; KEEP+=("${1:?--keep needs a suffix}") ;;
        -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

if [[ $DELETE -eq 1 && -z "$MINE" && $ALL -eq 0 ]]; then
    cat >&2 <<'MSG'
refusing to delete without a scope.

  --mine <branch>   remove only entangled-target-<branch> (what a finishing
                    task should run)
  --delete --all    sweep every stray dir — only when you know no other build
                    is running; another task's dir looks exactly like a stray

MSG
    exit 2
fi

in_wsl() { grep -qi microsoft /proc/version 2>/dev/null; }

kept() {
    local name="$1" k
    [[ "$name" == "entangled-target" ]] && return 0
    for k in "${KEEP[@]}"; do
        [[ "$name" == "entangled-target-$k" ]] && return 0
    done
    return 1
}

size_of() { # prints a human size, or nothing when sizing is off
    [[ $SIZES -eq 1 || $(in_wsl; echo $?) -eq 0 ]] || return 0
    local kb
    kb=$(du -sk "$1" 2>/dev/null | cut -f1) || return 0
    [[ -n "$kb" ]] || return 0
    awk -v k="$kb" 'BEGIN { printf (k>1048576) ? "(%.1f GiB)" : "(%.0f MiB)", (k>1048576) ? k/1048576 : k/1024 }'
}

scan() {
    local label="$1"; shift
    echo "== $label"
    local found=0 dir name
    for dir in "$@"; do
        [[ -d "$dir" ]] || continue
        name="$(basename "$dir")"
        if [[ -n "$MINE" ]]; then
            # Scoped mode: this task's dir and nothing else.
            [[ "$name" == "entangled-target-$MINE" ]] || continue
            found=1
            rm -rf "$dir" && echo "  DELETED $name"
            continue
        fi
        if kept "$name"; then
            echo "  keep    $name"
            continue
        fi
        found=1
        if [[ $DELETE -eq 1 ]]; then
            rm -rf "$dir" && echo "  DELETED $name $(size_of "$dir" 2>/dev/null)"
        else
            echo "  stray   $name $(size_of "$dir")  [--delete removes]"
        fi
    done
    if [[ $found -eq 0 ]]; then
        echo "  (nothing to clean)"
    fi
    return 0
}

if in_wsl; then
    scan "WSL \$HOME" "$HOME"/entangled-target-*
    echo
    echo "WSL filesystem:"
    df -h / | tail -1
    echo "note: freeing space inside WSL does NOT shrink the VHDX on C: — it only"
    echo "      stops further growth. Reclaiming it needs 'wsl --shutdown' plus a"
    echo "      diskpart 'compact vdisk' run as admin, which stops every VM: ask"
    echo "      the user before doing that."
else
    lad="${LOCALAPPDATA:-}"
    command -v cygpath >/dev/null 2>&1 && [[ -n "$lad" ]] && lad="$(cygpath -u "$lad")"
    scan "Windows LOCALAPPDATA" "${lad:-/c/Users/$USERNAME/AppData/Local}"/entangled-target-*
    echo
    df -h /c 2>/dev/null | tail -1
    echo
    if command -v wsl.exe >/dev/null 2>&1; then
        args=""
        [[ -n "$MINE" ]] && args="$args --mine $MINE"
        [[ $DELETE -eq 1 && -z "$MINE" ]] && args="$args --delete"
        [[ $ALL -eq 1 ]] && args="$args --all"
        [[ $SIZES -eq 1 ]] && args="$args --sizes"
        for k in "${KEEP[@]}"; do args="$args --keep $k"; done
        repo="$(cd "$(dirname "$0")/.." && pwd)"
        wslrepo="$(printf '%s' "$repo" | sed -E 's#^/([a-zA-Z])/#/mnt/\1/#')"
        wsl.exe -d Ubuntu -e bash -lc "cd '$wslrepo' && bash scripts/dev-clean.sh$args" | tr -d '\0'
    fi
fi
