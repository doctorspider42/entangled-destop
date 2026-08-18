//! The eframe application: state, the action loop and the frame layout.
//!
//! Rules of the house:
//! - the frame loop never blocks — scans run on a worker thread, children are
//!   supervised by [`crate::process`];
//! - every failure becomes a toast or a banner, never a panic;
//! - UI code produces [`Action`]s, `ManagerApp::apply` is the only place that
//!   mutates state or touches the filesystem.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::discovery::{self, Scan, VmEntry};
use crate::launcher::{self, NewMachine};
use crate::process::{Supervisor, TaskId, TaskKind};
use crate::settings::{self, Settings};
use crate::theme;
use crate::ui;
use crate::ScreenshotView;

/// How often the VM directory is re-scanned while the window is open.
const SCAN_INTERVAL: Duration = Duration::from_millis(2500);
/// Animation clock cadence: the logo and status indicators are always moving.
const FRAME_INTERVAL: Duration = Duration::from_millis(40);

pub struct Startup {
    pub vm_dir: Option<PathBuf>,
    pub entangled: Option<PathBuf>,
    pub screenshot: Option<PathBuf>,
    pub screenshot_view: ScreenshotView,
}

pub fn launch(startup: Startup) -> Result<(), String> {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([880.0, 560.0])
            .with_title("Entangled Manager")
            .with_app_id("entangled-manager"),
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
        "Entangled Manager",
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

/// Wizard state (GUI-1602).
pub struct WizardState {
    pub machine: NewMachine,
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
}

#[allow(clippy::large_enum_variant)]
pub enum Modal {
    None,
    Wizard(WizardState),
    Delete(DeleteState),
    Settings(SettingsForm),
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
}

pub struct ManagerApp {
    pub settings: Settings,
    pub settings_path: Option<PathBuf>,
    /// Non-fatal startup problem (unreadable settings file) shown as a banner.
    pub startup_warning: Option<String>,
    pub cli: Result<PathBuf, String>,
    pub scan: Scan,
    pub scan_error: Option<String>,
    pub supervisor: Supervisor,
    pub pending: Vec<PendingInstall>,
    pub toasts: Vec<Toast>,
    pub modal: Modal,
    pub log_open: bool,
    pub log_selected: Option<TaskId>,
    pub log_follow: bool,
    scanner: Scanner,
    last_scan: Instant,
    screenshot: Option<ScreenshotJob>,
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

        let settings_path = settings::config_path().ok();
        let mut startup_warning = None;
        let mut settings = match &settings_path {
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
        };
        if let Some(dir) = startup.vm_dir {
            settings.vm_dir = dir;
        }
        if let Some(cli) = startup.entangled {
            settings.entangled_binary = Some(cli);
        }

        let mut app = Self {
            cli: launcher::locate_cli(&settings).map_err(|e| e.to_string()),
            settings,
            settings_path,
            startup_warning,
            scan: Scan::default(),
            scan_error: None,
            supervisor: Supervisor::new(Arc::clone(&waker)),
            pending: Vec::new(),
            toasts: Vec::new(),
            modal: Modal::None,
            log_open: false,
            log_selected: None,
            log_follow: true,
            scanner: Scanner::spawn(waker),
            last_scan: Instant::now() - SCAN_INTERVAL,
            screenshot: startup.screenshot.map(|path| ScreenshotJob {
                path,
                requested: false,
            }),
            frame: 0,
        };

        match startup.screenshot_view {
            ScreenshotView::Wizard => app.open_wizard(),
            ScreenshotView::Settings => app.open_settings(),
            ScreenshotView::Main => {}
        }
        app
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
                variant: self.settings.default_variant.clone(),
                automated: true,
                headless: self.settings.headless_install,
            },
            error: None,
        });
    }

    fn suggest_name(&self) -> String {
        for n in 1..=99 {
            let candidate = format!("debian-{n}");
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
        "debian-new".to_string()
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
        });
    }

    fn request_scan(&mut self, force: bool) {
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
        if launcher::bootstrap_kernel_missing(&cwd) {
            self.toast(
                ToastLevel::Warn,
                format!(
                    "no artifacts/bootstrap/vmlinuz under {} — a profile with relative \
                     kernel paths will fail to boot (Settings ▸ working directory)",
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
        let disk_path = self.settings.vm_dir.join(format!("{trimmed}.raw"));
        if profile_path.exists() || disk_path.exists() || self.vm(&trimmed).is_some() {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(format!("'{trimmed}' already exists in the VM directory"));
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
        if launcher::bootstrap_kernel_missing(&cwd) {
            if let Modal::Wizard(state) = &mut self.modal {
                state.error = Some(format!(
                    "no {} under {} — the installer boots the project kernel from there                      (build it with guest/bootstrap-kernel/build.sh, or point Settings ▸                      working directory at a tree that has it)",
                    launcher::BOOTSTRAP_KERNEL,
                    cwd.display()
                ));
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
        if self.supervisor.is_busy(name) {
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

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Refresh => self.request_scan(true),
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
            Action::CloseModal => self.modal = Modal::None,
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
        self.request_scan(false);
        self.collect_scan();
        self.collect_task_results();
        self.supervisor.prune(12);
        self.ensure_log_selection();
        self.expire_toasts(ctx.input(|i| i.time));

        let mut actions = Vec::new();
        ui::header::show(ctx, self, &mut actions);
        if self.log_open {
            ui::logpane::show(ctx, self, &mut actions);
        }
        ui::cards::show(ctx, self, &mut actions);
        ui::dialogs::show(ctx, self, &mut actions);
        ui::toasts::show(ctx, &self.toasts, &mut actions);

        for action in actions {
            self.apply(action, ctx);
        }

        self.handle_screenshot(ctx);
        // The logo and the status indicators are always in motion.
        ctx.request_repaint_after(FRAME_INTERVAL);
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
