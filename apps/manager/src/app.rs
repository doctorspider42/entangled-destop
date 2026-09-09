//! The eframe application: state, the action loop and the frame layout.
//!
//! Rules of the house:
//! - the frame loop never blocks — scans run on a worker thread, children are
//!   supervised by [`crate::process`];
//! - every failure becomes a toast or a banner, never a panic;
//! - UI code produces [`Action`]s, `ManagerApp::apply` is the only place that
//!   mutates state or touches the filesystem.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::backend::{self, Backend};
use crate::discovery::{self, Scan, VmEntry};
use crate::hostcheck;
use crate::launcher::{self, Engine, NewMachine, Runner};
use crate::metrics;
use crate::picker::{self, PickTarget};
use crate::process::{Supervisor, TaskId, TaskKind};
use crate::settings::{self, Settings};
use crate::snapshots::{self, SnapshotRow, Verdict};
use crate::theme;
use crate::ui;
use crate::update::{self, UpdateInfo};
use crate::wslengine;
use crate::ScreenshotView;

use control_api::wsl::EngineFault;

/// How often the VM directory is re-scanned while the window is open.
const SCAN_INTERVAL: Duration = Duration::from_millis(2500);
/// Animation clock cadence when ambient motion is enabled.
const FRAME_INTERVAL: Duration = Duration::from_millis(40);

pub struct Startup {
    pub vm_dir: Option<PathBuf>,
    pub entangled: Option<PathBuf>,
    pub screenshot: Option<PathBuf>,
    pub screenshot_view: ScreenshotView,
    pub mock: bool,
}

/// Window title: the product name with the injected build version, e.g.
/// "Entangled Desktop v0.2.17".
pub fn window_title() -> String {
    format!("Entangled Desktop v{}", crate::VERSION)
}

pub fn launch(startup: Startup) -> Result<(), String> {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([880.0, 560.0])
            .with_title(window_title())
            .with_app_id("entangled-manager")
            .with_icon(crate::logo::app_icon()),
        ..Default::default()
    };

    // Debug builds of wgpu validate indirect draw calls with a compute shader
    // that software GL stacks (llvmpipe under WSLg, plain GLES) cannot compile —
    // the device is then lost before the first frame. egui issues no indirect
    // draws at all, so the check is dropped rather than worked around with an
    // environment variable.
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup
            .instance_descriptor
            .flags
            .remove(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
    }

    // The renderer is wgpu: the only backend feature this crate enables, and
    // the same stack `crates/display` already uses.
    eframe::run_native(
        "entangled-manager",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(ManagerApp::new(&cc.egui_ctx, startup)))
        }),
    )
    .map_err(|e| format!("cannot start the manager window: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Success,
    Warn,
    Error,
}

pub struct Toast {
    pub text: String,
    pub level: ToastLevel,
    pub born: f64,
}

impl Toast {
    pub fn lifetime(&self) -> f64 {
        match self.level {
            ToastLevel::Error => 14.0,
            ToastLevel::Warn => 10.0,
            _ => 6.0,
        }
    }
}

/// Which main surface the central panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Machines,
    Disks,
    /// Saved sessions: every `*.esnap` in the VM directory (ADR-0006).
    Snapshots,
    /// Host readiness: `entangled doctor`, rendered.
    Diagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshState {
    Idle,
    Scanning,
    Complete,
}

/// Wizard state (GUI-1602).
pub struct WizardState {
    pub machine: NewMachine,
    pub step: usize,
    pub error: Option<String>,
}

/// "New disk" dialog state.
pub struct CreateDiskState {
    pub name: String,
    /// Size as typed ("32G", "512M", plain bytes) — validated by the same
    /// `disk-image` parser the CLI uses.
    pub size: String,
    pub error: Option<String>,
}

/// Disk delete confirmation state.
pub struct DeleteDiskState {
    pub row: discovery::DiskRow,
    pub error: Option<String>,
}

/// "Attach to VM" dialog state.
pub struct AttachDiskState {
    pub disk: PathBuf,
    pub selected: Option<String>,
    pub error: Option<String>,
}

/// Edit-VM dialog state: the form plus its inline error.
pub struct EditVmState {
    pub form: crate::editor::EditForm,
    pub section: EditVmSection,
    pub error: Option<String>,
}

/// The editor grows by adding a section here instead of lengthening one giant
/// form. Only the active section is rendered; Save still validates and writes
/// the complete profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditVmSection {
    Hardware,
    BootMedia,
    NetworkDisplay,
    Storage,
}

/// What the move worker thread reports back to the modal.
pub enum MoveEvent {
    /// `(data bytes copied, data bytes total)` — allocated data, not apparent.
    Progress(u64, u64),
    Done(Box<Result<disk_image::MoveOutcome, String>>),
}

/// "Move disk to another drive" dialog state.
pub struct MoveDiskState {
    pub row: discovery::DiskRow,
    /// Destination directory as typed.
    pub dest: String,
    pub error: Option<String>,
    /// `(copied, total)` while the worker runs.
    pub progress: Option<(u64, u64)>,
    pub running: bool,
    pub events: Option<mpsc::Receiver<MoveEvent>>,
}

/// "Grow this disk" dialog state. Shrinking is not offered at all — the CLI
/// refuses it because it would destroy whatever the guest put at the end of the
/// image, and an action that can only fail does not belong in a GUI.
pub struct ResizeDiskState {
    pub row: discovery::DiskRow,
    /// New size as typed, validated by the same parser the CLI uses.
    pub size: String,
    pub error: Option<String>,
}

/// Snapshot delete confirmation state (ADR-0006).
pub struct DeleteSnapshotState {
    pub row: SnapshotRow,
    pub error: Option<String>,
}

/// "Start this machine fresh even though it has a saved session" state.
///
/// A confirmation rather than a warning toast, because the choice is
/// irreversible in the only way that matters: the moment the cold-booted guest
/// writes to its disk, the saved session can no longer go back onto it. Better
/// to throw the file away deliberately than to keep one that will refuse.
pub struct DiscardSnapshotState {
    pub vm: String,
    pub row: SnapshotRow,
    pub error: Option<String>,
}

/// Delete confirmation state (GUI-1604).
pub struct DeleteState {
    pub name: String,
    pub typed: String,
    pub plan: discovery::DeletePlan,
    pub error: Option<String>,
}

pub struct SettingsForm {
    pub vm_dir: String,
    pub entangled_binary: String,
    pub work_dir: String,
    pub headless_install: bool,
    pub check_updates_on_startup: bool,
    pub animations_enabled: bool,
    pub default_backend: Backend,
    pub wsl_distro: String,
    pub wsl_entangled: String,
    /// The Advanced group starts collapsed: everything inside it is something
    /// the application already worked out for itself.
    pub advanced_open: bool,
}

#[allow(clippy::large_enum_variant)]
pub enum Modal {
    None,
    Wizard(WizardState),
    Delete(DeleteState),
    Settings(SettingsForm),
    CreateDisk(CreateDiskState),
    DeleteDisk(DeleteDiskState),
    AttachDisk(AttachDiskState),
    EditVm(EditVmState),
    MoveDisk(MoveDiskState),
    ResizeDisk(ResizeDiskState),
    DeleteSnapshot(DeleteSnapshotState),
    DiscardSnapshot(DiscardSnapshotState),
}

impl Modal {
    pub fn is_open(&self) -> bool {
        !matches!(self, Modal::None)
    }
}

/// What the UI asks the application to do.
pub enum Action {
    Refresh,
    CreateVmDir,
    OpenWizard,
    CloseModal,
    SubmitWizard,
    OpenSettings,
    SaveSettings,
    Start(String),
    Stop(String),
    /// Freeze a running VM, or let a frozen one continue (ADR-0005).
    TogglePause(String),
    /// Reboot a running VM in place — the machine reset, not stop-then-start.
    Reset(String),
    /// Write a running VM to its snapshot file and stop it (ADR-0006).
    Suspend(String),
    /// Start a suspended machine from its own snapshot instead of booting it.
    ResumeVm(String),
    /// The same, for a snapshot file that may belong to no profile here.
    ResumeSnapshot(PathBuf),
    AskDeleteSnapshot(PathBuf),
    ConfirmDeleteSnapshot,
    /// Cold-boot a suspended machine, throwing the saved session away.
    AskDiscardSnapshot(String),
    ConfirmDiscardSnapshot,
    AskDelete(String),
    ConfirmDelete,
    CopyProfilePath(String),
    SelectLog(TaskId),
    ToggleLogPane,
    DismissToast(usize),
    /// Download the new installer and launch it (Windows).
    InstallUpdate,
    /// Open the release page in the browser (hosts without the installer).
    OpenReleasePage,
    /// Hide the update banner for this session.
    DismissUpdate,
    // ---- Disks view -------------------------------------------------------
    SwitchView(View),
    OpenCreateDisk,
    SubmitCreateDisk,
    AskDeleteDisk(PathBuf),
    ConfirmDeleteDisk,
    OpenAttachDisk(PathBuf),
    SubmitAttachDisk,
    DetachDisk {
        vm: String,
        profile: PathBuf,
        declared: PathBuf,
    },
    /// Open the host file manager with the file selected (explorer/xdg-open).
    Reveal(PathBuf),
    // ---- VM editor ---------------------------------------------------------
    AskEditVm(String),
    SubmitEditVm,
    // ---- Move to another drive ----------------------------------------------
    AskMoveDisk(PathBuf),
    SubmitMoveDisk,
    // ---- Paths, chosen rather than typed -------------------------------------
    /// Open a native file/folder dialog for one field (never on this thread).
    PickPath(PickTarget),
    // ---- Diagnostics ----------------------------------------------------------
    /// Run `entangled doctor` on the current backend and show the answer.
    RunDiagnostics,
    /// Re-run the WSL engine pre-flight (the distro, the path, `--version`).
    CheckWslEngine,
    /// Download the pinned Linux engine and copy it into the WSL distribution.
    InstallWslEngine,
    // ---- Storage, the rest of the CLI's disk surface ---------------------------
    AskResizeDisk(PathBuf),
    SubmitResizeDisk,
}

/// A VM being installed that has no profile on disk yet, so it still gets a
/// card (GUI-1601 "Installing" state).
pub struct PendingInstall {
    pub name: String,
    pub machine: NewMachine,
    pub profile_path: PathBuf,
    pub applied: bool,
}

struct Scanner {
    request: mpsc::Sender<ScanRequest>,
    result: mpsc::Receiver<Result<Scan, String>>,
    in_flight: bool,
}

struct ScanRequest {
    vm_dir: PathBuf,
    work_dir: Option<PathBuf>,
}

impl Scanner {
    fn spawn(waker: crate::process::Waker) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<ScanRequest>();
        let (result_tx, result_rx) = mpsc::channel();
        let builder = std::thread::Builder::new().name("vm-scan".to_string());
        let spawned = builder.spawn(move || {
            while let Ok(req) = request_rx.recv() {
                let result = discovery::scan(&req.vm_dir, req.work_dir.as_deref())
                    .map_err(|e| e.to_string());
                if result_tx.send(result).is_err() {
                    return;
                }
                waker();
            }
        });
        if let Err(e) = spawned {
            // Without the worker the UI would show an empty list forever; the
            // error surfaces through the first scan attempt instead.
            tracing::error!(error = %e, "cannot start the scanner thread");
        }
        Self {
            request: request_tx,
            result: result_rx,
            in_flight: false,
        }
    }

    fn disabled() -> Self {
        let (request, _request_rx) = mpsc::channel();
        let (_result_tx, result) = mpsc::channel();
        Self {
            request,
            result,
            in_flight: false,
        }
    }
}

