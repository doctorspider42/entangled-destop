---
name: gui-manager
description: The native desktop manager (egui/eframe on wgpu) — Deep Field theme tokens, the machine-centric layout, view architecture, child-process supervision of the entangled CLI, and how to run it under WSLg (backlog EPIC 16, crate apps/manager). Load before touching the GUI.
---

# GUI manager (`entangled-manager`)

Scope: backlog EPIC 16 (GUI-1601…1606). Crate: `apps/manager`, binary
`entangled-manager`. It manages VMs; it never links VMM code. Every action is
an `entangled` CLI invocation as a **child process**, so anything the CLI can do
the GUI can do, and a GUI crash can never take a guest down.

## Architecture

```
main.rs      CLI flags (--vm-dir, --entangled, --mock, --screenshot[-view]) + tracing
app.rs       ManagerApp (eframe::App): state, Action loop, frame layout
  ui/        views: the machine sidebar, All machines (cards.rs), one page
             per machine (machine.rs), the wizard and dialogs with the
             editor drawer, Storage (Disks + Snapshots), Health, the
             activity pane and toasts
  theme.rs   every colour, radius and font size in the product
  logo.rs    the linked-pair mark, app icon and spinner (painter, no assets)
settings.rs  ~/.config/entangled/manager.toml, typed load/save
update.rs    startup update check (GitHub /releases/latest) + installer
             download/launch, both on background threads; banner in cards.rs
discovery.rs VM directory scan through control-api, delete plan + guards
snapshots.rs reading the *.esnap files in the VM directory, and the verdict
             that decides whether Resume is even offered (ADR-0006)
launcher.rs  finding the engine (path + origin + version), Runner, install/run
             argument vectors
backend.rs   where a machine runs (native vs WSL), the capability matrix and
             Windows→WSL path translation — pure logic, tests on both hosts
wslengine.rs the WSL backend's pre-flight: is there a Linux engine in the
             distribution, and the verified download that installs one
picker.rs    native file/folder dialogs, off-thread (rfd, XDG portal on Linux)
hostcheck.rs `entangled doctor` on a worker thread, parsed into coloured rows
process.rs   Supervisor: children, log tailing, stop/kill, state machine
diagnose.rs  CLI failure output -> one actionable sentence
```

Two rules keep it honest:

1. **Views never mutate.** `ui::*` functions take state plus `&mut Vec<Action>`
   and push intent; `ManagerApp::apply` is the only place that touches state,
   the filesystem or processes. Adding a button = adding an `Action` variant.
2. **Nothing blocks the frame loop.** Directory scans run on the `vm-scan`
   worker thread (request/result channels), each child has its own watcher
   thread, and both wake the UI through a `process::Waker`
   (`ctx.request_repaint`). No `unwrap`/`expect` on any runtime path; failures
   become toasts (transient) or banners (persistent, e.g. VM directory missing,
   CLI not found).

### Child processes (GUI-1603/1605)

- Output goes to `<vm-dir>/<name>-{run,install}.log` and is **tailed** from
  there — never a pipe. A pipe dies with the manager and would break the
  guest's serial console; a file also leaves a post-mortem log.
- Children get their **own process group** (`detach_process_group`): a signal
  aimed at the manager (terminal SIGHUP, Ctrl+C, a supervisor killing the job)
  must not reach a VM. This was a real bug, found by killing the manager's
  group with a VM running.
- Stop = SIGTERM on Unix (`entangled run` turns that into an orderly shutdown),
  `Child::kill` on Windows, escalating to a kill after `STOP_GRACE` (20 s).
  Pressing Stop twice kills immediately.
- Closing the manager leaves running VMs alone; `Child` is not reaped on drop.
- Signal tests must wait for a child-emitted readiness marker after installing
  its handler; the supervisor's Running state only means spawn succeeded.
  A shell fixture should exec its sleeper so forcing the child down leaves no
  orphan. The SIGTERM-ignore test raced on the macOS release runner otherwise.
  **Known gap:** a restarted manager does not adopt those orphans — the card
  shows Stopped while the VM runs, and Start then fails with the busy-TAP
  message. Adoption would need a pid file plus a portable liveness check.

#### The engine is resolved, never requested

`launcher::locate_engine` returns the path **and how it was found**
(`Chosen` / `BesideManager` / `OnPath`), and a background probe fills in
`--version`. Settings shows one status line — "Engine: entangled 0.2.0 — next to
the manager  [ready]" — and the override lives under **Advanced and
diagnostics**, collapsed. A GUI user must never be asked for a path the
application can work out.

When resolution fails it is a *problem with a fix button*, not an empty field:
the banner's action opens the file picker directly and applies the answer
immediately (`collect_picker` special-cases `EngineBinary` because there is no
form open to hold it).

## Health (`hostcheck.rs` + `ui/diagnostics.rs`)

**The user never meets the engine.** Entangled is one application: no
"engine", "CLI", "next to the manager", program paths or commands in the
ordinary UI. The part that runs machines is found by itself and only mentioned
when it is missing — as "Part of Entangled is missing … Reinstalling fixes it",
with *Locate it…* for whoever has a copy. Hypervisors are "Windows" and "WSL",
not WHP/KVM. Anything technical lives under a collapsed **Advanced** (Settings)
or **Technical details** (Health).

