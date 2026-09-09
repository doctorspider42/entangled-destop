---
name: gui-manager
description: The native desktop manager (egui/eframe on wgpu) — quantum theme tokens, view architecture, child-process supervision of the entangled CLI, and how to run it under WSLg (backlog EPIC 16, crate apps/manager). Load before touching the GUI.
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
  ui/        views: top bar + side navigation, cards, guided wizard/dialogs,
             Disks, Snapshots, Diagnostics, activity pane and toasts
  theme.rs   every colour, radius and font size in the product
  logo.rs    procedural mark + install spinner (egui painter, no assets)
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

## Diagnostics (`hostcheck.rs` + `ui/diagnostics.rs`)

The panel is `entangled doctor`, run on a worker thread and rendered: `MISSING`
lines in the warning colour, the instruction that follows each one in the quiet
one. It does **not** reimplement the checks — `doctor` already knows them per
host, and two copies would drift. It also carries the engine card and the
backend/capability summary. `--mock` shows `hostcheck::mock_report()` (a healthy
host with one missing artifact) so screenshots are identical everywhere.

## Install flow (GUI-1602)

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
[--auto] [--headless]` is spawned with the configured working directory.
`--variant` is Debian-only — `install ubuntu` ignores the flag, and a setting
that does nothing on the reviewed command line is worse than an absent one.
`--network` is never passed either: the CLI's per-host default (TAP on Linux,
usernet on Windows) is right on both hosts and a pinned GUI value would be wrong
on one. The CLI writes
the profile itself, but hardcodes its resource defaults, so on success the
manager stamps the wizard's memory/vCPU choice onto the profile
(`discovery::apply_resources`). While the install runs the VM has no profile
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
- The control pipe survives `wsl.exe`, so Pause and Restart still work. The
  window does not come up on the Windows desktop — WSLg opens its own.
- **Capabilities follow the kernel, not the host.** `Backend::virgl_block`,
  `tap_block` and `debian_install_block` are the single source of truth for
  "this fails at boot"; the editor and the wizard grey the control out and show
  `.short`, and the same check runs again at submit time in case the setting
  changed underneath the form.

## Theme tokens (GUI-1606)

All in `theme.rs`; views never invent a colour.

| Token | Value | Use |
|---|---|---|
| `BG_DEEP` | `#080b16` | page background, `clear_color` |
| `BG_PANEL` | `#0b1021` | header, log pane, modals |
| `CARD` / `CARD_HOVER` | `#11182d` / `#17213d` | card surface, hover target |
| `INSET` | `#060912` | console, text fields, code blocks |
| `STROKE` / `STROKE_STRONG` | `#1d2a48` / `#2b3d63` | borders |
| `TEXT` / `TEXT_DIM` / `TEXT_FAINT` | `#dde6f7` / `#8b9cbd` / `#5d6b8a` | body / caption / metadata |
| `CYAN` … `VIOLET` | `#35e2f0` … `#a86bff` | the entanglement ramp, `accent(t)` |
| `OK` / `WARN` / `ERR` | `#3ddc97` / `#ffb454` / `#ff5d73` | Running / Stopping / errors |
| `CARD_RADIUS` / `CONTROL_RADIUS` | 14 / 9 | corners |

- `accent(t)` positions anything on the cyan→violet ramp; each card picks a
  stable `t` from its name so a grid does not look uniform.
- `gradient_rect` is a two-triangle `Mesh` — egui has no gradient brush. Used
  for the header hairline, card top edge and modal title rule (square corners
  only; rounded gradients would need clipping).
- Motion goes through `theme::animate_bool` / `theme::animation_time`: card
  hover (180 ms fill/border lift), button hover (140 ms glow + accent slide),
  the subtle grid scan and eight-node background field, a breathing halo on
  Running, a blinking dot on Stopping, and the entangled-particle spinner while installing
  (`logo::paint_particle_spinner`). The persisted **Interface motion** setting
  disables all of it. Enabled mode repaints every 40 ms while the window is
  focused; an unfocused or motion-disabled window uses a one-second heartbeat
  for stats/toast expiry and otherwise remains event driven. Do not bypass
  these theme helpers with direct egui animations.
- `logo.rs` owns both the painter mark and `app_icon()`. The latter rasterises
  the same two-loop geometry once at startup for the native window/taskbar
  icon, avoiding platform-specific bitmap assets.