pub struct ManagerApp {
    pub mock_mode: bool,
    pub view: View,
    pub settings: Settings,
    pub settings_path: Option<PathBuf>,
    /// Non-fatal startup problem (unreadable settings file) shown as a banner.
    pub startup_warning: Option<String>,
    /// The resolved engine, or why it could not be found. Reported as status —
    /// a GUI user is never asked to type this path.
    pub engine: Result<Engine, String>,
    engine_version: Option<mpsc::Receiver<Option<String>>>,
    pub scan: Scan,
    pub scan_error: Option<String>,
    mock_statuses: HashMap<String, Status>,
    pub supervisor: Supervisor,
    /// Live host/VM numbers from the metrics sampler thread, refreshed ~1/s.
    pub stats: metrics::Snapshot,
    metrics: metrics::Metrics,
    pub pending: Vec<PendingInstall>,
    pub toasts: Vec<Toast>,
    pub modal: Modal,
    pub log_open: bool,
    pub log_selected: Option<TaskId>,
    pub log_follow: bool,
    /// A newer release the banner offers; `None` means none found (yet) or
    /// dismissed.
    pub update: Option<UpdateInfo>,
    /// True while the installer asset is being downloaded on its thread.
    pub update_downloading: bool,
    update_check: Option<mpsc::Receiver<UpdateInfo>>,
    update_download: Option<mpsc::Receiver<Result<PathBuf, String>>>,
    /// The one open file dialog, if any.
    picker: Option<picker::Pending>,
    /// Is there a usable Linux engine inside WSL? The pre-flight for the WSL
    /// backend, refreshed whenever the distribution or the engine path changes
    /// and never blocking the frame loop.
    pub wsl_engine: wslengine::Status,
    /// What the current answer is *about*; a re-check is only needed when this
    /// changes.
    wsl_engine_target: Option<wslengine::Target>,
    wsl_engine_rx: Option<mpsc::Receiver<Result<control_api::wsl::EngineFound, EngineFault>>>,
    /// True while the pinned Linux engine is being downloaded and copied in.
    pub wsl_install_running: bool,
    wsl_install_rx: Option<mpsc::Receiver<Result<wslengine::Outcome, String>>>,
    /// The last `entangled doctor` answer, and whether one is in flight.
    pub doctor: Option<hostcheck::Report>,
    pub doctor_running: bool,
    doctor_rx: Option<mpsc::Receiver<hostcheck::Report>>,
    waker: crate::process::Waker,
    scanner: Scanner,
    last_scan: Instant,
    manual_refresh_at: Option<Instant>,
    screenshot: Option<ScreenshotJob>,
    screenshot_view: ScreenshotView,
    screenshot_surface_opened: bool,
    frame: u64,
}

struct ScreenshotJob {
    path: PathBuf,
    requested: bool,
}

impl ManagerApp {
    fn new(ctx: &egui::Context, startup: Startup) -> Self {
        let waker: crate::process::Waker = {
            let ctx = ctx.clone();
            Arc::new(move || ctx.request_repaint())
        };

        let mock_mode = startup.mock;
        let settings_path = if mock_mode {
            None
        } else {
            settings::config_path().ok()
        };
        let mut startup_warning = None;
        let mut settings = if mock_mode {
            Settings {
                vm_dir: PathBuf::from("mock-vms"),
                ..Settings::default()
            }
        } else {
            match &settings_path {
                Some(path) => match Settings::load_from(path) {
                    Ok(settings) => settings,
                    Err(e) => {
                        startup_warning = Some(format!("{e} — using defaults"));
                        Settings::default()
                    }
                },
                None => {
                    startup_warning =
                        Some("no configuration directory; settings will not persist".to_string());
                    Settings::default()
                }
            }
        };
        if !mock_mode {
            if let Some(dir) = startup.vm_dir {
                settings.vm_dir = dir;
            }
        }
        if !mock_mode {
            if let Some(cli) = startup.entangled {
                settings.entangled_binary = Some(cli);
            }
        }

        // The startup update check (opt-out via Settings): one background
        // thread, one optional message, silence on any failure. Screenshot
        // runs are development renders — no network there.
        let update_check =
            (!mock_mode && settings.check_updates_on_startup && startup.screenshot.is_none())
                .then(|| {
                    update::Version::parse(crate::VERSION)
                        .map(|current| update::spawn_check(current, Arc::clone(&waker)))
                })
                .flatten();

        let scan = mock_mode.then(crate::mock::scan).unwrap_or_default();
        let stats = mock_mode.then(crate::mock::metrics).unwrap_or_default();
        let metrics = if mock_mode {
            metrics::Metrics::fixed(stats.clone())
        } else {
            metrics::Metrics::spawn(Arc::clone(&waker))
        };
        let scanner = if mock_mode {
            Scanner::disabled()
        } else {
            Scanner::spawn(Arc::clone(&waker))
        };

        // Resolving the engine is the application's job, not the user's: the
        // path, how it was found, and (once the probe answers) its version all
        // become one status line rather than a text field to fill in.
        let engine = if mock_mode {
            Ok(Engine {
                path: PathBuf::from("mock-entangled"),
                origin: launcher::EngineOrigin::BesideManager,
                version: Some(crate::VERSION.to_string()),
            })
        } else {
            launcher::locate_engine(&settings).map_err(|e| e.to_string())
        };
        let engine_version = (!mock_mode)
            .then(|| engine.as_ref().ok())
            .flatten()
            .map(|engine| launcher::spawn_version_probe(engine.path.clone(), Arc::clone(&waker)));

        let mut app = Self {
            mock_mode,
            view: View::default(),
            engine,
            engine_version,
            settings,
            settings_path,
            startup_warning,
            scan,
            scan_error: None,
            mock_statuses: mock_mode.then(crate::mock::statuses).unwrap_or_default(),
            supervisor: Supervisor::new(Arc::clone(&waker)),
            stats,
            metrics,
            pending: Vec::new(),
            toasts: Vec::new(),
            modal: Modal::None,
            log_open: false,
            log_selected: None,
            log_follow: true,
            update: None,
            update_downloading: false,
            update_check,
            update_download: None,
            picker: None,
            // The mock session shows the state this whole pre-flight exists
            // for — a distribution with no engine — so the wizard's refusal and
            // its install button are reviewable in a screenshot on any machine.
            wsl_engine: if mock_mode {
                wslengine::mock_status()
            } else {
                wslengine::Status::Unknown
            },
            wsl_engine_target: None,
            wsl_engine_rx: None,
            wsl_install_running: false,
            wsl_install_rx: None,
            doctor: mock_mode.then(hostcheck::mock_report),
            doctor_running: false,
            doctor_rx: None,
            waker: Arc::clone(&waker),
            scanner,
            last_scan: Instant::now() - SCAN_INTERVAL,
            manual_refresh_at: None,
            screenshot: startup.screenshot.map(|path| ScreenshotJob {
                path,
                requested: false,
            }),
            screenshot_view: startup.screenshot_view,
            screenshot_surface_opened: false,
            frame: 0,
        };

        // The WSL pre-flight starts with the window, not with the wizard's
        // last step: the answer takes seconds (a cold distribution has to boot)
        // and it decides which backend the wizard even opens on.
        app.ensure_wsl_check(false);

        match startup.screenshot_view {
            ScreenshotView::Wizard => app.open_wizard(),
            // The two WSL surfaces open the wizard and then *choose* the
            // backend the pre-flight refuses — `preferred_backend` would not,
            // which is the point of it.
            ScreenshotView::WizardWsl | ScreenshotView::WizardWslReview => {
                app.open_wizard();
                if let Modal::Wizard(state) = &mut app.modal {
                    state.machine.backend = Backend::Wsl;
                    if startup.screenshot_view == ScreenshotView::WizardWslReview {
                        state.step = 3;
                    }
                }
            }
            ScreenshotView::Settings => app.open_settings(),
            ScreenshotView::Disks => app.view = View::Disks,
            ScreenshotView::Snapshots => app.view = View::Snapshots,
            ScreenshotView::Diagnostics => app.view = View::Diagnostics,
            ScreenshotView::SnapshotDelete => {
                app.view = View::Snapshots;
            }
            // The discard confirmation belongs to a card, so it is reviewed
            // over the Machines grid it is opened from.
            ScreenshotView::SnapshotDiscard => app.view = View::Machines,
            ScreenshotView::Main
            | ScreenshotView::Editor
            | ScreenshotView::EditorBoot
            | ScreenshotView::EditorNetwork
            | ScreenshotView::EditorStorage => {}
        }
        app
    }

    pub fn refresh_state(&self) -> RefreshState {
        if self.scanner.in_flight && self.manual_refresh_at.is_some() {
            RefreshState::Scanning
        } else if self
            .manual_refresh_at
            .is_some_and(|started| started.elapsed() < Duration::from_millis(1400))
        {
            RefreshState::Complete
        } else {
            RefreshState::Idle
        }
    }

    pub fn toast(&mut self, level: ToastLevel, text: impl Into<String>) {
        let text = text.into();
        match level {
            ToastLevel::Error => tracing::error!("{text}"),
            ToastLevel::Warn => tracing::warn!("{text}"),
            _ => tracing::info!("{text}"),
        }
        self.toasts.push(Toast {
            text,
            level,
            born: f64::NAN, // stamped on the next frame, where `ctx.time` exists
        });
    }

    /// Status of a VM as the cards draw it.
    ///
    /// Two of the five states are not properties of a process. **Suspending**
    /// is a running child that has been told to write itself to a file and will
    /// exit when it has; **Suspended** is the absence of a child *plus* the
    /// presence of that file. The second one is the reason a suspended machine
    /// still reads correctly after the manager has been closed and reopened:
    /// unlike "running", it is a fact on disk rather than one this process was
    /// holding in memory.
    pub fn status_of(&self, name: &str) -> Status {
        if self
            .supervisor
            .active_task(name)
            .is_some_and(|t| t.suspend_requested())
        {
            return Status::Suspending;
        }
        let base = match self.mock_statuses.get(name) {
            Some(status) => *status,
            None => match self.supervisor.active_kind(name) {
                Some(TaskKind::Run) => {
                    if self
                        .supervisor
                        .active_task(name)
                        .is_some_and(|t| t.stop_requested())
                    {
                        Status::Stopping
                    } else {
                        Status::Running
                    }
                }
                Some(TaskKind::Install) => Status::Installing,
                None => Status::Stopped,
            },
        };
        match base {
            Status::Stopped if self.snapshot_for(name).is_some() => Status::Suspended,
            other => other,
        }
    }

    pub fn is_busy(&self, name: &str) -> bool {
        if self.mock_mode {
            return matches!(
                self.status_of(name),
                Status::Running | Status::Stopping | Status::Installing | Status::Suspending
            );
        }
        self.supervisor.is_busy(name)
    }

    /// This machine's saved session, if it has one.
    ///
    /// Keyed on the *conventional* path — `<name>.esnap` beside the profile,
    /// which is where a bare `save` writes — rather than on the machine name
    /// recorded inside every snapshot in the directory. A copy someone made by
    /// hand is a snapshot of the same machine and belongs in the Snapshots
    /// view, but it is not the session this machine would come back from, and a
    /// card that offered to resume an arbitrary one of several would be
    /// guessing.
    pub fn snapshot_for(&self, name: &str) -> Option<&SnapshotRow> {
        let vm = self.vm(name)?;
        let path = snapshots::path_for(vm);
        self.scan.snapshots.iter().find(|row| row.path == path)
    }

    /// Whether that saved session could actually be resumed, and why not.
    pub fn snapshot_verdict(&self, row: &SnapshotRow) -> Verdict {
        let vm_name = row.vm_name().unwrap_or_default().to_string();
        let profile = self.vm(&vm_name);
        // A snapshot's machine may have no profile here at all, in which case
        // the backend it would run on is the default one — the same answer
        // `Settings::backend_for` gives for an unknown name.
        snapshots::verdict(row, self.backend_of(&vm_name), profile)
    }

    pub fn vm(&self, name: &str) -> Option<&VmEntry> {
        self.scan.vms.iter().find(|vm| vm.name == name)
    }

    fn open_wizard(&mut self) {
        self.ensure_wsl_check(false);
        let name = self.suggest_name();
        self.modal = Modal::Wizard(WizardState {
            machine: NewMachine {
                name,
                memory_mib: self.settings.default_memory_mib,
                vcpus: self.settings.default_vcpus,
                disk_gib: self.settings.default_disk_gib,
                disk_path: String::new(),
                disk_mode: launcher::DiskMode::CreateNew,
                family: launcher::GuestFamily::default_for_host(),
                iso_path: String::new(),
                variant: self.settings.default_variant.clone(),
                automated: true,
                headless: self.settings.headless_install,
                backend: self.preferred_backend(),
            },
            step: 0,
            error: None,
        });
    }

    /// A free name for a new machine, prefixed with the distribution the wizard
    /// opens on — `ubuntu-1` on Windows, `debian-1` on Linux. The name becomes
    /// the disk, the profile and the hostname, so suggesting the wrong distro's
    /// name is a label that outlives the wizard.
    fn suggest_name(&self) -> String {
        for n in 1..=99 {
            let candidate = format!(
                "{}-{n}",
                launcher::GuestFamily::default_for_host().cli_name()
            );
            let taken = self.vm(&candidate).is_some()
                || self.pending.iter().any(|p| p.name == candidate)
                || self
                    .settings
                    .vm_dir
                    .join(format!("{candidate}.toml"))
                    .exists();
            if !taken {
                return candidate;
            }
        }
        format!(
            "{}-new",
            launcher::GuestFamily::default_for_host().cli_name()
        )
    }