Health is a checklist built by `diagnostics::checks(app)`: each line a plain
sentence, a state (`Ok`, `Optional`, `Attention`, `Problem`, `Pending`) and, when
there is a fix, a button. The facts still come from `entangled doctor`, run on a
worker thread when the page opens (`SwitchView(Diagnostics)` starts it) — it does
**not** reimplement the checks, `doctor` knows them per host and two copies
would drift. What the view adds is translation: a `MISSING` line becomes a
sentence, and `entangled fetch firmware` / `bootstrap-kernel` become a
**Download** button (`Action::Fetch(FetchKind)`, `Runner::fetch_command`, run on
its own thread, re-checking when it finishes). WSL is its own line from
`app.wsl_engine` with **Set up WSL**; it is `Optional` unless a machine (or the
default) runs there. The sidebar's "N to finish" is `diagnostics::to_fix`, the
same list, so the two cannot disagree. The raw report, the program path and the
command are under Technical details. `--mock` shows `hostcheck::mock_report()`
so screenshots are identical everywhere.

Form-row tooltips (`ui::form_label`) are painted, not laid out as widgets: egui
hands the hover to the top-most widget, so a `Label` inside the cell swallowed
it and the `?` never answered.

## Install flow (GUI-1602)

The x86-64 wizard also offers Fedora and **Other Linux / own ISO**. Fedora
uses the existing `install fedora` path (Everything netinst, cached or local,
Kickstart when automatic). Other Linux uses `install other --iso`, always
interactive and windowed: the CLI creates a UEFI/PCI profile with a writable
disk, persistent NVRAM, usernet and the read-only ISO, then starts it. The
profile exists before boot and retains the ISO for interrupted installations.
The manager tracks this child as `TaskKind::Run`, with a control pipe, not as
a completed automatic installation; never stamp resources over its live profile.
After powering off, the user ejects the ISO in Settings > Startup and saves.
The console offers named `serial-key` controls (shared vocabulary in control-api)
for the serial-only firmware/bootloader menus. ISO drag-and-drop selects this
manual path; native Apple Silicon keeps its Ubuntu ARM64-only provisioning.

The four-stage wizard collects system/media, name/resources, storage and a
review before it launches anything. It supports Debian's verified variants and
Ubuntu through the verified cache or an explicit local `--iso`. Storage is an
explicit choice between a new sparse RAW image and an existing image in the VM
directory; the latter is never recreated or truncated.

Which family the wizard *opens* on is per host: `GuestFamily::default_for_host()`
is Ubuntu on Windows and Debian on Linux. That is now a *preference*, not a
capability — Ubuntu installs entirely offline from a verified ISO while d-i
downloads the system from a mirror, so it is the better first experience on a
laptop. `suggest_name` uses the same answer, so a fresh wizard on Windows
proposes `ubuntu-1` — the name becomes the disk, the profile and the hostname,
so the wrong distro's name outlives the wizard. Pre-flight is per family
(`launcher::missing_install_artifact`): the UEFI firmware for Ubuntu, the
bootstrap kernel **and initramfs** for Debian. Gating both on the kernel is what
once made the only installer that works on Windows unreachable there.

**`Backend::debian_install_block` is gone, and do not bring it back.** It greyed
out the Debian card on the Windows backend because the bootstrap kernel could
not be built there. The kernel is now *downloaded*
(`entangled fetch bootstrap-kernel`, digest-pinned), so the card is offered on
both hosts and the only question left is whether this machine has the files yet
— which is a pre-flight with a command in its message, not a capability gate.
A greyed-out control that says "this backend cannot" about something the backend
*can* is worse than no gate at all.

`missing_install_artifact` therefore looks in three places for the Debian pair,
mirroring `apps/entangled/src/bootstrap.rs` (which is the authority):
`ENTANGLED_BOOTSTRAP_DIR`, the child's working directory, then any tag directory
under `<cache>/bootstrap/`. The cache arm is deliberately *more* generous than
the CLI's — the CLI knows which release tag this build pins and the manager
does not — because the failure mode of being generous is an install that
starts and gets a precise message from the CLI, which beats any message this
check could write.

