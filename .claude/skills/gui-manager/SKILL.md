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
main.rs      CLI flags (--vm-dir, --entangled, --screenshot[-view]) + tracing
app.rs       ManagerApp (eframe::App): state, Action loop, frame layout
  ui/        views: top bar + side navigation, cards, guided wizard/dialogs,
             activity pane and toasts
  theme.rs   every colour, radius and font size in the product
  logo.rs    procedural mark + install spinner (egui painter, no assets)
settings.rs  ~/.config/entangled/manager.toml, typed load/save
update.rs    startup update check (GitHub /releases/latest) + installer
             download/launch, both on background threads; banner in cards.rs
discovery.rs VM directory scan through control-api, delete plan + guards
launcher.rs  locating the CLI, building install/run argument vectors
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

### Install flow (GUI-1602)

The four-stage wizard collects system/media, name/resources, storage and a
review before it launches anything. It supports Debian's verified variants and
Ubuntu through the verified cache or an explicit local `--iso`. Storage is an
explicit choice between a new sparse RAW image and an existing image in the VM
directory; the latter is never recreated or truncated.

The resulting `entangled install <debian|ubuntu> --disk <path> --size <n>G
--variant <v> --memory-mib <max(1536, m)> --name <name> [--iso <path>] [--auto]
[--headless]` is spawned with the configured working directory. The CLI writes
the profile itself, but hardcodes its resource defaults, so on success the
manager stamps the wizard's memory/vCPU choice onto the profile
(`discovery::apply_resources`). While the install runs the VM has no profile
yet, so `PendingInstall` gives it a card with the Installing badge.

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
  the subtle grid scan, a breathing halo on Running, a blinking dot on
  Stopping, and the entangled-particle spinner while installing
  (`logo::paint_particle_spinner`). The persisted **Interface motion** setting
  disables all of it. Enabled mode repaints every 40 ms; disabled mode uses a
  one-second heartbeat for stats/toast expiry and otherwise remains event
  driven. Do not bypass these theme helpers with direct egui animations.
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
- `--screenshot <png> [--screenshot-view main|wizard|settings]` renders a few
  frames, saves a PNG through `ViewportCommand::Screenshot` and exits — the
  quickest way to review a visual change without a human at the keyboard.
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