    /// Which distribution and Linux engine the WSL pre-flight is about.
    fn wsl_target(&self) -> wslengine::Target {
        wslengine::Target::new(
            &self.settings.wsl_distro,
            self.settings.wsl_entangled.as_deref(),
        )
    }

    /// Starts the WSL pre-flight if it has not already answered this question.
    ///
    /// Cheap to call from anywhere — the wizard opening, a settings save, the
    /// Diagnostics view — because it does nothing when the answer it holds is
    /// already about the current distribution and engine path. `force` is for
    /// the one case where the same question deserves a new answer: the user
    /// asked, or an install just changed the world.
    fn ensure_wsl_check(&mut self, force: bool) {
        if self.mock_mode || !Backend::Wsl.available_on_host() {
            return;
        }
        let target = self.wsl_target();
        if !force && self.wsl_engine_target.as_ref() == Some(&target) {
            return;
        }
        if self.wsl_engine_rx.is_some() && !force {
            return;
        }
        self.wsl_engine_target = Some(target.clone());
        self.wsl_engine = wslengine::Status::Checking;
        self.wsl_engine_rx = Some(wslengine::spawn_probe(target, Arc::clone(&self.waker)));
    }

    /// Downloads the pinned Linux engine and copies it into the distribution.
    ///
    /// The button behind this exists because the honest answer to "your WSL has
    /// no engine" is not a text field asking for a path a Windows user does not
    /// have — it is doing the work. What comes back is an absolute Linux path,
    /// and that path is *saved as the setting*: `wsl -e` runs no login shell, so
    /// `~/.local/bin` being on a terminal's PATH proves nothing about a launch.
    fn install_wsl_engine(&mut self) {
        if self.wsl_install_running {
            return;
        }
        if let Some(block) = wslengine::install_block() {
            self.toast(ToastLevel::Error, block);
            return;
        }
        let distro = self.wsl_target().distro;
        let distro = if distro.is_empty() {
            backend::DEFAULT_WSL_DISTRO.to_string()
        } else {
            distro
        };
        self.wsl_install_running = true;
        self.toast(
            ToastLevel::Info,
            format!("downloading the Linux engine and installing it into {distro}…"),
        );
        self.wsl_install_rx = Some(wslengine::spawn_install(distro, Arc::clone(&self.waker)));
    }

    /// Collects both WSL-engine channels; each delivers at most one message.
    fn collect_wsl_events(&mut self) {
        if let Some(rx) = &self.wsl_engine_rx {
            if let Ok(answer) = rx.try_recv() {
                self.wsl_engine_rx = None;
                self.wsl_engine = match answer {
                    Ok(found) => {
                        tracing::info!(engine = %found.summary(), "WSL engine found");
                        wslengine::Status::Ready(found)
                    }
                    Err(fault) => {
                        tracing::warn!(fault = %fault, "WSL engine unusable");
                        wslengine::Status::Failed(fault)
                    }
                };
            }
        }
        if let Some(rx) = &self.wsl_install_rx {
            if let Ok(result) = rx.try_recv() {
                self.wsl_install_rx = None;
                self.wsl_install_running = false;
                match result {
                    Ok(outcome) => {
                        // The absolute path is the setting, for the reason in
                        // `install_wsl_engine`'s doc comment. Persisted quietly:
                        // a user who pressed one button should not then have to
                        // press Save in a panel they never opened.
                        self.settings.wsl_entangled = Some(outcome.installed.path.clone());
                        self.persist_settings_quietly();
                        let mut message = outcome.summary();
                        if outcome.cached {
                            message.push_str(" (from the verified download already on disk)");
                        }
                        if !outcome.rechecked {
                            message.push_str(
                                " — the distribution has no sha256sum, so the copy inside it \
                                 could not be re-checked; the download itself was verified",
                            );
                        }
                        self.toast(ToastLevel::Success, message);
                        self.ensure_wsl_check(true);
                    }
                    Err(e) => self.toast(
                        ToastLevel::Error,
                        format!("the Linux engine was not installed: {e}"),
                    ),
                }
            }
        }
    }

    /// Which backend a *new* machine should default to on this host.
    ///
    /// The saved default, unless that default is one this host cannot actually
    /// use. A Windows machine with no Linux engine in its WSL must not open the
    /// wizard on WSL: the user would fill in four steps and meet the failure at
    /// the end, which is the bug this whole feature is about. The setting is
    /// not changed — the engine may be installed a minute later — only the
    /// wizard's starting point.
    pub fn preferred_backend(&self) -> Backend {
        let chosen = self.settings.default_backend;
        if !chosen.available_on_host() {
            return Backend::Native;
        }
        if chosen == Backend::Wsl && matches!(self.wsl_engine, wslengine::Status::Failed(_)) {
            return Backend::Native;
        }
        chosen
    }

    /// The pre-flight verdict for one backend, or `None` when there is nothing
    /// standing in the way. The wizard's Create button and `start` both gate on
    /// this, so they cannot disagree.
    pub fn backend_block(&self, backend: Backend) -> Option<String> {
        (backend == Backend::Wsl)
            .then(|| self.wsl_engine.refusal())
            .flatten()
    }

    /// Collects the update-check and update-download answers; both channels
    /// deliver at most one message.
    fn collect_update_events(&mut self) {
        if let Some(rx) = &self.update_check {
            if let Ok(info) = rx.try_recv() {
                self.update = Some(info);
                self.update_check = None;
            }
        }
        if let Some(rx) = &self.update_download {
            if let Ok(result) = rx.try_recv() {
                self.update_downloading = false;
                self.update_download = None;
                match result {
                    Ok(path) => {
                        self.update = None;
                        self.toast(
                            ToastLevel::Success,
                            format!(
                                "installer saved to {} and launched — it takes over from here",
                                path.display()
                            ),
                        );
                    }
                    Err(e) => self.toast(ToastLevel::Error, format!("update failed: {e}")),
                }
            }
        }
    }

    fn install_update(&mut self) {
        if self.update_downloading {
            return;
        }
        let Some(info) = self.update.clone() else {
            return;
        };
        if info.installer_url.is_none() {
            // Banner offers the release page in this case; belt and braces.
            self.toast(
                ToastLevel::Warn,
                "this release has no installer asset — use the release page",
            );
            return;
        }
        self.update_downloading = true;
        self.update_download = Some(update::spawn_download_and_launch(
            info,
            Arc::clone(&self.waker),
        ));
    }

    fn open_settings(&mut self) {
        self.modal = Modal::Settings(SettingsForm {
            vm_dir: self.settings.vm_dir.display().to_string(),
            entangled_binary: self
                .settings
                .entangled_binary
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            work_dir: self
                .settings
                .work_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            headless_install: self.settings.headless_install,
            check_updates_on_startup: self.settings.check_updates_on_startup,
            animations_enabled: self.settings.animations_enabled,
            default_backend: self.settings.default_backend,
            wsl_distro: self.settings.wsl_distro.clone(),
            wsl_entangled: self.settings.wsl_entangled.clone().unwrap_or_default(),
            advanced_open: false,
        });
    }

    /// Writes the settings without a toast — for the changes the application
    /// makes on the user's behalf (a machine remembering its backend), which
    /// should not announce themselves as if the user had pressed Save.
    fn persist_settings_quietly(&mut self) {
        if self.mock_mode {
            return;
        }
        if let Some(path) = &self.settings_path {
            if let Err(e) = self.settings.save_to(path) {
                tracing::warn!(error = %e, "cannot persist settings");
            }
        }
    }

    fn request_scan(&mut self, force: bool) {
        if self.mock_mode {
            return;
        }
        if self.scanner.in_flight {
            return;
        }
        if !force && self.last_scan.elapsed() < SCAN_INTERVAL {
            return;
        }
        let req = ScanRequest {
            vm_dir: self.settings.vm_dir.clone(),
            work_dir: Some(self.settings.child_cwd()),
        };
        match self.scanner.request.send(req) {
            Ok(()) => {
                self.scanner.in_flight = true;
                self.last_scan = Instant::now();
            }
            Err(_) => {
                self.scan_error = Some("the VM scanner thread is gone; restart the manager".into());
            }
        }
    }

    fn collect_scan(&mut self) {
        while let Ok(result) = self.scanner.result.try_recv() {
            self.scanner.in_flight = false;
            match result {
                Ok(scan) => {
                    self.scan = scan;
                    self.scan_error = None;
                }
                Err(message) => self.scan_error = Some(message),
            }
        }
        // A pending install whose profile has appeared is now a real VM.
        let known: Vec<String> = self.scan.vms.iter().map(|vm| vm.name.clone()).collect();
        self.pending
            .retain(|p| !(known.contains(&p.name) && !self.supervisor.is_busy(&p.name)));
    }

    fn open_screenshot_surface(&mut self) {
        if self.screenshot_surface_opened || self.screenshot.is_none() {
            return;
        }
        // The confirmation dialogs need a row to be about, which only exists
        // once the first scan has produced one.
        if self.screenshot_view == ScreenshotView::SnapshotDelete {
            let Some(path) = self.scan.snapshots.first().map(|row| row.path.clone()) else {
                return;
            };
            self.screenshot_surface_opened = true;
            self.ask_delete_snapshot(&path);
            return;
        }
        if self.screenshot_view == ScreenshotView::SnapshotDiscard {
            // The one machine that is actually suspended, which is the only
            // state the dialog can be reached from.
            let Some(name) = self
                .scan
                .vms
                .iter()
                .map(|vm| vm.name.clone())
                .find(|name| self.status_of(name) == Status::Suspended)
            else {
                return;
            };
            self.screenshot_surface_opened = true;
            self.ask_discard_snapshot(&name);
            return;
        }
        let section = match self.screenshot_view {
            ScreenshotView::Editor => EditVmSection::Hardware,
            ScreenshotView::EditorBoot => EditVmSection::BootMedia,
            ScreenshotView::EditorNetwork => EditVmSection::NetworkDisplay,
            ScreenshotView::EditorStorage => EditVmSection::Storage,
            _ => return,
        };
        let Some(name) = self
            .scan
            .vms
            .iter()
            .find(|vm| !self.is_busy(&vm.name))
            .map(|vm| vm.name.clone())
        else {
            return;
        };
        self.screenshot_surface_opened = true;
        self.ask_edit_vm(&name);
        if let Modal::EditVm(state) = &mut self.modal {
            state.section = section;
        }
    }

    /// Turns finished children into toasts, and finishes the install flow by
    /// stamping the wizard's memory/vCPU choice onto the profile the CLI wrote.
    fn collect_task_results(&mut self) {
        for (kind, vm, outcome) in self.supervisor.drain_finished() {
            // A suspend ends the child too, and the exit status cannot tell the
            // two apart: the engine exits cleanly whether the snapshot was
            // written or not (ADR-0006), because a VM that has been told to
            // stop existing must not run on either way. The reply line it
            // printed is the only answer, so it decides the toast.
            if let Some(result) = self.suspend_outcome(&vm) {
                match result {
                    Ok(detail) => {
                        self.toast(ToastLevel::Success, format!("'{vm}' suspended — {detail}"))
                    }
                    Err(reason) => {
                        self.toast(
                            ToastLevel::Error,
                            format!(
                                "'{vm}' could not be suspended: {reason}. It has stopped, and \
                                 no saved session was written."
                            ),
                        );
                        self.log_open = true;
                    }
                }
                self.request_scan(true);
                continue;
            }
            match (kind, outcome.success, outcome.stopped_by_user) {
                (TaskKind::Run, _, true) => {
                    self.toast(ToastLevel::Info, format!("'{vm}' stopped"));
                }
                (TaskKind::Run, true, false) => {
                    self.toast(ToastLevel::Info, format!("'{vm}' powered off"));
                }
                (TaskKind::Install, true, false) => {
                    self.finish_install(&vm);
                }
                (TaskKind::Install, _, true) => {
                    // The CLI treats a signalled installer as a clean exit and
                    // then inspects the disk, so an abort can still leave a
                    // half-installed image — and even a profile — behind.
                    self.toast(
                        ToastLevel::Warn,
                        format!(
                            "installation of '{vm}' was aborted — its partial disk (and any \
                             profile the CLI already wrote) are still there; Delete removes them"
                        ),
                    );
                    self.pending.retain(|p| p.name != vm);
                }
                (kind, false, false) => {
                    let hint = self.failure_hint(&vm);
                    self.toast(
                        ToastLevel::Error,
                        format!(
                            "{} of '{vm}' failed: {}{hint}",
                            kind.label(),
                            outcome.detail
                        ),
                    );
                    self.log_open = true;
                    if let Some(task) = self.supervisor.tasks().iter().rev().find(|t| t.vm == vm) {
                        self.log_selected = Some(task.id);
                    }
                    if kind == TaskKind::Install {
                        self.pending.retain(|p| p.name != vm);
                    }
                }
            }
            self.request_scan(true);
        }
    }