**The same applies to the firmware, and for the same reason it once did not.**
`launcher::locate_firmware` mirrors `apps/entangled/src/firmware.rs`:
`ENTANGLED_FIRMWARE_DIR`, `artifacts\firmware\` **beside the program** (what the
Windows installer ships), any tag directory under `<cache>/firmware/`, then the
working directory. Use it — never `cwd.join(UEFI_FIRMWARE).is_file()`, which is
what the card warnings and the wizard used to do and what produced the worst
message this project has shipped: a fresh Windows install told the user to
"copy artifacts/firmware/CLOUDHV.fd in from a Linux checkout or a release",
having neither and there being no such release. Every firmware message must
name something the person in front of *that* computer can do — `entangled fetch
firmware`, or reinstalling — which is what `launcher::FIRMWARE_FIX` now says.
The machine editor uses the same lookup twice: to offer a real path when the
field is empty, and to say *which* firmware a profile with a stale path will
actually start on, because the CLI falls through rather than refusing.
The resulting `entangled install <debian|ubuntu> --disk <path> --size <n>G
[--variant <v>] --memory-mib <max(1536, m)> --name <name> [--iso <path>]
[--auto] [--headless] --network usernet` is spawned with the configured
working directory. `--variant` is Debian-only — `install ubuntu` ignores the
flag, and a setting that does nothing on the reviewed command line is worse
than an absent one. `--network` is always
`control_api::DEFAULT_NEW_MACHINE_NETWORK` (usernet): it is the CLI's own
default on both hosts since ADR-0002's installed-network amendment, and it is
spelled out because a WSL engine may be an older release whose Linux default
was TAP. The CLI writes
the profile itself with its own resource defaults, so on success the
manager stamps the wizard's memory/vCPU choice onto the profile — and, if the
profile has no `[network]` (an engine from before that amendment wrote Ubuntu
profiles without one), the section a current engine would have written:
usernet plus `control_api::new_machine_mac(name, disk)`
(`discovery::apply_resources`). `--vcpus` is deliberately **not** passed: the
manager may be driving an older WSL engine that has no such flag, and the stamp
already does the job.

**The two defaults must agree, and the vCPU one is shared code.** A new machine
gets `control_api::host_default_vcpus()` — half the host's logical CPUs,
clamped to 2..=8 — from `entangled install` (for the profile it writes) and
from the wizard (`Settings::wizard_vcpus`, the slider's starting value). The
display is 1920×1080 either way, since the wizard has no display step and the
CLI writes `DisplaySection::default()`. `Settings::default_vcpus` is an
`Option`: absent means "follow the host" and is not written back. A settings
file that says `default_vcpus = 2` loads as absent (`STALE_VCPUS_DEFAULT`),
because every earlier manager wrote that 2 on every save, with no control
anywhere that set it — the development host's own `manager.toml` had it. Taking
it as a choice kept an upgraded manager stamping 2 vCPUs over the CLI's 8.
`--mock` pins the default to an 8-thread host's 4, so screenshots stay
identical everywhere. While the install runs the VM has no profile
yet, so `PendingInstall` gives it a card with the Installing badge.

## Form conventions — read before adding any field

The GUI is the product; the CLI is the engine underneath it. Two rules follow,
and both are enforced by helpers in `ui/mod.rs` rather than by discipline at the
call site. Do not hand-roll a labelled row.

### 1. The explanation is a tooltip, not body text

**User-facing explanation goes in a tooltip; the form stays uncluttered.**
Every piece of jargon the product exposes — firmware, NVRAM, VirGL, transport,
network backend, cdrom, the backend choice — gets a short label and a sentence
that teaches, hanging off the label on hover. A paragraph of helper text under
every field turns a five-field form into a wall of prose that experienced users
skim past and new users still do not read.

- `ui::form_row(ui, LABEL, tooltip, |ui, field_w| …)` — the label cell is the
  hover target and shows a quiet `?` when a tooltip exists.
- A **disabled** control puts its reason in the tooltip too, and a short version
  inline. `backend::Block { short, long }` carries both: `short` sits under the
  control, `long` is the tooltip. Do not paste the long one inline.
- `ui::form_note` is for *state*, not explanation — "not found: …", "= 32 GiB",
  "used by ubuntu-lab". If a note is teaching, it belongs in the tooltip.

### 2. One label column, one field width

`FORM_LABEL_W` (label cell, exact width — long labels truncate rather than push
the field right) and `FORM_TRAIL_W` (reserved for a Browse button whether or not
the row has one). Stacked rows therefore line up on both edges.

- Call **`ui::form_scope(ui)` once at the top of every form section.** Without
  it each row measures `available_width()` for itself, and the container grows
  as wrapped notes are added to it — row three ends up eight pixels wider than
  row one and the column visibly staircases. That bug was real and is invisible
  until you measure the PNG. `edit_panel_heading` calls it for the editor
  sections; the wizard, Settings and the disk dialogs call it themselves.
- A `ComboBox` draws ~8.5 px narrower than the width it is given. Pass
  `ui::combo_width(field_w)`, never `field_w`, or combos and text fields end on
  different pixels.
- Paths use `ui::path_row`, which is `form_row` + text field + Browse and
  returns `true` on click; the caller pushes `Action::PickPath(target)`.

### 2a. A default is not written

The manager may be driving an **older** engine (a WSL release), and every
profile section denies unknown keys — so an editor field whose value is the
engine's own default must leave the key *absent*, not spell the default out.
`EditForm::to_config` is where this lives, and `Display & GPU` shows the
pattern (2026-10-11): `gpu_boost` is a checkbox that is on unless the profile
says `false`, writes nothing while ticked and `false` when not, keeps an
explicit `true` only beside the GPU desktop (the engine refuses it elsewhere);
`gpu_memory_mib` is a checkbox plus a number, absent until ticked, with the
typed number kept across a toggle. Fields that only mean something with one
renderer (boost, VRAM cap) are shown only with it, but their values ride
through a renderer switch untouched. "Match monitor" on the refresh row is a
*value* (`control_api::refresh::host_default_refresh_hz`, the install rule,
read when the editor opens — 60 in `--mock`), never a key.

### 3. No path is typed unless the user wants to type it

Every path in the product has a native picker (`picker.rs`, `rfd`), and the text
field stays as the editable fallback. Rules:

- The dialog **never** runs on the egui thread — `picker::open` spawns a thread
  and the answer arrives through the waker/channel pattern.
- One dialog at a time (`ManagerApp::picker`), because they are OS-modal anyway.
- `picker::spec` is the one table of title/kind/filter per target: add a
  `PickTarget` there and the filter cannot be wrong at the call site.
- `--mock` intercepts `Action::PickPath` and toasts instead: a screenshot run
  must never stop on a modal dialog.
- On Linux rfd is built against the **XDG portal**, never `gtk3` — GTK is LGPL
  and this binary carries no copyleft (ADR-0001, `deny.toml`).

## Where a machine runs (`backend.rs`)

On Windows the manager offers two hypervisors: **Windows (WHP)**, running
`entangled.exe` natively, and **WSL (KVM)**, running the Linux build through
`wsl.exe -d <distro> --cd <windows cwd> -e <linux entangled> …`.

- The choice is **manager state, not profile state** (`Settings::vm_backends`,
  keyed by VM name). The same profile is meant to boot on either host (ADR-0002);
  writing the backend into it would make the file host-specific.
- Every path argument goes through `Runner::path_arg`, which is identity
  natively and `backend::to_wsl_path` under WSL (`D:\vms\x.toml` →
  `/mnt/d/vms/x.toml`). A UNC or relative path is **refused when the spec is
  built**, with the fix in the message — never discovered inside the engine.
- `backend::reachability` also warns (does not refuse) when a machine lives on a
  Windows drive: drvfs has no sparse files, so a 16 GiB image really occupies
  16 GiB.
- The control pipe survives `wsl.exe`, so Suspend and Restart still work. The
  window does not come up on the Windows desktop — WSLg opens its own.
- **Capabilities follow the kernel, not the host.** `Backend::virgl_block`
  (through `gpu_block(GpuRenderer)`, which blocks only virgl — the Venus GPU
  desktop runs on either engine), `tap_block` and `debian_install_block` are
  the single source of truth for
  "this fails at boot"; the editor and the wizard grey the control out and show
  `.short`, and the same check runs again at submit time in case the setting
  changed underneath the form.

### The engine inside WSL (`wslengine.rs` + `control_api::wsl{,_engine}`)

The WSL backend has a hole nothing else in the product has: **a Windows install
ships no Linux binary**, so `wsl.exe -d Ubuntu -e entangled …` on a fresh
machine runs a program that is not there, and WSL reports it as

```text
<3>WSL (13) ERROR: CreateProcessEntryCommon:505: execvpe entangled failed 2
```

after the user has filled in a whole wizard. Three rules close it, and all three
must stay closed:

1. **Ask before launching.** `control_api::wsl::probe_with` asks three questions
   in order — does `wsl.exe` run, does the distribution exist, does the engine
   inside it answer `--version` — and every "no" becomes an `EngineFault` with a
   `what` and a `fix`. It lives in `control-api`, not here, because
   `entangled doctor` prints the same verdict in its `engines` section and two
   copies would drift. The runner is a parameter, so the whole flow (including
   every classification) is unit-tested on both hosts with no `wsl.exe`.
2. **Never let the raw noise out.** `control_api::wsl::translate` turns
   `execvpe … failed 2` (and 13, and 8) into a sentence, and `diagnose::explain`
   tries it first on every failed child's log — a distribution can always be
   shut down between the check and the launch.
3. **Offer the engine, do not ask for a path.** `wslengine::spawn_install`
   downloads the pinned Linux build, verifies it, copies it into the
   distribution and re-runs the check. The download, the digest check and the
   copy are **not in this crate**: they are `control_api::wsl_engine`
   (`engine-install` feature), because `entangled wsl install-engine` performs
   the identical install for the Windows installer's optional task, and a
   second copy of a security check is a security bug waiting for a release.
   This module supplies the one thing only this binary knows — its own
   `ENTANGLED_LINUX_ENGINE_SHA256`, version and cache — as a `wsl_engine::Pin`.

`ManagerApp::wsl_engine` holds the answer, `ensure_wsl_check` starts a probe
whenever the distribution or the engine path changes (never on the frame loop —
a cold distribution takes seconds to boot), and `backend_block` is the single
gate: `start()` refuses with it, the wizard's **Create & install** is
`primary_button_enabled(…, blocked.is_none())` with the reason on hover.
`preferred_backend()` is why a fresh wizard does not open on WSL when its engine
is missing — the *setting* is left alone, because the engine may be installed a
minute later, but the wizard starts where a machine can actually run.

Two details that look like polish and are not:

- **`wsl -e` runs no login shell.** `$PATH` is what WSL's init built, not what
  `~/.profile` would have made, and `~/.local/bin` is usually only on the
  second. So the install reports the **absolute** path it wrote and the manager
  saves *that* as `wsl_entangled`; a check that only asked `command -v` under
  `sh -lc` would pass and the launch would still fail. `PATH_PROBE` asks with
  `sh -c` for exactly this reason. When the bare name misses and
  `~/.local/bin/entangled` is there, `probe_with` re-runs *that* path and
  reports it as the engine, and `ManagerApp::adopt_wsl_engine_path` stores it
  when the setting is empty — otherwise an engine installed by the Windows
  installer (nobody is there afterwards to paste a Linux path into Settings)
  would read green and still not start a machine.
- **A loader complaint says "not found" and means the opposite.**
  `libc.so.6: version 'GLIBC_2.38' not found` is a *present* engine on a
  distribution older than the build; classifying it as missing would have the
  manager offer to install the binary that just refused to load. Hence the
  release build runs on `ubuntu-22.04`, for the oldest glibc it can reach.

#### What the download trusts

The asset is `entangled-linux-x86_64`, published by the release workflow's
`linux-engine` job, which runs **before** the Windows job and hands it the
SHA-256; `build.rs` compiles that digest into the manager as
`ENTANGLED_LINUX_ENGINE_SHA256`. So a manager verifies its download against a
digest fixed when it was built, describing the binary its own release published
— nobody types it and nothing fetches it. It is *not* a signature (whoever can
change the workflow can change what is hashed), which is the same honest ceiling
`apps/entangled/src/bootstrap.rs` documents for the guest kernel. A build the
pipeline did not make carries no digest, and then `install_block()` greys the
button out and says so rather than downloading something it cannot check —
which is what every developer build here does, so that is the state you will see
locally.

`apps/entangled/build.rs` stamps the same variable into the CLI, so
`entangled wsl install-engine` has the same anchor and the same refusal. The
Windows installer's optional task runs that command (see
`installer/entangled.iss`) through `ExecAsOriginalUser`, because WSL
distributions are per-Windows-user and an elevated step would otherwise ask the
wrong account's WSL. Its exit codes are the installer's interface and are
pinned by a test in `apps/entangled/src/wsl_engine.rs`; nothing it can do fails
the installation.

## Theme tokens (GUI-1606, "Deep Field" since 2026-10-15)

All in `theme.rs`; views never invent a colour. Graphite-navy ground,
hairline borders, one ice accent; status colours carry meaning and nothing
else glows. The cyan→violet ramp, `accent(t)` and `gradient_rect` are gone —
do not bring a gradient back, it is what made the old look loud.

| Token | Value | Use |
|---|---|---|
| `BG_DEEP` | `#0a0d12` | page background, `clear_color` |
| `BG_PANEL` | `#0d1117` | sidebar, activity pane |
| `MODAL` | `#0f141b` | dialogs, the editor drawer |
| `CARD` / `CARD_HOVER` | `#11161e` / `#151b24` | card surface; hover and the secondary button body |
| `SELECTED` | `#161d27` | the current sidebar row, a picked chip |
| `INSET` | `#070a0e` | console, text fields, the screen well |
| `STROKE` / `STROKE_STRONG` / `DIVIDER` | `#1f2733` / `#2c3645` / `#1a212b` | borders / buttons, hover / rules between panels |
| `TEXT` / `TEXT_DIM` / `TEXT_FAINT` | `#e4e8ee` / `#98a2b3` / `#7d8695` | body / caption / metadata |
| `ACCENT` / `ACCENT_DEEP` | `#8cc8e0` / `#5aa9cc` | selection, focus, meters, links |
| `PRIMARY` | `#dce6ef` | the primary button's fill (label in `BG_DEEP`) |
| `OK` / `WARN` / `ERR` / `IDLE` | `#4fc38d` / `#e2a74f` / `#e7707a` / `#6b7586` | Running / Installing, warnings / errors / Stopped |
| `CARD_RADIUS` / `CONTROL_RADIUS` | 10 / 6 | corners |

