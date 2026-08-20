---
name: dev-environment
description: >
  How to build, test and run Entangled Desktop on this two-host machine
  (Windows native + WSL Ubuntu), where to put cargo target dirs and VM disks,
  and the disk-space discipline that keeps the WSL VHDX from eating drive C:.
  Load this BEFORE your first build, before creating a worktree, before
  launching any demo VM, and before any long test campaign — even if your task
  is about a single subsystem. Every agent that ever builds anything in this
  repo needs the target-dir and cleanup rules in here; twelve agents who
  skipped them once cost the machine ~100 GB and two WSL crashes.
---

# Entangled Desktop — dev environment

Two supported hosts, both exercised for every change:

- **Linux/KVM** lives in WSL Ubuntu (`wsl -d Ubuntu`, user `spider`, has
  `/dev/kvm` via nested virtualization). This is the MVP target and the only
  host for `entangled install`.
- **Windows native** builds and tests the whole workspace too (WHP backend,
  manager, installer). cargo is on PATH in PowerShell. This machine has the
  Windows Hypervisor Platform feature, so `whp_*` acceptance tests run for
  real here (they self-skip on machines without it).

Canonical command shapes:

```bash
# Linux side (from Windows):
wsl -d Ubuntu -e bash -lc "cd /mnt/d/entangled-desktop && export CARGO_TARGET_DIR=$HOME/entangled-target-main && cargo test --workspace"
```

```powershell
# Windows side:
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\entangled-target"   # or -<branch> for a worktree
cargo test --workspace
```

