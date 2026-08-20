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

use crate::discovery::{self, Scan, VmEntry};
use crate::launcher::{self, NewMachine};
use crate::metrics;
use crate::process::{Supervisor, TaskId, TaskKind};
use crate::settings::{self, Settings};
use crate::theme;
use crate::ui;
use crate::update::{self, UpdateInfo};
use crate::ScreenshotView;

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
    /// Open the host file manager with the disk selected (explorer/xdg-open).
    RevealDisk(PathBuf),
    // ---- VM editor ---------------------------------------------------------
    AskEditVm(String),
    SubmitEditVm,
    // ---- Move to another drive ----------------------------------------------
    AskMoveDisk(PathBuf),
    SubmitMoveDisk,
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
    pub cli: Result<PathBuf, String>,
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

        let mut app = Self {
            mock_mode,
            view: View::default(),
            cli: if mock_mode {
                Ok(PathBuf::from("mock-entangled"))
            } else {
                launcher::locate_cli(&settings).map_err(|e| e.to_string())
            },
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

        match startup.screenshot_view {
            ScreenshotView::Wizard => app.open_wizard(),
            ScreenshotView::Settings => app.open_settings(),
            ScreenshotView::Disks => app.view = View::Disks,
            ScreenshotView::Main | ScreenshotView::Editor => {}
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
    pub fn status_of(&self, name: &str) -> Status {
        if let Some(status) = self.mock_statuses.get(name) {
            return *status;
        }
        match self.supervisor.active_kind(name) {
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
        }
    }

    pub fn is_busy(&self, name: &str) -> bool {
        if self.mock_mode {
            return matches!(
                self.status_of(name),
                Status::Running | Status::Stopping | Status::Installing
            );
        }
        self.supervisor.is_busy(name)
    }

    pub fn vm(&self, name: &str) -> Option<&VmEntry> {
        self.scan.vms.iter().find(|vm| vm.name == name)
    }

    fn open_wizard(&mut self) {
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
        });
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
        if self.screenshot_surface_opened
            || self.screenshot.is_none()
            || self.screenshot_view != ScreenshotView::Editor
        {
            return;
        }
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
    }

    /// Turns finished children into toasts, and finishes the install flow by
    /// stamping the wizard's memory/vCPU choice onto the profile the CLI wrote.
    fn collect_task_results(&mut self) {
        for (kind, vm, outcome) in self.supervisor.drain_finished() {
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

    fn cli_path(&mut self) -> Option<PathBuf> {
        self.cli = launcher::locate_cli(&self.settings).map_err(|e| e.to_string());
        match &self.cli {
            Ok(path) => Some(path.clone()),
            Err(message) => {
                let message = message.clone();
                self.toast(ToastLevel::Error, message);
                None
            }
        }
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
        let Some(cli) = self.cli_path() else { return };

        let cwd = self.settings.child_cwd();
        // Whichever artifact *this* profile boots from. A UEFI profile (every
        // Ubuntu install) needs the firmware and has no use for a bootstrap
        // kernel, so warning about the kernel there is noise that trains people
        // to ignore the warning that matters.
        let artifact = if vm.uefi {
            launcher::UEFI_FIRMWARE
        } else {
            launcher::BOOTSTRAP_KERNEL
        };
        if !cwd.join(artifact).is_file() {
            self.toast(
                ToastLevel::Warn,
                format!(
                    "no {artifact} under {} — a profile with relative boot paths will \
                     fail to start (Settings ▸ working directory)",
                    cwd.display()
                ),
            );
        }
        let spec = launcher::run_spec(&cli, &vm, cwd, &self.settings.vm_dir);
        match self.supervisor.spawn(spec) {
            Ok(id) => {
                self.log_selected = Some(id);
                self.toast(ToastLevel::Success, format!("starting '{name}'"));
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
        let Some(cli) = self.cli_path() else {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some("the entangled CLI was not found (see Settings)".into());
            }
            return;
        };

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
        let spec = launcher::install_spec(&cli, &self.settings.vm_dir, cwd, &machine);
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
        self.cli = launcher::locate_cli(&self.settings).map_err(|e| e.to_string());
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
            Ok(form) => {
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
        match state.form.save() {
            Ok(()) => {
                self.modal = Modal::None;
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

    fn reveal_disk(&mut self, path: &Path) {
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
            Action::CreateVmDir
            | Action::RevealDisk(_)
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
            Action::RevealDisk(path) => self.reveal_disk(&path),
            Action::AskEditVm(name) => self.ask_edit_vm(&name),
            Action::SubmitEditVm => self.submit_edit_vm(),
            Action::AskMoveDisk(path) => self.ask_move_disk(&path),
            Action::SubmitMoveDisk => self.submit_move_disk(),
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
    Stopped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Running => "Running",
            Status::Stopping => "Stopping",
            Status::Installing => "Installing",
            Status::Stopped => "Stopped",
        }
    }

    pub fn color(self) -> egui::Color32 {
        match self {
            Status::Running => theme::OK,
            Status::Stopping => theme::WARN,
            Status::Installing => theme::CYAN,
            Status::Stopped => theme::TEXT_FAINT,
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
        self.collect_move_events();
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