- Status colours: Running `OK`, Installing `WARN`, Suspended `ACCENT`,
  Suspending `ACCENT_DEEP`, Stopping `TEXT_DIM`, Stopped `IDLE` (a ring, not a
  dot). `status_labels_and_colors_are_distinct` holds them apart.
- `every_text_colour_reads_on_every_surface` pins 4.5:1 for all three text
  colours on every surface. `TEXT_FAINT` was nudged up once to pass it on
  `CARD_HOVER`; change a surface and that test tells you.
- Buttons (`ui/mod.rs`): `primary_button` — at most **one** per view, the
  brightest thing on it; `secondary_button` / `ghost_button(…, tint)` — the
  quiet body, tinted only for Stop/Delete; `text_button` — Cancel and other
  least-important actions; `icon_button(Icon, hover)` — square, the hover is
  required because it has no label. All are `BUTTON_H` (34) tall.
- Icons are `ui::Icon` + `ui::paint_icon`: painter strokes on a 16-unit grid,
  no image assets. egui's bundled font has no `✕` or `→` — add an `Icon`.
- Motion goes through `theme::animate_bool` / `theme::animation_time`, and
  there is little of it: 100–120 ms fill/border fades on hover, a slow breath
  on Running's dot, a blinking dot on Stopping, a turning arc
  (`logo::paint_spinner`) for Installing and Suspending. The backdrop is nine
  static one-pixel stars — no grid, no scan line, no node field. The persisted
  **Interface motion** setting disables the rest. Enabled mode repaints every
  40 ms while the window is focused; an unfocused or motion-disabled window
  uses a one-second heartbeat for stats/toast expiry and otherwise remains
  event driven. Do not bypass these theme helpers with direct egui animations.