    /// The engine's own answer to a suspend, for the task that just finished.
    ///
    /// `None` when this machine was not being suspended — and also when it was
    /// but the reply never arrived, which is a child that died mid-save. That
    /// falls through to the ordinary failure path, where the log is opened, and
    /// the missing snapshot then speaks for itself.
    fn suspend_outcome(&self, vm: &str) -> Option<Result<String, String>> {
        let task = self.supervisor.tasks().iter().rev().find(|t| t.vm == vm)?;
        task.suspend_requested().then(|| task.save_result())?
    }

    /// Reads the tail of the failed task's log and explains the usual suspects
    /// (a busy TAP being by far the most common).
    fn failure_hint(&self, vm: &str) -> String {
        let Some(task) = self.supervisor.tasks().iter().rev().find(|t| t.vm == vm) else {
            return String::new();
        };
        let (lines, _) = task.log_tail(60);
        match crate::diagnose::explain(&lines) {
            Some(hint) => format!(" — {hint}"),
            None => String::new(),
        }
    }

    fn finish_install(&mut self, vm: &str) {
        let pending = self.pending.iter_mut().find(|p| p.name == vm);
        let mut message = format!("'{vm}' installed");
        if let Some(pending) = pending {
            if !pending.applied {
                pending.applied = true;
                let (profile, memory, vcpus) = (
                    pending.profile_path.clone(),
                    pending.machine.memory_mib,
                    pending.machine.vcpus,
                );
                match discovery::apply_resources(&profile, memory, vcpus) {
                    Ok(()) => message.push_str(&format!(" ({memory} MiB, {vcpus} vCPU)")),
                    Err(e) => {
                        self.toast(
                            ToastLevel::Warn,
                            format!(
                                "'{vm}' installed, but the profile keeps the CLI defaults: {e}"
                            ),
                        );
                        return;
                    }
                }
            }
        }
        self.toast(ToastLevel::Success, message);
    }

    /// Re-resolves the engine and restarts its version probe. Called whenever
    /// something that could change the answer changes: the override, or a fix
    /// the user just applied.
    fn refresh_engine(&mut self) {
        if self.mock_mode {
            return;
        }
        self.engine = launcher::locate_engine(&self.settings).map_err(|e| e.to_string());
        self.engine_version = self.engine.as_ref().ok().map(|engine| {
            launcher::spawn_version_probe(engine.path.clone(), Arc::clone(&self.waker))
        });
    }

    /// The version probe's single answer, folded into the engine status.
    fn collect_engine_version(&mut self) {
        let Some(rx) = &self.engine_version else {
            return;
        };
        if let Ok(version) = rx.try_recv() {
            self.engine_version = None;
            if let Ok(engine) = &mut self.engine {
                engine.version = version;
            }
        }
    }

    fn engine_path(&mut self) -> Option<PathBuf> {
        self.refresh_engine();
        match &self.engine {
            Ok(engine) => Some(engine.path.clone()),
            Err(message) => {
                let message = message.clone();
                self.toast(ToastLevel::Error, message);
                None
            }
        }
    }

    /// Where a machine runs, honouring its own choice and this host's reality.
    pub fn backend_of(&self, vm: &str) -> Backend {
        self.settings.backend_for(vm)
    }

    /// Everything of a machine that must be visible from its backend: the
    /// profile and every disk it declares.
    fn machine_paths(vm: &VmEntry) -> Vec<PathBuf> {
        std::iter::once(vm.profile_path.clone())
            .chain(vm.disks.iter().map(|disk| disk.resolved.clone()))
            .collect()
    }

    fn start(&mut self, name: &str) {
        if self.supervisor.is_busy(name) {
            self.toast(ToastLevel::Warn, format!("'{name}' is already busy"));
            return;
        }
        let Some(vm) = self.vm(name).cloned() else {
            self.toast(ToastLevel::Error, format!("'{name}' is gone from disk"));
            return;
        };
        if let Some(disk) = vm.disks.iter().find(|d| !d.exists) {
            self.toast(
                ToastLevel::Error,
                format!("disk {} is missing", disk.resolved.display()),
            );
            return;
        }
        let Some(cli) = self.engine_path() else {
            return;
        };
        let chosen = self.backend_of(name);

        // The engine the chosen backend would run, before anything is spawned.
        // Without this the WSL case fails as raw `wsl.exe` noise in a log file,
        // which is where this whole feature started.
        if let Some(refusal) = self.backend_block(chosen) {
            self.toast(
                ToastLevel::Error,
                format!("'{name}' cannot start on {}: {refusal}", chosen.label()),
            );
            return;
        }

        // Everything the chosen backend must be able to see, checked here
        // rather than discovered as a "file not found" three layers down.
        let paths = Self::machine_paths(&vm);
        match backend::reachability(chosen, paths.iter().map(PathBuf::as_path)) {
            backend::Reachability::Refused(message) => {
                self.toast(
                    ToastLevel::Error,
                    format!("'{name}' cannot start on {}: {message}", chosen.label()),
                );
                return;
            }
            backend::Reachability::Caveat(message) => self.toast(ToastLevel::Warn, message),
            backend::Reachability::Fine => {}
        }

        let cwd = self.settings.child_cwd();
        // Whichever artifact *this* profile boots from. A UEFI profile (every
        // Ubuntu install) needs the firmware and has no use for a bootstrap
        // kernel, so warning about the kernel there is noise that trains people
        // to ignore the warning that matters.
        //
        // The direct-Linux arm goes through the same three-place lookup the
        // install pre-flight uses rather than a bare `cwd.join(...)`: a profile
        // written on a host that *downloaded* the bootstrap pair names it
        // absolutely, in the cache, and complaining that the working directory
        // has no `artifacts/bootstrap/vmlinuz` would be a false alarm on every
        // start of a perfectly good machine.
        let missing = if vm.uefi {
            (!cwd.join(launcher::UEFI_FIRMWARE).is_file()).then(|| {
                format!(
                    "no {} under {} — a profile with relative boot paths will fail to \
                     start (Settings ▸ Advanced ▸ working directory)",
                    launcher::UEFI_FIRMWARE,
                    cwd.display()
                )
            })
        } else {
            (!launcher::bootstrap_artifacts_present(&cwd)).then(|| {
                format!(
                    "no bootstrap kernel under {} and none in the verified cache — a \
                     profile that boots one will fail to start. `entangled fetch \
                     bootstrap-kernel` downloads it",
                    cwd.display()
                )
            })
        };
        if let Some(message) = missing {
            self.toast(ToastLevel::Warn, message);
        }
        let runner = Runner::new(chosen, cli, &self.settings);
        let spec = match launcher::run_spec(&runner, &vm, cwd, &self.settings.vm_dir) {
            Ok(spec) => spec,
            Err(message) => {
                self.toast(ToastLevel::Error, message);
                return;
            }
        };
        match self.supervisor.spawn(spec) {
            Ok(id) => {
                self.log_selected = Some(id);
                let window = if chosen == Backend::Wsl {
                    " — its window opens through WSLg"
                } else {
                    ""
                };
                self.toast(
                    ToastLevel::Success,
                    format!("starting '{name}' on {}{window}", chosen.label()),
                );
            }
            Err(e) => self.toast(ToastLevel::Error, e.to_string()),
        }
    }

    fn stop(&mut self, name: &str) {
        match self.supervisor.active_task(name) {
            Some(task) if task.stop_requested() => {
                task.request_kill();
                self.toast(ToastLevel::Warn, format!("killing '{name}'"));
            }
            Some(task) => {
                task.request_stop();
                self.toast(ToastLevel::Info, format!("asking '{name}' to shut down"));
            }
            None => self.toast(ToastLevel::Warn, format!("'{name}' is not running")),
        }
    }

    /// Freezes a running VM or lets it continue (ADR-0005).
    ///
    /// Over the control channel `run_spec` opened, which is also why this can
    /// only work for a VM *this* manager started: a control channel is a pipe
    /// to a child, and a VM someone launched from a terminal has none.
    fn toggle_pause(&mut self, name: &str) {
        let Some(task) = self.supervisor.active_task(name) else {
            self.toast(ToastLevel::Warn, format!("'{name}' is not running"));
            return;
        };
        let paused = task.pause_requested();
        let (level, text) = if !task.has_control() {
            (
                ToastLevel::Warn,
                format!("'{name}' has no control channel — it was not started from here"),
            )
        } else if task.set_paused(!paused) {
            (
                ToastLevel::Info,
                if paused {
                    format!("'{name}' resumed")
                } else {
                    format!("'{name}' paused")
                },
            )
        } else {
            (
                ToastLevel::Error,
                format!("could not reach '{name}' to pause it"),
            )
        };
        self.toast(level, text);
    }

    /// Reboots a running VM in place: the same reset the guest's own Restart
    /// performs, in the same process and the same window.
    fn reset(&mut self, name: &str) {
        let Some(task) = self.supervisor.active_task(name) else {
            self.toast(ToastLevel::Warn, format!("'{name}' is not running"));
            return;
        };
        let (level, text) = if !task.has_control() {
            (
                ToastLevel::Warn,
                format!("'{name}' has no control channel — it was not started from here"),
            )
        } else if task.reset() {
            (ToastLevel::Info, format!("restarting '{name}'"))
        } else {
            (
                ToastLevel::Error,
                format!("could not reach '{name}' to restart it"),
            )
        };
        self.toast(level, text);
    }

    /// Writes a running VM to a file and stops it (ADR-0006).
    ///
    /// Down the same control channel Pause and Restart use, and with the same
    /// honesty rule: a machine this manager did not start has no pipe, and the
    /// button says so rather than pretending. What it does *not* do is wait —
    /// a desktop-sized guest takes several seconds, and the frame loop is the
    /// one thing that must keep moving. The card shows the elapsed time, and
    /// the engine's own reply line, read out of the log as it goes past, is
    /// what ends the wait.
    fn suspend(&mut self, name: &str) {
        let Some(task) = self.supervisor.active_task(name) else {
            self.toast(ToastLevel::Warn, format!("'{name}' is not running"));
            return;
        };
        if !task.has_control() {
            self.toast(
                ToastLevel::Warn,
                format!("'{name}' has no control channel — it was not started from here"),
            );
            return;
        }
        if task.suspend_requested() {
            return;
        }
        if task.suspend() {
            self.toast(
                ToastLevel::Info,
                format!("suspending '{name}' — writing its memory to disk"),
            );
        } else {
            self.toast(
                ToastLevel::Error,
                format!("could not reach '{name}' to suspend it"),
            );
        }
    }

    /// Starts a machine from its saved session instead of booting it.
    fn resume_vm(&mut self, name: &str) {
        let Some(row) = self.snapshot_for(name).cloned() else {
            self.toast(
                ToastLevel::Warn,
                format!("'{name}' has no saved session to come back from"),
            );
            return;
        };
        self.resume_snapshot(&row);
    }

    /// Starts `row`, whether or not a profile of that name still exists here.
    ///
    /// The verdict is re-taken at the moment of the click: the Resume button is
    /// already greyed out for a snapshot that cannot be restored, but a disk can
    /// change between two scans and a refusal that has just become true must not
    /// be discovered by a child process.
    fn resume_snapshot(&mut self, row: &SnapshotRow) {
        let Some(name) = row.vm_name().map(str::to_string) else {
            self.toast(
                ToastLevel::Error,
                format!(
                    "{} cannot be read, so there is nothing to resume",
                    row.path.display()
                ),
            );
            return;
        };
        if self.supervisor.is_busy(&name) {
            self.toast(ToastLevel::Warn, format!("'{name}' is already busy"));
            return;
        }
        let verdict = self.snapshot_verdict(row);
        if let Some(reason) = verdict.blocked.first() {
            self.toast(ToastLevel::Error, reason.clone());
            return;
        }
        let Some(cli) = self.engine_path() else {
            return;
        };
        let chosen = self.backend_of(&name);
        // The snapshot file itself has to be visible from the backend, exactly
        // as a profile and its disks do for a cold start.
        match backend::reachability(chosen, std::iter::once(row.path.as_path())) {
            backend::Reachability::Refused(message) => {
                self.toast(
                    ToastLevel::Error,
                    format!("'{name}' cannot resume on {}: {message}", chosen.label()),
                );
                return;
            }
            backend::Reachability::Caveat(message) => self.toast(ToastLevel::Warn, message),
            backend::Reachability::Fine => {}
        }
        let runner = Runner::new(chosen, cli, &self.settings);
        let spec = match launcher::resume_spec(
            &runner,
            &name,
            &row.path,
            self.settings.child_cwd(),
            &self.settings.vm_dir,
        ) {
            Ok(spec) => spec,
            Err(message) => {
                self.toast(ToastLevel::Error, message);
                return;
            }
        };
        match self.supervisor.spawn(spec) {
            Ok(id) => {
                self.log_selected = Some(id);
                self.toast(
                    ToastLevel::Success,
                    format!("resuming '{name}' where it left off"),
                );
            }
            Err(e) => self.toast(ToastLevel::Error, e.to_string()),
        }
    }