- Do **not** set `visuals.override_text_color`: it repaints hint text and
  disabled labels at full strength, which once made the delete dialog's
  placeholder look like typed input. Body colour comes from
  `widgets.noninteractive.fg_stroke`.

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
  only update the fixture or show a toast. A `MOCK DATA` chip stays visible in
  the header so screenshots cannot be confused with a real host. Use it for
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
  `main`, `wizard`, `settings`, `disks`, `snapshots`, `snapshot-delete`,
  `snapshot-discard`, `diagnostics`, and one per editor section: `editor`
  (Hardware), `editor-boot`, `editor-network`, `editor-storage`. **Every new
  surface gets one**, or it cannot be reviewed.
- Alignment regressions do not survive a look at the pixels but do survive a
  glance at the window. When touching form layout, measure: crop the PNG and
  compare the right-hand edge of each field (PIL is available on this machine).
- The UI itself has no automated tests. To exercise it end to end, drive the
  real window from the Windows side: `EnumWindows` for
  `"Entangled Desktop*"` (the title carries the injected version, e.g.
  "Entangled Desktop v0.2.17"), `SetCursorPos`+`mouse_event` for clicks at fractions
  of the client rect, `CopyFromScreen` for shots (call `SetProcessDPIAware`
  first, or you capture a scaled crop).
- Only one VM can hold the TAP. A second Start fails fast; `diagnose::explain`
  turns that into "the TAP interface is already held by another VM …". When
  testing against the shared `debian-demo` profile, check
  `pgrep -af "[e]ntangled run"` first — two VMs on one disk would corrupt it.

## Pause and Restart: the control channel (ADR-0005)

A running VM's card has **Pause/Resume** and **Restart** beside **Stop**. They
are not a stop and a start: they reach the live VM.

The mechanism is the child's stdin. `launcher::run_spec` now spawns
`entangled run --control-stdin <profile>` and `TaskSpec::control` makes the
supervisor keep stdin as a pipe instead of `Stdio::null()`; `Task::set_paused`
and `Task::reset` write `pause` / `resume` / `reset` lines into it. A pipe rather
than a socket because it is the one channel that exists identically on both
hosts, needs no path, no permissions and no cleanup, and dies with the process it
controls — which for "pause this VM" is exactly the lifetime wanted.

Two honesty rules the UI follows, and should keep following:

- **The buttons are disabled for a VM this manager did not start.** A control
  channel is a pipe to a child; a VM launched from a terminal has none, and the
  hover text says so rather than the button lying.
- **`pause_requested` is a request, not a state.** The VM can also be frozen
  from its own window with `Ctrl+Alt+P`, and the manager would not know. What the
  flag drives is the button's label, which is all it can honestly claim. The
  VM's own answer is one `status` command away for anything that needs the truth.

`Task::send_control` drops the pipe on a write error, so a child that exited
between the click and the write turns the buttons off instead of retrying into a
broken pipe.

## Suspend, Resume and the Snapshots view (ADR-0006)

Suspend rides the same pipe as Pause and Restart — `Task::suspend` writes one
`save` line — and Resume is a different child command, `entangled resume
--control-stdin <file>` (`launcher::resume_spec`), with the same `TaskKind::Run`
and the same log, because "resumed" and "started" are one event in a machine's
history. Four rules earned by building it:

- **The vocabulary is `control_api::control`, never a literal.** Two crates sit
  at the ends of that pipe. A prefix spelled twice drifts, and the failure is
  silent: the Suspend button spins until the child exits and then says the wrong
  thing. `parse_reply` / `save_outcome` are the reader.
- **A failed suspend still exits 0.** The engine stops the VM either way, so the
  exit status cannot tell success from a snapshot that was never written — the
  reply line is the only answer, and `LogBuffer::push_line` lifts it out **as it
  goes past**. Scanning the tail afterwards loses it: a desktop guest can push
  the whole log buffer through in the seconds a suspend takes.
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

### Cards that grow

`horizontal_wrapped` only reports how many lines it took **after** it has drawn
them, and a card's action strip is bottom-anchored, so a wrapped second line
grows straight through the border. Both places that hit this now measure instead
of declaring: `cards::measured_action_height` and the Snapshots row lay their
content out once in an `egui::UiBuilder::new().sizing_pass().invisible()` child
and use the resulting height. Declaring a line count per state was the first
attempt and it was wrong the day a state was added — silently, because only a
screenshot shows it. When you add a button or a sentence to a card, take the
screenshot and check the pixels; `CARD_HEIGHT` is still a fixed floor and the
tallest state (Suspended: two chip rows, a "saved 3 hours ago" line and two
button lines) is what sets it.

Related: a suspended machine's extra line is **always exactly one**. When the
snapshot cannot go back it reads "saved 3 hours ago · 1.1 GiB · cannot resume
here" with the full reason on the hover, rather than adding a second line the
card has no room for.