- `logo.rs` owns the mark — the **linked pair**: two rings, the left one over
  at the top crossing and the right one over at the bottom, each over-arc drawn
  on a knockout in the *background colour*, which is why `paint_mark` takes
  one. Strokes thicken below 96 px so the gap reads at 16 px. `app_icon()`
  rasterises the same geometry (4×4 supersampled, on a rounded `CARD` tile)
  once at startup, avoiding platform-specific bitmap assets.
- The **file** icon is a separate thing from the window's: Explorer, a pinned
  taskbar button and the installer read the icon resource inside the exe.
  `installer/entangled.ico` holds it (16–256 px, drawn by
  `scripts/make-icon.ps1` from the same geometry — rerun it when the mark
  changes), and `apps/manager/build.rs` compiles it with the SDK's `rc.exe`
  and links the `.res` into `entangled-manager.exe` (no crate; without the
  SDK the build warns and keeps the generic icon). Each over-arc is drawn a few
  degrees longer than its knockout, in the script and in `paint_mark`: ends
  that meet on one pixel leave an anti-aliasing seam across the ring.
- Do **not** set `visuals.override_text_color`: it repaints hint text and
  disabled labels at full strength, which once made the delete dialog's
  placeholder look like typed input. Body colour comes from
  `widgets.noninteractive.fg_stroke`.