    fn ask_delete_snapshot(&mut self, path: &Path) {
        let Some(row) = self
            .scan
            .snapshots
            .iter()
            .find(|row| row.path == path)
            .cloned()
        else {
            self.toast(
                ToastLevel::Error,
                "that saved session is gone from the list",
            );
            return;
        };
        if row.vm_name().is_some_and(|name| self.is_busy(name)) {
            self.toast(
                ToastLevel::Warn,
                "that machine is busy — a snapshot must not be deleted underneath it",
            );
            return;
        }
        self.modal = Modal::DeleteSnapshot(DeleteSnapshotState { row, error: None });
    }

    fn confirm_delete_snapshot(&mut self) {
        let Modal::DeleteSnapshot(state) = &self.modal else {
            return;
        };
        let path = state.row.path.clone();
        match remove_if_present(&path) {
            Ok(()) => {
                self.modal = Modal::None;
                self.toast(
                    ToastLevel::Success,
                    format!("deleted the saved session {}", path.display()),
                );
                self.request_scan(true);
            }
            Err(message) => {
                if let Modal::DeleteSnapshot(state) = &mut self.modal {
                    state.error = Some(message);
                }
            }
        }
    }

    /// "Start it fresh anyway" — the saved session goes first.
    ///
    /// Deleting rather than leaving it is the honest half: the cold-booted
    /// guest writes to the disk within seconds, and from that moment the file
    /// is one the engine would refuse. A snapshot that can only ever produce a
    /// refusal is worse than no snapshot, because the card would go on offering
    /// to resume it.
    fn ask_discard_snapshot(&mut self, name: &str) {
        let Some(row) = self.snapshot_for(name).cloned() else {
            self.start(name);
            return;
        };
        self.modal = Modal::DiscardSnapshot(DiscardSnapshotState {
            vm: name.to_string(),
            row,
            error: None,
        });
    }

    fn confirm_discard_snapshot(&mut self) {
        let Modal::DiscardSnapshot(state) = &self.modal else {
            return;
        };
        let (vm, path) = (state.vm.clone(), state.row.path.clone());
        match remove_if_present(&path) {
            Ok(()) => {
                self.modal = Modal::None;
                self.request_scan(true);
                self.start(&vm);
            }
            Err(message) => {
                if let Modal::DiscardSnapshot(state) = &mut self.modal {
                    state.error = Some(message);
                }
            }
        }
    }

    /// Whether the manager has asked this VM to pause — what the card's
    /// Pause/Resume button is labelled from.
    pub fn is_paused(&self, name: &str) -> bool {
        self.supervisor
            .active_task(name)
            .is_some_and(|task| task.pause_requested())
    }

    /// Whether this VM can be paused or restarted from here at all.
    pub fn has_control(&self, name: &str) -> bool {
        self.supervisor
            .active_task(name)
            .is_some_and(|task| task.has_control())
    }