Full pre-merge verification checklist (both hosts): `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test
--workspace`, `cargo deny check` (Linux is enough for deny; it is a license
gate and licenses don't differ per OS).

## Disk discipline — read this even if nothing else

This machine has three chronically scarce disks and a history of self-inflicted
outages. The mechanics:

- **D: is ~50 GB.** A full D: shows up as rustc "IO failure on output stream".
  NEVER set a target dir inside the repo or any worktree on D:.
- **The WSL filesystem lives in a VHDX file on C: that only ever grows.**
  Deleting files inside WSL does not shrink the file on C:; it only stops
  further growth. Reclaiming the space needs `wsl --shutdown` plus a diskpart
  `compact vdisk` run as admin — which the resident assistant coordinates with
  the user, because it kills everything running in WSL. Your job is to not
  create the bloat in the first place.
- **C: hitting zero kills WSL hard**: the ext4 filesystem flips read-only
  mid-build and every running VM dies. It has happened twice in one day.

Rules that follow — and note how narrow your mandate is:

1. **Clean up after yourself, and only after yourself.** Target dirs are
   per-task and disposable: WSL `$HOME/entangled-target-<branch>`, Windows
   `$env:LOCALAPPDATA\entangled-target-<branch>`. Each grows to 5–20 GB.
   **Deleting your own two dirs is your last action** — a task that leaves
   them behind is not finished:

   ```bash
   scripts/dev-clean.sh --mine <branch>      # both hosts, your dirs only
   ```

2. **Machine-wide cleanup is the user's call, never yours.** This machine is
   someone's working desktop: containers they need, VMs they are watching,
   editors attached to WSL. So do not run any of these, even when disk space
   looks scary — and especially not "helpfully" in the background:

   - `wsl --shutdown` / `wsl --terminate` (kills every VM and build in WSL),
   - stopping Docker Desktop or `com.docker.service`, `docker system prune`,
   - VHDX compaction (`diskpart compact vdisk`),
   - `scripts/dev-clean.sh --delete` without `--mine` (that sweeps *other*
     tasks' dirs, including in-flight ones),
   - killing processes you did not start — other agents' builds, the user's
     `entangled` VMs, anything with a window.

   If you genuinely cannot proceed for lack of space, delete your own dirs,
   then **stop and say so in your report**. "I freed space by restarting X"
   is a worse outcome than "I stopped, here is what needs freeing" — one of
   those interrupts the user's work without asking.

3. **Check before you build big.** `df -h /` inside WSL and free space on C:
   before a workspace build, a VM install, or an ISO download. **Under 10 GB
   free on C: is a stop sign**: run `scripts/dev-clean.sh --mine <branch>`,
   and if that is not enough, report it — see rule 2.

4. **VM disks live in WSL-native `~/entangled-vms`**, never on `/mnt/d`
   (drvfs cannot do sparse files — a 16 G sparse image allocates 16 G real).
   Media/kernel caches live in `~/.cache/entangled*`. Both are shared
   machine state: never delete them to free space.

5. **Do not run heavy Windows builds concurrently with WSL VM boots or
   installs.** Both dig into C: at once (LOCALAPPDATA target dirs + VHDX
   growth + WSL swap); that combination has cost this machine two outages.

## Running VMs and demos

- **WSL kills the whole distro ~8 s after the last `wsl.exe` client exits** —
  even with `setsid`/`nohup`/`disown`. A demo VM launched via
  `bash -lc '... &'` dies mid-boot with no trace. Launch VMs as the
  FOREGROUND command of a session that stays attached (a long-lived
  background *task* on the Windows side whose `wsl` process stays alive).
- GUI windows from WSL appear via WSLg (Wayland). WSLg quirks (decorations,
  cursors, RAIL) are documented in the host-display skill.
- Never `kill` a windowed VM: a client dying with a pending pointer
  constraint has crashed WSLg's compositor and taken every WSLg window with
  it. Use Ctrl+Alt+Q, the window ✕, or ACPI power-off.
- One WHP VM per process on Windows (hypervisor limit); `entangled run` is
  one process per VM by design.

## Artifacts are gitignored and go stale

`artifacts/` (test kernel, initramfs, firmware) is built per-checkout, not
committed. Consequences that have produced false test failures three separate
times:

- After merging or editing anything under `guest/test-rootfs/`, rebuild:
  `bash scripts/build-test-initramfs.sh` — or the boot tests run the OLD
  initramfs and fail (or silently skip new probes). Until 2026-08-20 the
  script itself could hand you a stale one: cargo honours `CARGO_TARGET_DIR`,
  which everyone here sets, but the script copied the init binary from the
  crate's own `target/`. It now resolves the real output path and refuses to
  package an init older than its sources — if you see that refusal, your
  build failed, it did not.
- The UEFI firmware comes from `bash guest/firmware/build-cloudhv.sh`
  (~2.5 min, pinned EDK2, pflash PCDs asserted). A fresh checkout or a new
  worktree has NO firmware — `run` with UEFI boot fails with "cannot read
  firmware". Worktrees each need their own artifacts (or run the script once
  in the checkout you test from).
- `scripts/fetch-test-kernel.sh` fetches the bootstrap kernel;
  `scripts/fetch-ubuntu-iso.sh [desktop]` fetches + verifies ISOs into the
  shared cache.

## Worktree workflow for parallel agents

- One agent = one worktree (`git worktree add ../entangled-worktrees/<name>
  -b <branch>`) = one target-dir suffix on each host. Never touch main or
  another worktree.
- Commit per completed deliverable, author
  `doctorspider42 <pawel.pajak@hotmail.com>`; do not push — the coordinator
  merges after verification.
- Architecture ground rules live in CLAUDE.md (hard rules) and the ADRs:
  0002 (portability seams — read before OS-specific code), 0003 (UEFI),
  0004 (3D). The subsystem skills in `.claude/skills/` carry the earned
  gotchas — load the one for the subsystem you touch; add what you learn.
- Leave the machine as you found it: **your** target dirs deleted, no orphan
  `entangled` processes that **you** started
  (`wsl -d Ubuntu -e bash -lc "pgrep -a entangled"` — but a VM you did not
  launch belongs to the user or another agent, so leave it alone), demo VMs
  you started shut down cleanly via Ctrl+Alt+Q / ACPI power-off. Services,
  containers and other tasks' work are out of scope by construction: see
  rule 2 above.