## Information architecture (2026-10-15)

The sidebar (`ui/sidebar.rs`) lists **the machines themselves**, each with a
status dot and a few words (`ui::status_line`: "Running · 2 h", "Saved 3 hours
ago", "Off"), under "All machines". Host-wide tools sit at its foot — Storage,
Health, Activity, Settings — with one line of host numbers, the version and the
`MOCK DATA` chip. There is no top header.

- **All machines** (`ui/cards.rs`): running machines large (screen well, live
  meters, Suspend · Shut down), everything else as a fixed-height tile with at
  most one action (Start or Resume), a dashed New-machine tile, and the
  first-launch page (`welcome`) when there are no machines at all.
- **The machine page** (`ui/machine.rs`, `View::Machine(name)`) owns every
  lifecycle button: the state's buttons in the header, everything rarer under
  the `…` menu (Restart, Force stop, autostart, rename, clone,
  delete), notices for a missing disk / a shared name / a saved session that
  cannot resume, the screen with "Right now" and "Connect" panels, and setup
  tiles. A tile opens the editor on its own section through
  `Action::AskEditVmAt(name, section)`; tiles are disabled (with the reason on
  hover) unless the machine is Stopped or Suspended.
- **The editor is a drawer** (`dialogs::edit_vm_drawer`): a `Modal` whose area
  is anchored `RIGHT_TOP`, full window height, section tabs (`ui::page_tab`)
  along the top, a fixed footer with Save. Processor & memory opens with the
  Light / Balanced / Performance presets (`POWER_PRESETS`, shared with the
  wizard) above the sliders.
- **The wizard** is three steps — System, Name & size, Review. Name & size is
  the presets, disk-size chips and a "More options" toggle
  (`WizardState::more`) that reveals every manual setting: exact memory and
  cores, unattended / headless install, new vs existing disk.
- **Storage** is Disks and Saved sessions behind one sidebar item, switched by
  the tabs in `ui::storage_header`. **Health** is the old Diagnostics view.
- A machine page whose name has gone (deleted, renamed, moved) says so with a
  way back rather than falling over; a pending install has its own page until
  its profile exists.

## Dependency notes

- egui/eframe are pinned to the **0.32** series: the last one whose MSRV
  (1.85) matches `workspace.package.rust-version`. 0.33 needs 1.88, 0.36 needs
  1.95. Bump both together with the workspace MSRV, never separately.
- eframe 0.32 links wgpu **25** while `crates/display` uses wgpu **26**, so two
  wgpu majors coexist. Two consequences, both encoded in Cargo.toml:
  - `naga = { version = "26", features = ["termcolor"] }` in `apps/manager`
    aligns naga 26's feature set with the `codespan-reporting` build naga 25
    forces; without it naga 26 does not compile.
  - `wgpu = { version = "25", … }` selects the backend set, because eframe's
    `wgpu` feature pulls wgpu in with no backend at all.
  Both lines disappear once one wgpu major serves the whole workspace.
- `deny.toml` carries scoped exceptions for egui's bundled fonts (OFL-1.1,
  Ubuntu-font-1.0 — font *data*, no obligation on our code), the Windows
  clipboard backend (BSL-1.0) and the unmaintained-advisory on `ttf-parser`.

## Running it

```bash
# Build both binaries into one directory so the manager finds the CLI.
wsl -d Ubuntu -e bash -c 'source $HOME/.cargo/env && \
  export CARGO_TARGET_DIR=$HOME/entangled-target-gui && \
  cd /mnt/d/entangled-desktop && cargo build --workspace'

# WSLg shows the window on the Windows desktop. Run it from a directory that
# holds artifacts/bootstrap/ — profiles written by the installer use relative
# kernel paths, and children inherit this working directory.
wsl -d Ubuntu -e bash -c 'cd /mnt/d/entangled-desktop && \
  export DISPLAY=:0 WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 && \
  $HOME/entangled-target-gui/debug/entangled-manager'
```

- `--vm-dir <dir>` / `--entangled <path>` override the saved settings for one
  run; the Settings panel persists them.
- `--mock` starts the complete UI with deterministic in-memory machines,
  disks, statuses and host metrics. It does not load saved settings, scan a VM
  directory, start the metrics worker or invoke `entangled`; mutating buttons
  only update the fixture or show a toast. A `MOCK DATA` chip stays visible at
  the foot of the sidebar so screenshots cannot be confused with a real host. Use it for
  ordinary button/layout work instead of preparing profiles or launching a VM:

  ```powershell
  $env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\entangled-target-gui"
  cargo run -p entangled-manager -- --mock
  ```

- Combine mock mode with the screenshot surfaces for unattended visual QA:

  ```powershell
  cargo run -p entangled-manager -- --mock --screenshot .\manager.png --screenshot-view main
  cargo run -p entangled-manager -- --mock --screenshot .\editor.png --screenshot-view editor
  ```