    fn submit_wizard(&mut self) {
        let Modal::Wizard(state) = &mut self.modal else {
            return;
        };
        let machine = state.machine.clone();
        let trimmed = machine.name.trim().to_string();
        if let Err(e) = discovery::validate_name(&trimmed) {
            state.error = Some(e);
            return;
        }
        let profile_path = self.settings.vm_dir.join(format!("{trimmed}.toml"));
        let disk_path = machine.disk_path(&self.settings.vm_dir);
        if profile_path.exists() || self.vm(&trimmed).is_some() {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(format!("a machine named '{trimmed}' already exists"));
            }
            return;
        }
        if disk_path.parent() != Some(self.settings.vm_dir.as_path()) {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(format!(
                    "keep the installer disk directly in {} so the generated profile stays visible to the manager",
                    self.settings.vm_dir.display()
                ));
            }
            return;
        }
        match machine.disk_mode {
            launcher::DiskMode::CreateNew if disk_path.exists() => {
                if let Modal::Wizard(state) = &mut self.modal {
                    state.error = Some(format!(
                        "{} already exists — choose 'Use existing' or another file name",
                        disk_path.display()
                    ));
                }
                return;
            }
            launcher::DiskMode::UseExisting if !disk_path.is_file() => {
                if let Modal::Wizard(state) = &mut self.modal {
                    state.error = Some(format!(
                        "existing disk {} was not found",
                        disk_path.display()
                    ));
                }
                return;
            }
            _ => {}
        }
        if machine.family == launcher::GuestFamily::Ubuntu
            && !machine.iso_path.trim().is_empty()
            && !Path::new(machine.iso_path.trim()).is_file()
        {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(format!(
                    "installer ISO {} was not found",
                    machine.iso_path.trim()
                ));
            }
            return;
        }
        if self.supervisor.is_busy(&trimmed) {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(format!("'{trimmed}' is already being installed"));
            }
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&self.settings.vm_dir) {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(format!(
                    "cannot create {}: {e}",
                    self.settings.vm_dir.display()
                ));
            }
            return;
        }

        let mut machine = machine;
        machine.name = trimmed.clone();
        let Some(cli) = self.engine_path() else {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(
                    "the Entangled engine could not be found — see Settings ▸ Advanced".into(),
                );
            }
            return;
        };

        if let Some(refusal) = self.backend_block(machine.backend) {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(refusal);
            }
            return;
        }

        match backend::reachability(
            machine.backend,
            [profile_path.as_path(), disk_path.as_path()],
        ) {
            backend::Reachability::Refused(message) => {
                if let Modal::Wizard(state) = &mut self.modal {
                    state.error = Some(message);
                }
                return;
            }
            backend::Reachability::Caveat(message) => self.toast(ToastLevel::Warn, message),
            backend::Reachability::Fine => {}
        }

        let cwd = self.settings.child_cwd();
        // Per distribution: Ubuntu needs the UEFI firmware, Debian the bootstrap
        // kernel. Checking only the kernel used to make the Ubuntu install — the
        // one that works on Windows — unreachable there.
        if let Some(message) = launcher::missing_install_artifact(&cwd, machine.family) {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(message);
            }
            return;
        }
        let runner = Runner::new(machine.backend, cli, &self.settings);
        let spec = match launcher::install_spec(&runner, &self.settings.vm_dir, cwd, &machine) {
            Ok(spec) => spec,
            Err(message) => {
                if let Modal::Wizard(state) = &mut self.modal {
                    state.error = Some(message);
                }
                return;
            }
        };
        let chosen_backend = machine.backend;
        match self.supervisor.spawn(spec) {
            Ok(id) => {
                self.pending.push(PendingInstall {
                    name: trimmed.clone(),
                    machine,
                    profile_path,
                    applied: false,
                });
                self.log_selected = Some(id);
                self.log_open = true;
                self.modal = Modal::None;
                // The machine keeps the backend it was installed on: an Ubuntu
                // installed under WSL boots there again without the user having
                // to remember which host built it.
                self.settings.set_backend_for(&trimmed, chosen_backend);
                self.persist_settings_quietly();
                self.toast(
                    ToastLevel::Success,
                    format!("installing '{trimmed}' — watch the log below"),
                );
            }
            Err(e) => {
                if let Modal::Wizard(state) = &mut self.modal {
                    state.error = Some(e.to_string());
                }
            }
        }
    }

    fn ask_delete(&mut self, name: &str) {
        if self.is_busy(name) {
            self.toast(
                ToastLevel::Warn,
                format!("'{name}' is busy — stop it before deleting"),
            );
            return;
        }
        let Some(vm) = self.vm(name) else {
            self.toast(ToastLevel::Error, format!("'{name}' is gone from disk"));
            return;
        };
        self.modal = Modal::Delete(DeleteState {
            name: name.to_string(),
            typed: String::new(),
            plan: discovery::plan_delete(vm, &self.settings.vm_dir),
            error: None,
        });
    }

    fn confirm_delete(&mut self) {
        let Modal::Delete(state) = &self.modal else {
            return;
        };
        let (name, typed) = (state.name.clone(), state.typed.clone());
        let busy = self.supervisor.is_busy(&name);
        let Some(vm) = self.vm(&name).cloned() else {
            self.modal = Modal::None;
            self.toast(ToastLevel::Warn, format!("'{name}' is already gone"));
            return;
        };
        match discovery::delete(&vm, &self.settings.vm_dir, busy, &typed) {
            Ok(removed) => {
                self.modal = Modal::None;
                self.toast(
                    ToastLevel::Success,
                    format!("deleted '{name}' ({} files)", removed.len()),
                );
                self.request_scan(true);
            }
            Err(e) => {
                if let Modal::Delete(state) = &mut self.modal {
                    state.error = Some(e.to_string());
                }
            }
        }
    }

    fn save_settings(&mut self) {
        let Modal::Settings(form) = &self.modal else {
            return;
        };
        let vm_dir = PathBuf::from(form.vm_dir.trim());
        if vm_dir.as_os_str().is_empty() {
            if let Modal::Settings(form) = &mut self.modal {
                form.vm_dir = self.settings.vm_dir.display().to_string();
            }
            self.toast(ToastLevel::Warn, "the VM directory must not be empty");
            return;
        }
        let optional = |value: &str| {
            let value = value.trim();
            (!value.is_empty()).then(|| PathBuf::from(value))
        };
        self.settings.vm_dir = vm_dir;
        self.settings.entangled_binary = optional(&form.entangled_binary);
        self.settings.work_dir = optional(&form.work_dir);
        self.settings.headless_install = form.headless_install;
        self.settings.check_updates_on_startup = form.check_updates_on_startup;
        self.settings.animations_enabled = form.animations_enabled;
        self.settings.default_backend = form.default_backend;
        let distro = form.wsl_distro.trim();
        self.settings.wsl_distro = if distro.is_empty() {
            backend::DEFAULT_WSL_DISTRO.to_string()
        } else {
            distro.to_string()
        };
        self.settings.wsl_entangled = {
            let value = form.wsl_entangled.trim();
            (!value.is_empty()).then(|| value.to_string())
        };

        match &self.settings_path {
            Some(path) => match self.settings.save_to(path) {
                Ok(()) => self.toast(
                    ToastLevel::Success,
                    format!("settings saved to {}", path.display()),
                ),
                Err(e) => self.toast(ToastLevel::Error, e.to_string()),
            },
            None => self.toast(
                ToastLevel::Warn,
                "settings applied for this session only (no configuration directory)",
            ),
        }
        self.refresh_engine();
        self.ensure_wsl_check(false);
        self.modal = Modal::None;
        self.request_scan(true);
    }

    // ---- Disks view ---------------------------------------------------

    pub fn disk_row(&self, path: &Path) -> Option<&discovery::DiskRow> {
        self.scan.disks.iter().find(|d| d.path == *path)
    }

    /// True while any VM attached to the disk is running or installing — every
    /// mutation of the disk is refused then.
    pub fn disk_busy(&self, row: &discovery::DiskRow) -> bool {
        row.attachments.iter().any(|a| self.is_busy(&a.vm))
    }

    fn open_create_disk(&mut self) {
        // Suggest a free file name, the same way the wizard suggests VM names.
        let mut name = "disk-1".to_string();
        for n in 1..=99 {
            let candidate = format!("disk-{n}");
            if !self
                .settings
                .vm_dir
                .join(format!("{candidate}.raw"))
                .exists()
            {
                name = candidate;
                break;
            }
        }
        self.modal = Modal::CreateDisk(CreateDiskState {
            name,
            size: format!("{}G", self.settings.default_disk_gib),
            error: None,
        });
    }

    fn submit_create_disk(&mut self) {
        let Modal::CreateDisk(state) = &mut self.modal else {
            return;
        };
        let name = state.name.trim().to_string();
        if let Err(e) = discovery::validate_name(&name) {
            state.error = Some(e);
            return;
        }
        let bytes = match disk_image::parse_size(state.size.trim()) {
            Ok(bytes) => bytes,
            Err(e) => {
                state.error = Some(e.to_string());
                return;
            }
        };
        let path = self.settings.vm_dir.join(format!("{name}.raw"));
        if let Err(e) = std::fs::create_dir_all(&self.settings.vm_dir) {
            if let Modal::CreateDisk(state) = &mut self.modal {
                state.error = Some(format!(
                    "cannot create {}: {e}",
                    self.settings.vm_dir.display()
                ));
            }
            return;
        }
        match disk_image::create_raw(&path, bytes) {
            Ok(()) => {
                self.modal = Modal::None;
                self.toast(
                    ToastLevel::Success,
                    format!(
                        "created {} ({}, sparse)",
                        path.display(),
                        discovery::format_bytes(bytes)
                    ),
                );
                self.request_scan(true);
            }
            Err(e) => {
                if let Modal::CreateDisk(state) = &mut self.modal {
                    state.error = Some(e.to_string());
                }
            }
        }
    }

    fn ask_delete_disk(&mut self, path: &Path) {
        let Some(row) = self.disk_row(path).cloned() else {
            self.toast(ToastLevel::Error, "that disk is gone from the list");
            return;
        };
        if self.disk_busy(&row) {
            self.toast(
                ToastLevel::Warn,
                "a VM using this disk is running — stop it first",
            );
            return;
        }
        self.modal = Modal::DeleteDisk(DeleteDiskState { row, error: None });
    }

    fn confirm_delete_disk(&mut self) {
        let Modal::DeleteDisk(state) = &self.modal else {
            return;
        };
        let row = state.row.clone();
        if self.disk_busy(&row) {
            if let Modal::DeleteDisk(state) = &mut self.modal {
                state.error = Some("a VM using this disk is running — stop it first".into());
            }
            return;
        }
        if !row.attachments.is_empty() {
            if let Modal::DeleteDisk(state) = &mut self.modal {
                state.error = Some(
                    "the disk is still attached to a VM profile — detach it there first".into(),
                );
            }
            return;
        }
        // The reference scan inside remove_disk re-checks the VM directory, so
        // a profile written since the last scan still blocks the delete.
        match disk_image::remove_disk(
            &row.path,
            false,
            std::slice::from_ref(&self.settings.vm_dir),
        ) {
            Ok(outcome) => {
                self.modal = Modal::None;
                let removed_nvram = outcome.removed.len() > 1;
                self.toast(
                    ToastLevel::Success,
                    if removed_nvram {
                        format!("deleted {} and its .nvram sidecar", row.path.display())
                    } else {
                        format!("deleted {}", row.path.display())
                    },
                );
                self.request_scan(true);
            }
            Err(e) => {
                if let Modal::DeleteDisk(state) = &mut self.modal {
                    state.error = Some(e.to_string());
                }
            }
        }
    }

    fn open_attach_disk(&mut self, path: &Path) {
        let Some(row) = self.disk_row(path) else {
            self.toast(ToastLevel::Error, "that disk is gone from the list");
            return;
        };
        let attached: Vec<&str> = row.attachments.iter().map(|a| a.vm.as_str()).collect();
        // Candidates: stopped VMs not already attached to this disk.
        let first_free = self
            .scan
            .vms
            .iter()
            .find(|vm| !attached.contains(&vm.name.as_str()) && !self.supervisor.is_busy(&vm.name))
            .map(|vm| vm.name.clone());
        self.modal = Modal::AttachDisk(AttachDiskState {
            disk: path.to_path_buf(),
            selected: first_free,
            error: None,
        });
    }

    fn submit_attach_disk(&mut self) {
        let Modal::AttachDisk(state) = &self.modal else {
            return;
        };
        let disk = state.disk.clone();
        let Some(vm_name) = state.selected.clone() else {
            if let Modal::AttachDisk(state) = &mut self.modal {
                state.error = Some("pick a machine to attach to".into());
            }
            return;
        };
        let error = if self.supervisor.is_busy(&vm_name) {
            Some(format!("'{vm_name}' is running — stop it first"))
        } else if let Some(vm) = self.vm(&vm_name) {
            discovery::attach_disk(&vm.profile_path.clone(), &disk).err()
        } else {
            Some(format!("'{vm_name}' is gone from disk"))
        };
        match error {
            None => {
                self.modal = Modal::None;
                self.toast(
                    ToastLevel::Success,
                    format!("attached {} to '{vm_name}'", disk.display()),
                );
                self.request_scan(true);
            }
            Some(message) => {
                if let Modal::AttachDisk(state) = &mut self.modal {
                    state.error = Some(message);
                }
            }
        }
    }

    fn detach_disk(&mut self, vm: &str, profile: &Path, declared: &Path) {
        if self.supervisor.is_busy(vm) {
            self.toast(
                ToastLevel::Warn,
                format!("'{vm}' is running — stop it before detaching its disk"),
            );
            return;
        }
        match discovery::detach_disk(profile, declared) {
            Ok(()) => {
                self.toast(
                    ToastLevel::Success,
                    format!("detached {} from '{vm}'", declared.display()),
                );
                self.request_scan(true);
            }
            Err(e) => self.toast(ToastLevel::Error, e),
        }
    }

    // ---- VM editor ------------------------------------------------------

    fn ask_edit_vm(&mut self, name: &str) {
        if self.is_busy(name) {
            self.toast(
                ToastLevel::Warn,
                format!("'{name}' is busy — stop it before editing"),
            );
            return;
        }
        let Some(vm) = self.vm(name) else {
            self.toast(ToastLevel::Error, format!("'{name}' is gone from disk"));
            return;
        };
        let form = if self.mock_mode {
            crate::editor::EditForm::mock(name)
        } else {
            crate::editor::EditForm::from_profile(&vm.profile_path.clone())
        };
        match form {
            Ok(mut form) => {
                // The backend is manager state, not profile state, so it is
                // loaded alongside the profile rather than out of it.
                form.backend = self.backend_of(name);
                form.base_backend = form.backend;
                form.work_dir = self.settings.child_cwd();
                self.modal = Modal::EditVm(EditVmState {
                    form,
                    section: EditVmSection::Hardware,
                    error: None,
                });
            }
            Err(e) => self.toast(
                ToastLevel::Error,
                format!("cannot open '{name}' for editing: {e}"),
            ),
        }
    }

    fn submit_edit_vm(&mut self) {
        let Modal::EditVm(state) = &self.modal else {
            return;
        };
        let name = state.form.name.clone();
        // The VM could have been started from outside between open and save.
        if self.supervisor.is_busy(&name) {
            if let Modal::EditVm(state) = &mut self.modal {
                state.error = Some(format!("'{name}' is running — stop it first"));
            }
            return;
        }
        let backend = state.form.backend;
        match state.form.save() {
            Ok(()) => {
                self.modal = Modal::None;
                self.settings.set_backend_for(&name, backend);
                self.persist_settings_quietly();
                self.toast(ToastLevel::Success, format!("saved '{name}'"));
                self.request_scan(true);
            }
            Err(e) => {
                if let Modal::EditVm(state) = &mut self.modal {
                    state.error = Some(e);
                }
            }
        }
    }

    // ---- Move to another drive -------------------------------------------

    fn ask_move_disk(&mut self, path: &Path) {
        let Some(row) = self.disk_row(path).cloned() else {
            self.toast(ToastLevel::Error, "that disk is gone from the list");
            return;
        };
        if self.disk_busy(&row) {
            self.toast(
                ToastLevel::Warn,
                "a VM using this disk is running — stop it first",
            );
            return;
        }
        self.modal = Modal::MoveDisk(MoveDiskState {
            row,
            dest: String::new(),
            error: None,
            progress: None,
            running: false,
            events: None,
        });
    }

    fn submit_move_disk(&mut self) {
        let Modal::MoveDisk(state) = &self.modal else {
            return;
        };
        if state.running {
            return;
        }
        let row = state.row.clone();
        let dest_text = state.dest.trim().to_string();
        let fail = |app: &mut Self, message: String| {
            if let Modal::MoveDisk(state) = &mut app.modal {
                state.error = Some(message);
            }
        };
        if dest_text.is_empty() {
            fail(self, "name a destination directory".into());
            return;
        }
        // The VM could have started between opening the dialog and Move.
        if self.disk_busy(&row) {
            fail(
                self,
                "a VM using this disk is running — stop it first".into(),
            );
            return;
        }
        // Every profile that references the disk gets rewritten — the
        // attachments the scan found, deduplicated.
        let mut profiles: Vec<PathBuf> = Vec::new();
        for attachment in &row.attachments {
            if !profiles.contains(&attachment.profile) {
                profiles.push(attachment.profile.clone());
            }
        }

        let (tx, rx) = mpsc::channel::<MoveEvent>();
        let waker = Arc::clone(&self.waker);
        let disk = row.path.clone();
        let dest = PathBuf::from(&dest_text);
        let builder = std::thread::Builder::new().name("disk-move".to_string());
        let spawned = builder.spawn(move || {
            let progress_tx = tx.clone();
            let progress_waker = Arc::clone(&waker);
            // Throttle to whole-percent changes: a 100 GiB image would
            // otherwise send one message per MiB.
            let mut last_percent = u64::MAX;
            let mut progress = move |done: u64, total: u64| {
                let percent = (done * 100).checked_div(total).unwrap_or(100);
                if percent != last_percent {
                    last_percent = percent;
                    let _ = progress_tx.send(MoveEvent::Progress(done, total));
                    progress_waker();
                }
            };
            let result = disk_image::move_disk(&disk, &dest, &profiles, &mut progress)
                .map_err(|e| e.to_string());
            let _ = tx.send(MoveEvent::Done(Box::new(result)));
            waker();
        });
        match spawned {
            Ok(_) => {
                if let Modal::MoveDisk(state) = &mut self.modal {
                    state.running = true;
                    state.error = None;
                    state.progress = Some((0, 0));
                    state.events = Some(rx);
                }
            }
            Err(e) => fail(self, format!("cannot start the move worker: {e}")),
        }
    }

    /// Drains the move worker's channel; called once per frame.
    fn collect_move_events(&mut self) {
        let Modal::MoveDisk(state) = &mut self.modal else {
            return;
        };
        let Some(events) = &state.events else { return };
        let mut finished: Option<Result<disk_image::MoveOutcome, String>> = None;
        while let Ok(event) = events.try_recv() {
            match event {
                MoveEvent::Progress(done, total) => state.progress = Some((done, total)),
                MoveEvent::Done(result) => finished = Some(*result),
            }
        }
        match finished {
            None => {}
            Some(Ok(outcome)) => {
                let mut message = format!(
                    "moved {} — {} of data copied and verified",
                    outcome
                        .moved
                        .first()
                        .map(|(_, to)| to.display().to_string())
                        .unwrap_or_default(),
                    discovery::format_bytes(outcome.data_bytes),
                );
                if !outcome.updated_profiles.is_empty() {
                    message.push_str(&format!(
                        "; {} profile(s) updated",
                        outcome.updated_profiles.len()
                    ));
                }
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, message);
                for leftover in outcome.leftover_sources {
                    self.toast(
                        ToastLevel::Warn,
                        format!(
                            "could not delete the source {} — the verified copy is in place, \
                             remove the leftover by hand",
                            leftover.display()
                        ),
                    );
                }
                self.request_scan(true);
            }
            Some(Err(message)) => {
                state.running = false;
                state.events = None;
                state.progress = None;
                state.error = Some(message);
            }
        }
    }

    // ---- Paths, chosen rather than typed ----------------------------------

    /// The text buffer a picked path belongs in.
    ///
    /// One function for both directions — the dialog reads it to decide where
    /// to open, and writes it when the user picks — so a new picker cannot read
    /// one field and fill in another.
    fn picker_field(&mut self, target: PickTarget) -> Option<&mut String> {
        match (&mut self.modal, target) {
            (Modal::Settings(form), PickTarget::VmDir) => Some(&mut form.vm_dir),
            (Modal::Settings(form), PickTarget::EngineBinary) => Some(&mut form.entangled_binary),
            (Modal::Settings(form), PickTarget::WorkDir) => Some(&mut form.work_dir),
            (Modal::Wizard(state), PickTarget::WizardIso) => Some(&mut state.machine.iso_path),
            (Modal::Wizard(state), PickTarget::WizardDisk) => Some(&mut state.machine.disk_path),
            (Modal::MoveDisk(state), PickTarget::MoveDestination) => Some(&mut state.dest),
            (Modal::EditVm(state), target) => match target {
                PickTarget::EditorKernel => Some(&mut state.form.kernel),
                PickTarget::EditorInitramfs => Some(&mut state.form.initramfs),
                PickTarget::EditorFirmware => Some(&mut state.form.firmware),
                PickTarget::EditorNvram => Some(&mut state.form.nvram),
                PickTarget::EditorCdrom => Some(&mut state.form.cdrom),
                PickTarget::EditorAddDisk => Some(&mut state.form.add_disk),
                _ => None,
            },
            _ => None,
        }
    }

    /// Opens one native dialog. Never more than one: they are modal to the OS
    /// anyway, and a queue of them would be a trap rather than a feature.
    fn open_picker(&mut self, target: PickTarget) {
        if self.picker.is_some() {
            return;
        }
        let fallback = match target {
            PickTarget::EngineBinary | PickTarget::WorkDir => self.settings.child_cwd(),
            _ => self.settings.vm_dir.clone(),
        };
        let current = self
            .picker_field(target)
            .map(|value| value.clone())
            .unwrap_or_default();
        let start = picker::start_directory(&current, &fallback);
        let seed = picker::file_name_of(&current);
        match picker::open(target, Some(start), seed, Arc::clone(&self.waker)) {
            Ok(pending) => self.picker = Some(pending),
            Err(message) => self.toast(ToastLevel::Error, message),
        }
    }

    /// Collects the answer, if the dialog has closed. Called once per frame;
    /// the waker means "once per frame" is immediate rather than a heartbeat
    /// away.
    fn collect_picker(&mut self) {
        let Some(pending) = &self.picker else { return };
        let Some(answer) = pending.poll() else { return };
        let target = pending.target;
        self.picker = None;
        let Some(path) = answer else { return }; // cancelled
        let text = path.display().to_string();
        match self.picker_field(target) {
            Some(field) => *field = text,
            // The engine picker is also reachable from the "engine missing"
            // banner, where no form exists to hold the answer. Applying it
            // straight away is the point of that button: one click, fixed.
            None if target == PickTarget::EngineBinary => {
                self.settings.entangled_binary = Some(path);
                self.persist_settings_quietly();
                self.toast(
                    ToastLevel::Success,
                    format!("using the Entangled engine at {text}"),
                );
            }
            None => {}
        }
        // A new engine has to be re-resolved and re-probed straight away, so
        // the status line answers before the user presses Save.
        if target == PickTarget::EngineBinary {
            self.refresh_engine();
        }
    }

    // ---- Diagnostics -------------------------------------------------------

    /// Runs `entangled doctor` for the default backend.
    fn run_diagnostics(&mut self) {
        if self.doctor_running {
            return;
        }
        let Some(engine) = self.engine.as_ref().ok().map(|e| e.path.clone()) else {
            self.doctor = Some(hostcheck::Report {
                command: String::new(),
                backend: self.settings.default_backend,
                healthy: false,
                lines: vec![hostcheck::Line {
                    text: self
                        .engine
                        .as_ref()
                        .err()
                        .cloned()
                        .unwrap_or_else(|| "the Entangled engine is missing".to_string()),
                    level: hostcheck::Level::Failure,
                }],
            });
            return;
        };
        let runner = Runner::new(self.settings.default_backend, engine, &self.settings);
        self.doctor_running = true;
        self.doctor_rx = Some(hostcheck::spawn(
            runner,
            self.settings.child_cwd(),
            Arc::clone(&self.waker),
        ));
    }

    fn collect_diagnostics(&mut self) {
        let Some(rx) = &self.doctor_rx else { return };
        if let Ok(report) = rx.try_recv() {
            self.doctor_rx = None;
            self.doctor_running = false;
            self.doctor = Some(report);
        }
    }

    // ---- Grow a disk -------------------------------------------------------

    fn ask_resize_disk(&mut self, path: &Path) {
        let Some(row) = self.disk_row(path).cloned() else {
            self.toast(ToastLevel::Error, "that disk is gone from the list");
            return;
        };
        if self.disk_busy(&row) {
            self.toast(
                ToastLevel::Warn,
                "a VM using this disk is running — stop it first",
            );
            return;
        }
        // Seed with the current size rounded up to the next whole GiB: the
        // dialog then only has to be nudged upward.
        const GIB: u64 = 1024 * 1024 * 1024;
        let next = row.apparent_bytes.div_ceil(GIB).max(1) + 8;
        self.modal = Modal::ResizeDisk(ResizeDiskState {
            row,
            size: format!("{next}G"),
            error: None,
        });
    }

    fn submit_resize_disk(&mut self) {
        let Modal::ResizeDisk(state) = &self.modal else {
            return;
        };
        let (path, text) = (state.row.path.clone(), state.size.trim().to_string());
        let row = state.row.clone();
        let fail = |app: &mut Self, message: String| {
            if let Modal::ResizeDisk(state) = &mut app.modal {
                state.error = Some(message);
            }
        };
        if self.disk_busy(&row) {
            fail(
                self,
                "a VM using this disk is running — stop it first".into(),
            );
            return;
        }
        let bytes = match disk_image::parse_size(&text) {
            Ok(bytes) => bytes,
            Err(e) => {
                fail(self, e.to_string());
                return;
            }
        };
        match disk_image::resize_raw(&path, bytes) {
            Ok(outcome) => {
                self.modal = Modal::None;
                self.toast(
                    ToastLevel::Success,
                    format!(
                        "{} is now {} (was {}) — grow the filesystem inside the guest to use it",
                        path.display(),
                        discovery::format_bytes(outcome.new_bytes),
                        discovery::format_bytes(outcome.previous_bytes)
                    ),
                );
                self.request_scan(true);
            }
            Err(e) => fail(self, e.to_string()),
        }
    }

    fn reveal(&mut self, path: &Path) {
        match reveal_in_file_manager(path) {
            Ok(()) => {}
            Err(e) => self.toast(
                ToastLevel::Error,
                format!("cannot open the file manager: {e}"),
            ),
        }
    }

    /// Handles actions that would otherwise touch the host. Navigation and
    /// opening dialogs still use the real application paths; committing a
    /// change is simulated in memory and acknowledged with a toast.
    fn apply_mock(&mut self, action: &Action) -> bool {
        if !self.mock_mode {
            return false;
        }
        match action {
            Action::Refresh => {
                self.manual_refresh_at = Some(Instant::now());
                self.toast(ToastLevel::Success, "Mock data refreshed");
            }
            Action::Start(name) => {
                self.mock_statuses.insert(name.clone(), Status::Running);
                self.toast(ToastLevel::Success, format!("Mock: started '{name}'"));
            }
            Action::Stop(name) => {
                self.mock_statuses.insert(name.clone(), Status::Stopped);
                self.toast(ToastLevel::Info, format!("Mock: stopped '{name}'"));
            }
            // Suspend and Resume move the fixture between the same two states
            // the real ones move a machine between — a snapshot row appearing
            // and disappearing — so the card states can be reviewed without a
            // guest, a hypervisor or a half-gigabyte file.
            Action::Suspend(name) => {
                self.mock_statuses.insert(name.clone(), Status::Suspending);
                self.toast(ToastLevel::Info, format!("Mock: suspending '{name}'"));
            }
            Action::ResumeVm(name) => {
                if let Some(row) = self.snapshot_for(name).map(|row| row.path.clone()) {
                    self.scan.snapshots.retain(|snap| snap.path != row);
                }
                self.mock_statuses.insert(name.clone(), Status::Running);
                self.toast(ToastLevel::Success, format!("Mock: resumed '{name}'"));
            }
            Action::ResumeSnapshot(path) => {
                let name = self
                    .scan
                    .snapshots
                    .iter()
                    .find(|row| row.path == *path)
                    .and_then(|row| row.vm_name().map(str::to_string));
                self.scan.snapshots.retain(|row| row.path != *path);
                if let Some(name) = name {
                    self.mock_statuses.insert(name.clone(), Status::Running);
                    self.toast(ToastLevel::Success, format!("Mock: resumed '{name}'"));
                }
            }
            // "Start fresh" on a machine whose saved session has already gone
            // is just a start, and `ask_discard_snapshot` does exactly that —
            // including the child spawn, which mock mode must never reach. The
            // guard leaves the ordinary case (there *is* a session) to the real
            // handler, which only opens a modal.
            Action::AskDiscardSnapshot(name) if self.snapshot_for(name).is_none() => {
                self.mock_statuses.insert(name.clone(), Status::Running);
                self.toast(ToastLevel::Success, format!("Mock: started '{name}'"));
            }
            Action::ConfirmDeleteSnapshot => {
                if let Modal::DeleteSnapshot(state) = &self.modal {
                    let path = state.row.path.clone();
                    self.scan.snapshots.retain(|row| row.path != path);
                }
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, "Mock: saved session forgotten");
            }
            Action::ConfirmDiscardSnapshot => {
                if let Modal::DiscardSnapshot(state) = &self.modal {
                    let (path, vm) = (state.row.path.clone(), state.vm.clone());
                    self.scan.snapshots.retain(|row| row.path != path);
                    self.mock_statuses.insert(vm, Status::Running);
                }
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, "Mock: started fresh in memory");
            }
            Action::SubmitWizard => {
                let name = match &self.modal {
                    Modal::Wizard(state) => state.machine.name.trim().to_string(),
                    _ => "preview-machine".into(),
                };
                self.modal = Modal::None;
                self.toast(
                    ToastLevel::Success,
                    format!("Mock: create flow completed for '{name}'"),
                );
            }
            Action::SaveSettings => {
                if let Modal::Settings(form) = &self.modal {
                    self.settings.animations_enabled = form.animations_enabled;
                    self.settings.headless_install = form.headless_install;
                    self.settings.check_updates_on_startup = form.check_updates_on_startup;
                }
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, "Mock: settings applied for this run");
            }
            Action::SubmitEditVm => {
                let result = match &self.modal {
                    Modal::EditVm(state) => state.form.to_config(),
                    _ => return true,
                };
                match result {
                    Ok(_) => {
                        self.modal = Modal::None;
                        self.toast(ToastLevel::Success, "Mock: machine changes saved in memory");
                    }
                    Err(error) => {
                        if let Modal::EditVm(state) = &mut self.modal {
                            state.error = Some(error);
                        }
                    }
                }
            }
            Action::ConfirmDelete => {
                let Some((name, typed)) = (match &self.modal {
                    Modal::Delete(state) => Some((state.name.clone(), state.typed.clone())),
                    _ => None,
                }) else {
                    return true;
                };
                if typed.trim() != name {
                    if let Modal::Delete(state) = &mut self.modal {
                        state.error = Some(format!("type '{name}' to confirm"));
                    }
                    return true;
                }
                self.scan.vms.retain(|vm| vm.name != name);
                for disk in &mut self.scan.disks {
                    disk.attachments.retain(|attachment| attachment.vm != name);
                }
                self.mock_statuses.remove(&name);
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, format!("Mock: deleted '{name}'"));
            }
            Action::SubmitCreateDisk => {
                let Some(name) = (match &self.modal {
                    Modal::CreateDisk(state) => Some(state.name.trim().to_string()),
                    _ => None,
                }) else {
                    return true;
                };
                let file_name = if name.ends_with(".raw") {
                    name
                } else {
                    format!("{name}.raw")
                };
                self.scan.disks.push(discovery::DiskRow {
                    path: PathBuf::from("mock-vms").join(&file_name),
                    file_name,
                    exists: true,
                    apparent_bytes: 8 * 1024 * 1024 * 1024,
                    allocated_bytes: Some(4 * 1024 * 1024),
                    attachments: Vec::new(),
                    nvram: false,
                    summary: Ok("Empty RAW image".into()),
                });
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, "Mock: disk added in memory");
            }
            Action::ConfirmDeleteDisk => {
                let Some(path) = (match &self.modal {
                    Modal::DeleteDisk(state) => Some(state.row.path.clone()),
                    _ => None,
                }) else {
                    return true;
                };
                self.scan.disks.retain(|disk| disk.path != path);
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, "Mock: disk removed in memory");
            }
            Action::SubmitAttachDisk => {
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, "Mock: attach action accepted");
            }
            Action::DetachDisk { .. } => {
                self.toast(ToastLevel::Success, "Mock: detach action accepted");
            }
            Action::SubmitResizeDisk => {
                self.modal = Modal::None;
                self.toast(ToastLevel::Success, "Mock: disk grown in memory");
            }
            Action::CheckWslEngine => {
                self.toast(
                    ToastLevel::Info,
                    "Mock: no WSL is probed; the fixture keeps its 'no engine' answer",
                );
            }
            Action::InstallWslEngine => {
                // No download, no `wsl.exe`, no child: the fixture simply moves
                // to the state a real install would have produced, so the
                // before-and-after of the button is reviewable in a screenshot.
                self.wsl_engine = wslengine::Status::Ready(control_api::wsl::EngineFound {
                    distro: crate::backend::DEFAULT_WSL_DISTRO.to_string(),
                    command: "/home/spider/.local/bin/entangled".to_string(),
                    path: Some("/home/spider/.local/bin/entangled".to_string()),
                    version: Some(crate::VERSION.to_string()),
                });
                self.toast(
                    ToastLevel::Success,
                    "Mock: the Linux engine would be downloaded, verified and installed",
                );
            }
            Action::RunDiagnostics => {
                // A canned report rather than a real `doctor`: mock mode never
                // probes the host, and a screenshot must show the same panel
                // on every machine.
                self.doctor = Some(crate::hostcheck::mock_report());
                self.toast(ToastLevel::Info, "Mock: showing a sample host report");
            }
            Action::PickPath(_) => {
                // A native dialog is not a host *mutation*, but it is modal and
                // unpredictable — a screenshot run must never stop on one.
                self.toast(
                    ToastLevel::Info,
                    "Mock mode: the file browser is not opened; type a path instead",
                );
            }
            Action::CreateVmDir
            | Action::Reveal(_)
            | Action::AskMoveDisk(_)
            | Action::SubmitMoveDisk
            | Action::InstallUpdate
            | Action::OpenReleasePage => {
                self.toast(
                    ToastLevel::Info,
                    "Mock mode: host-side action received; nothing was changed",
                );
            }
            _ => return false,
        }
        true
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        if self.apply_mock(&action) {
            return;
        }
        match action {
            Action::Refresh => {
                self.manual_refresh_at = Some(Instant::now());
                self.request_scan(true);
            }
            Action::CreateVmDir => {
                let dir = self.settings.vm_dir.clone();
                match std::fs::create_dir_all(&dir) {
                    Ok(()) => {
                        self.toast(ToastLevel::Success, format!("created {}", dir.display()));
                        self.request_scan(true);
                    }
                    Err(e) => self.toast(
                        ToastLevel::Error,
                        format!("cannot create {}: {e}", dir.display()),
                    ),
                }
            }
            Action::OpenWizard => self.open_wizard(),
            Action::CloseModal => {
                // A move in flight owns its modal: the copy is running on the
                // worker and closing the dialog would orphan its progress.
                if !matches!(&self.modal, Modal::MoveDisk(state) if state.running) {
                    self.modal = Modal::None;
                }
            }
            Action::SubmitWizard => self.submit_wizard(),
            Action::OpenSettings => self.open_settings(),
            Action::SaveSettings => self.save_settings(),
            Action::Start(name) => self.start(&name),
            Action::Stop(name) => self.stop(&name),
            Action::TogglePause(name) => self.toggle_pause(&name),
            Action::Reset(name) => self.reset(&name),
            Action::Suspend(name) => self.suspend(&name),
            Action::ResumeVm(name) => self.resume_vm(&name),
            Action::ResumeSnapshot(path) => {
                match self.scan.snapshots.iter().find(|r| r.path == path).cloned() {
                    Some(row) => self.resume_snapshot(&row),
                    None => self.toast(
                        ToastLevel::Error,
                        format!("{} is gone from the list", path.display()),
                    ),
                }
            }
            Action::AskDeleteSnapshot(path) => self.ask_delete_snapshot(&path),
            Action::ConfirmDeleteSnapshot => self.confirm_delete_snapshot(),
            Action::AskDiscardSnapshot(name) => self.ask_discard_snapshot(&name),
            Action::ConfirmDiscardSnapshot => self.confirm_discard_snapshot(),
            Action::AskDelete(name) => self.ask_delete(&name),
            Action::ConfirmDelete => self.confirm_delete(),
            Action::CopyProfilePath(path) => {
                ctx.copy_text(path.clone());
                self.toast(ToastLevel::Info, format!("copied {path}"));
            }
            Action::SelectLog(id) => {
                self.log_selected = Some(id);
                self.log_open = true;
                self.log_follow = true;
            }
            Action::ToggleLogPane => self.log_open = !self.log_open,
            Action::DismissToast(index) => {
                if index < self.toasts.len() {
                    self.toasts.remove(index);
                }
            }
            Action::InstallUpdate => self.install_update(),
            Action::OpenReleasePage => {
                if let Some(info) = &self.update {
                    ctx.open_url(egui::OpenUrl::new_tab(info.page_url.clone()));
                }
            }
            Action::DismissUpdate => {
                // For this session only: the check runs once per startup, so
                // the banner returns on the next launch while the version is
                // still newer.
                self.update = None;
            }
            Action::SwitchView(view) => {
                self.view = view;
                self.request_scan(true);
            }
            Action::OpenCreateDisk => self.open_create_disk(),
            Action::SubmitCreateDisk => self.submit_create_disk(),
            Action::AskDeleteDisk(path) => self.ask_delete_disk(&path),
            Action::ConfirmDeleteDisk => self.confirm_delete_disk(),
            Action::OpenAttachDisk(path) => self.open_attach_disk(&path),
            Action::SubmitAttachDisk => self.submit_attach_disk(),
            Action::DetachDisk {
                vm,
                profile,
                declared,
            } => self.detach_disk(&vm, &profile, &declared),
            Action::Reveal(path) => self.reveal(&path),
            Action::AskEditVm(name) => self.ask_edit_vm(&name),
            Action::SubmitEditVm => self.submit_edit_vm(),
            Action::AskMoveDisk(path) => self.ask_move_disk(&path),
            Action::SubmitMoveDisk => self.submit_move_disk(),
            Action::PickPath(target) => self.open_picker(target),
            Action::RunDiagnostics => self.run_diagnostics(),
            Action::CheckWslEngine => self.ensure_wsl_check(true),
            Action::InstallWslEngine => self.install_wsl_engine(),
            Action::AskResizeDisk(path) => self.ask_resize_disk(&path),
            Action::SubmitResizeDisk => self.submit_resize_disk(),
        }
    }

    /// Auto-selects a log to show: the newest active task, else the newest task.
    fn ensure_log_selection(&mut self) {
        let valid = self
            .log_selected
            .is_some_and(|id| self.supervisor.task(id).is_some());
        if valid {
            return;
        }
        self.log_selected = self
            .supervisor
            .tasks()
            .iter()
            .rev()
            .find(|t| t.is_active())
            .or_else(|| self.supervisor.tasks().last())
            .map(|t| t.id);
    }

    fn expire_toasts(&mut self, now: f64) {
        for toast in &mut self.toasts {
            if toast.born.is_nan() {
                toast.born = now;
            }
        }
        self.toasts
            .retain(|toast| now - toast.born < toast.lifetime());
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.screenshot else {
            return;
        };
        // Give the window a few frames to lay out and the animation to move.
        if !job.requested && self.frame > 45 {
            job.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            return;
        }
        if !job.requested {
            return;
        }
        let image = ctx.input(|i| {
            i.events.iter().rev().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        if let Some(image) = image {
            let path = job.path.clone();
            match save_png(&path, &image) {
                Ok(()) => tracing::info!(path = %path.display(), "screenshot saved"),
                Err(e) => tracing::error!(error = %e, "cannot save the screenshot"),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// Card status (GUI-1601).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Stopping,
    Installing,
    /// Writing itself to a snapshot file, on its way out (ADR-0006).
    Suspending,
    /// Not running, and not a plain stopped machine either: there is a file
    /// holding everything it was in the middle of.
    Suspended,
    Stopped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Running => "Running",
            Status::Stopping => "Stopping",
            Status::Installing => "Installing",
            Status::Suspending => "Suspending",
            Status::Suspended => "Suspended",
            Status::Stopped => "Stopped",
        }
    }

    pub fn color(self) -> egui::Color32 {
        match self {
            Status::Running => theme::OK,
            Status::Stopping => theme::WARN,
            Status::Installing => theme::CYAN,
            Status::Suspending => theme::VIOLET_DEEP,
            Status::Suspended => theme::VIOLET,
            Status::Stopped => theme::TEXT_FAINT,
        }
    }

    /// One sentence for the badge's hover, because three of the six states are
    /// not obvious from their name alone.
    pub fn tooltip(self) -> &'static str {
        match self {
            Status::Running => "The machine is running in its own window.",
            Status::Stopping => "Shutdown was asked for; the guest is closing down.",
            Status::Installing => "An installer is running inside it.",
            Status::Suspending => {
                "Writing the machine's memory to a file. It stops when that finishes; \
                 opening it again puts you back exactly here."
            }
            Status::Suspended => {
                "Not running, but not shut down either: a saved session on disk holds \
                 everything it had open. Resume goes back into it."
            }
            Status::Stopped => "Powered off. Starting it boots the guest from scratch.",
        }
    }
}

impl eframe::App for ManagerApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, a] = theme::BG_DEEP.to_array();
        [
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a as f32 / 255.0,
        ]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame += 1;
        theme::set_motion_enabled(self.settings.animations_enabled);
        self.request_scan(false);
        self.collect_scan();
        self.open_screenshot_surface();
        self.collect_task_results();
        self.collect_update_events();
        self.collect_wsl_events();
        self.collect_move_events();
        self.collect_engine_version();
        self.collect_picker();
        self.collect_diagnostics();
        self.supervisor.prune(12);
        self.ensure_log_selection();
        self.expire_toasts(ctx.input(|i| i.time));

        // Tell the sampler what to measure, take its latest answer. Both are
        // one mutex swap; the sampling itself lives on the metrics thread.
        let targets: Vec<(String, u32)> = self
            .supervisor
            .tasks()
            .iter()
            .filter(|t| t.is_active())
            .map(|t| (t.vm.clone(), t.pid))
            .collect();
        self.metrics
            .set_targets(targets, self.settings.vm_dir.clone());
        self.stats = self.metrics.snapshot();

        let mut actions = Vec::new();
        ui::header::show(ctx, self, &mut actions);
        if self.log_open {
            ui::logpane::show(ctx, self, &mut actions);
        }
        match self.view {
            View::Machines => ui::cards::show(ctx, self, &mut actions),
            View::Disks => ui::disks::show(ctx, self, &mut actions),
            View::Snapshots => ui::snapshots::show(ctx, self, &mut actions),
            View::Diagnostics => ui::diagnostics::show(ctx, self, &mut actions),
        }
        ui::dialogs::show(ctx, self, &mut actions);
        ui::toasts::show(ctx, &self.toasts, &mut actions);

        for action in actions {
            self.apply(action, ctx);
        }

        self.handle_screenshot(ctx);
        let window_focused = ctx.input(|input| input.viewport().focused.unwrap_or(true));
        // Screenshot renders intentionally run without taking focus from the
        // user's current window, but still need their short frame sequence.
        if self.settings.animations_enabled && (window_focused || self.screenshot.is_some()) {
            ctx.request_repaint_after(FRAME_INTERVAL);
        } else {
            // One quiet heartbeat keeps host stats, task uptime and toast
            // expiry fresh without burning a 25 FPS idle loop.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}

/// Deletes a file, treating "it was already gone" as success.
///
/// Both callers are confirmations the user has just agreed to, and a second
/// manager (or the engine itself, resuming and re-saving) may have removed the
/// file between the dialog opening and the button. Reporting that as a failure
/// would leave a modal open over a state that is already what was asked for.
fn remove_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot delete {}: {e}", path.display())),
    }
}

/// Opens the platform file manager with `path` selected (or its directory
/// shown). Fire and forget: the child is the user's file manager, not ours.
fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        // `/select,` highlights the file itself; the comma is part of the flag.
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = path.parent().unwrap_or(Path::new("."));
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        Err("no file manager integration on this platform".to_string())
    }
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        image.width() as u32,
        image.height() as u32,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|p| p.to_array())
        .collect::<Vec<u8>>();
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_lifetimes_favour_errors() {
        let toast = |level| Toast {
            text: String::new(),
            level,
            born: 0.0,
        };
        assert!(toast(ToastLevel::Error).lifetime() > toast(ToastLevel::Warn).lifetime());
        assert!(toast(ToastLevel::Warn).lifetime() > toast(ToastLevel::Info).lifetime());
    }

    #[test]
    fn status_labels_and_colors_are_distinct() {
        let all = [
            Status::Running,
            Status::Stopping,
            Status::Installing,
            Status::Suspending,
            Status::Suspended,
            Status::Stopped,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.label(), b.label());
                assert_ne!(a.color(), b.color());
            }
        }
    }
}