- `--screenshot <png> [--screenshot-view <surface>]` renders a few frames, saves
  a PNG through `ViewportCommand::Screenshot` and exits — the quickest way to
  review a visual change without a human at the keyboard. Surfaces:
  `main`, `machine` (the running machine's page), `wizard`, `wizard-wsl`
  (the system step with WSL chosen and no
  Linux engine), `wizard-wsl-review` (the same machine on the review step,
  where Create is unavailable), `settings`, `settings-advanced`, `disks`, `snapshots`,
  `snapshot-delete`, `snapshot-discard`, `diagnostics`, and one per editor
  section: `editor` (Hardware), `editor-boot`, `editor-display` (Display &
  GPU), `editor-network` (Network & devices), `editor-storage`. **Every new
  surface gets one**, or it cannot be reviewed.
- Alignment regressions do not survive a look at the pixels but do survive a
  glance at the window. When touching form layout, measure: crop the PNG and
  compare the right-hand edge of each field (PIL is available on this machine).
- The shared form help has an egui input regression test for immediate click-to-open and Escape-to-close. Other UI flows use visual QA. To exercise it end to end, drive the
  real window from the Windows side: `EnumWindows` for
  `"Entangled Desktop*"` (the title carries the injected version, e.g.
  "Entangled Desktop v0.2.17"), `SetCursorPos`+`mouse_event` for clicks at fractions
  of the client rect, `CopyFromScreen` for shots (call `SetProcessDPIAware`
  first, or you capture a scaled crop).
- Only one VM can hold the TAP. A second Start fails fast; `diagnose::explain`
  turns that into "the TAP interface is already held by another VM …". When
  testing against the shared `debian-demo` profile, check
  `pgrep -af "[e]ntangled run"` first — two VMs on one disk would corrupt it.

## Suspend and Restart: the control channel (ADR-0005/0006)

Running cards and the machine page have a primary **Suspend** action beside
**Shut down**. **Restart** and **Force stop** remain in the `…` menu. Pause and
Continue have no product buttons, manager actions or Ctrl+Alt+P shortcut.

`launcher::run_spec` starts `entangled run --control-stdin <profile>` and
`TaskSpec::control` retains its stdin pipe. `Task::suspend`, `Task::reset` and
`Task::shutdown` send `save`, `reset` and `shutdown`. The pipe works on both
hosts and dies with the child. Controls are disabled for machines this manager
did not start, with an explanation on hover. The engine retains diagnostic
pause/resume commands and its internal quiesce barrier for snapshot correctness;
removing a product option must not remove that mechanism.

**Port forwards** (2026-10-12) live under Network & devices for a usernet
machine: one text field per `[network] forward` rule, a Remove button each,
"+ Add a forward", and under every rule either what it means ("127.0.0.1:2222
on this computer reaches port 22 in the guest", from
`control_api::PortForwardRule::parse`) or why it does not parse. A TAP machine
writes no forwards (control-api refuses them there). egui's bundled font has
no `✕` or `→`: write words.

**Shut down** (2026-10-12) is the first button for a running machine:
`Task::shutdown` writes `shutdown` (`control_api::control::CMD_SHUTDOWN`), the
engine presses the guest's ACPI power button, and the machine turns Stopped when
the process ends — there is no state to set, because the guest decides (a
desktop may show its own dialog). Stop stays beside it, labelled for what it is:
pulling the plug.

`Task::send_control` drops the pipe on a write error, so a child that exited
between the click and the write turns the buttons off instead of retrying into a
broken pipe.

## Suspend, Resume and the Snapshots view (ADR-0006)

Suspend rides the same pipe as Restart — `Task::suspend` writes one
`save` line — and Resume is a different child command, `entangled resume
--control-stdin <file>` (`launcher::resume_spec`), with the same `TaskKind::Run`
and the same log, because "resumed" and "started" are one event in a machine's
history. Four rules earned by building it:

- **The vocabulary is `control_api::control`, never a literal.** Two crates sit
  at the ends of that pipe. A prefix spelled twice drifts, and the failure is
  silent: the Suspend button spins until the child exits and then says the wrong
  thing. `parse_reply` / `save_outcome` are the reader.
- **Only a successful suspend stops the VM.** A refusal keeps the session and
  restores its previous pause state. `collect_task_results` consumes failures
  from active tasks with `take_suspend_failure`, clears the spinner and permits
  a retry; waiting for child exit would leave the UI stuck. `LogBuffer::push_line`
  captures replies as they pass, including saves requested from the VM window.
- **`Suspended` is a fact on disk, not a process.** `status_of` returns it for a
  machine with no child *and* a `<name>.esnap` beside its profile, which is why
  a card still reads correctly after the manager restarts. Only that
  conventional path counts — a hand-made copy is a row in the Snapshots view,
  not something a card offers to resume.
- **Refusals are computed before the button is drawn**, in the scan worker:
  `vm_snapshot::inspect` is a file open, and `snapshots::verdict` turns it into
  plain sentences. Two things the engine cannot decide for the manager: the host
  check follows the machine's **backend** (a Linux snapshot is right for a
  Windows manager whose machine runs under WSL), and a path recorded by the
  other engine (`/mnt/d/...` seen from Windows) is left to that engine rather
  than reported as a disk that has vanished — see `snapshots::nameable_here`.

### Clone (`disk_image::clone`, 2026-10-12)

*More → Clone…* is `Action::AskCloneVm` → `Modal::CloneVm`, and the copy runs on a
`vm-clone` worker exactly like the move (`CloneEvent`, whole-percent progress,
`CloseModal` refused while it runs). All the policy is in
`disk_image::clone_machine`, not here, and it is unit-tested there:

- writable disks are copied through `relocate::copy_verified` (sparse plus
  digest re-read), read-only disks and the CD-ROM are shared;
- `boot.nvram` follows its disk's new name, and a named store not yet written
  gets a fresh path, never the source's;
- new MAC (`new_machine_mac`), `forward` and `interface` cleared, all paths in
  the new profile written absolute, and the profile created `create_new` so a
  race never overwrites one;
- any failure removes every file the call created.

The backend is manager state, so the manager copies the source's backend to
the clone after it succeeds (`set_backend_for`). `suggest_clone_name` proposes
`ubuntu-3` for `ubuntu-2`, not `ubuntu-2-2`.

### Rename, and the name as the key (`disk_image::rename`, 2026-10-12)

Everything in this crate is keyed by the machine's **name** (`status_of`, the
supervisor, `<name>-run.log`, the snapshot, the card picture, `vm_backends`),
not by its profile path. Two profiles with the same `name =` therefore share
all of it, which happens when profiles are copied by hand. `name_count` makes
the machine page say so, and *… → Rename…* is the fix. `Action::AskRenameVm`
carries the **profile path**, not the name, because the page's profile is the one to
rename and its name is ambiguous by definition. The dialog suggests the
profile's file stem.

`disk_image::rename_machine` renames the profile, plus every writable disk whose
file name starts with the old name, plus that disk's `.nvram` sidecar.
Exception: a disk another profile references
(`refs::find_references`) **stays put**. It is reported in `shared_disks`, and
the manager toasts it. All the renames are in one directory, and they are
undone on failure. The MAC is kept. The new profile is written before the old
one is removed. The manager refuses the rename while the machine is busy or
has a snapshot (ADR-0006 fingerprints the disk paths). Afterwards it moves the
backend setting, the logs and the picture, but only when no duplicate keeps
the old name.

### Autostart (`Settings::autostart`, 2026-10-13)

The machines to start when the manager opens are a set of names in
`manager.toml`. They are manager state, like `vm_backends`, never profile
state. `ManagerApp::autostart` runs once, after the **first** scan:

- a Stopped machine is started (`start`);
- a Suspended one is resumed (`resume_vm`);
- anything else is left alone.

It never runs under `--mock` or `--screenshot`. Rename carries the entry to
the new name, and delete removes it.

### The screen on the card (`thumbs.rs`, 2026-10-12)

Every `thumbs::ASK_EVERY` (3 s) `refresh_thumbnails` sends `thumbnail` to each
Running machine that has a control channel. The engine writes
`control_api::control::thumbnail_path(<profile dir>, <name>)` (a 480-pixel PNG,
written to a temporary file and renamed into place) and **answers nothing on
success**, so the run log does not grow by one line every 3 s. The manager never
sends a path: under WSL the engine sees the directory under another name, and
both sides derive the same file from the profile. `Thumbs::refresh` stats the
files once a second and decodes only those whose mtime changed.
`cards::screen_well` draws the picture covering its well (cropped, never
letterboxed, so every tile in a row has one shape), dimmed when the machine is
not running, and a quiet placeholder with the state in words when there is no
picture. Tiles, running rows and the machine page all use it. `plan_delete`
removes the file. `--mock` has no pictures, so screenshots stay identical.

### Tiles do not grow

All machines tiles are a fixed height because they carry at most one action;
everything else a machine can do is on its page. The one row that still has to
measure itself is a Snapshots row, whose refusal is a whole sentence that
wraps: it lays its body out once in an
`egui::UiBuilder::new().sizing_pass().invisible()` child and uses the result.
Declaring a line count per state was tried first and was wrong the day a state
was added — silently, because only a screenshot shows it. When you add a button
or a sentence to any fixed-size surface, take the screenshot and check the
pixels.

## First launch and form help (2026-10-24)

The scanner prepares the VM directory with `discovery::prepare_vm_directory`
on its worker before scanning, so a fresh profile opens on the welcome screen.
The Windows installer calls `entangled-manager --initialize-user-data` without
a window as the original user, even in silent setup or with Launch unchecked.
Both paths preserve existing files and report real creation errors. Setup also
sends `SHCNE_ASSOCCHANGED` to refresh Windows' cached taskbar icon after replacing
the EXE/ICO at the same paths; do not delete icon caches or restart Explorer.

`form_label` keeps the label and question mark as one painted interaction target.
Hover shows a tooltip; clicking opens the same help immediately in a popup,
closed by Escape or clicking away. Do not add a nested Label that steals input.
Path fields and their Browse buttons share `BUTTON_H` (34 px). Allocate the
button through the parent layout: a 22 px placeholder with a 34 px child painted
outside it caused both vertical misalignment and drifting rows. Call
`form_scope` again inside an indented/collapsing form such as Settings Advanced.


## Firmware and mouse preferences follow-up (2026-10-25)

Run preflight uses `launcher::locate_vm_firmware(profile, cwd, engine)`: honour
an existing firmware declared by this VM first, then the selected engine's
installation, then the usual host lookup. Do not warn from the manager's own
installation alone: a VM with a correct absolute firmware path was starting
successfully while receiving a false warning. Native host firmware checks are
not applied to WSL launches. Missing-file notices are brief user-facing text;
paths, download commands and digest details belong in diagnostic output.

Display & GPU has **Optimise mouse for games** (`EditForm::gaming_mouse`). It
loads from `[display] relative_mouse == Some(true)`. On writes Some(true);
off writes None unless preserving an existing explicit Some(false). This avoids
removing the automatic 3D PCI mouse just by changing the capture preference.
The choice applies at next start. Ctrl+Alt+M no longer changes it or reserves
that chord from the guest. Keep both skill copies valid UTF-8 and equivalent.
