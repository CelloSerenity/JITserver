#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod device;
mod device_services;
mod jit_memory;
mod script_catalog;
mod scripts;
mod targets;
mod ui;

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use device::{
    CheckStatus, DeviceConnection, DeviceInfo, DeviceStatus, DiskImageAction, WorkerActivity,
    WorkerCommand, WorkerEvent,
};
use device_services::RequiresScriptsStatus;
use eframe::egui;
use rfd::FileDialog;
use script_catalog::{BundledScript, script_name_key};
use targets::{AppInfo, PROCESS_REFRESH_INTERVAL, ProcessInfo};
use ui::JitAction;

fn main() -> eframe::Result {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1120.0, 760.0]),
        renderer: pick_renderer(),
        ..Default::default()
    };

    #[cfg(target_os = "macos")]
    {
        options.viewport.icon = Some(std::sync::Arc::new(egui::IconData::default()));
    }

    #[cfg(not(target_os = "macos"))]
    {
        let icon_bytes: &[u8] = include_bytes!("../Linux.png");
        let d = eframe::icon_data::from_png_bytes(icon_bytes).expect("The icon data must be valid");
        options.viewport.icon = Some(std::sync::Arc::new(d));
    }

    use eframe::egui_wgpu::WgpuSetup;
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        let defaults = std::sync::Arc::clone(&setup.device_descriptor);
        setup.device_descriptor = std::sync::Arc::new(move |adapter| {
            let mut descriptor = defaults(adapter);
            descriptor.required_limits = descriptor
                .required_limits
                .or_worse_values_from(&adapter.limits());
            descriptor
        });
    }

    eframe::run_native(
        &format!("JITserver v{}", env!("CARGO_PKG_VERSION")),
        options,
        Box::new(|cc| {
            ui::setup(&cc.egui_ctx);
            Ok(Box::new(DebuggerApp::new()))
        }),
    )
}

#[cfg(target_os = "macos")]
fn pick_renderer() -> eframe::Renderer {
    use eframe::wgpu;

    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::METAL;
    let instance = wgpu::Instance::new(descriptor);
    let metal_available = pollster::block_on(async {
        let Ok(adapter) = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
        else {
            return false;
        };
        adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .is_ok()
    });

    if metal_available {
        eframe::Renderer::Wgpu
    } else {
        eframe::Renderer::Glow
    }
}

#[cfg(not(target_os = "macos"))]
fn pick_renderer() -> eframe::Renderer {
    eframe::Renderer::Wgpu
}

#[derive(Clone, Debug)]
enum ScriptLocation {
    Bundled(&'static str),
    File(PathBuf),
}

#[derive(Clone, Debug)]
struct ScriptEntry {
    name: String,
    location: ScriptLocation,
    editable: bool,
}

struct DebuggerApp {
    command_tx: mpsc::Sender<WorkerCommand>,
    event_rx: mpsc::Receiver<WorkerEvent>,
    devices: Vec<DeviceInfo>,
    apps: Vec<AppInfo>,
    processes: Vec<ProcessInfo>,
    selected_device: Option<usize>,
    selected_app: Option<String>,
    selected_process: Option<u32>,
    app_filter: String,
    process_filter: String,
    manual_pid: String,
    scripts_dir: PathBuf,
    script_entries: Vec<ScriptEntry>,
    selected_script: Option<usize>,
    script_name: String,
    script_source: String,
    device_status: Option<Result<DeviceStatus, String>>,
    logs: Vec<String>,
    show_logs: bool,
    worker_activity: WorkerActivity,
    disk_image_pending: bool,
    attached: bool,
    script_running: bool,
    last_pid_refresh: Instant,
    has_successful_process_list: bool,
    has_logged_process_list: bool,
}

impl DebuggerApp {
    fn new() -> Self {
        let (command_tx, event_rx) = device::spawn_worker();

        let scripts_dir = custom_scripts_dir();
        let mut app = Self {
            command_tx,
            event_rx,
            devices: Vec::new(),
            apps: Vec::new(),
            processes: Vec::new(),
            selected_device: None,
            selected_app: None,
            selected_process: None,
            app_filter: String::new(),
            process_filter: String::new(),
            manual_pid: String::new(),
            scripts_dir,
            script_entries: Vec::new(),
            selected_script: None,
            script_name: String::new(),
            script_source: String::new(),
            device_status: None,
            logs: Vec::new(),
            show_logs: true,
            worker_activity: WorkerActivity::Idle,
            disk_image_pending: false,
            attached: false,
            script_running: false,
            last_pid_refresh: Instant::now(),
            has_successful_process_list: false,
            has_logged_process_list: false,
        };
        app.refresh_scripts();
        app.select_script_by_name(BundledScript::default().name());
        app
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                WorkerEvent::Activity(activity) => self.worker_activity = activity,
                WorkerEvent::Devices(result) => match result {
                    Ok(devices) => {
                        let previous_device = self
                            .selected_device
                            .and_then(|index| self.devices.get(index))
                            .cloned();
                        self.devices = devices;
                        self.selected_device = previous_device.as_ref().and_then(|previous| {
                            self.devices.iter().position(|device| {
                                device.udid == previous.udid
                                    && device.connection == previous.connection
                            })
                        });
                        if self.selected_device.is_none()
                            && let Some(index) = previous_device.as_ref().and_then(|previous| {
                                self.devices
                                    .iter()
                                    .position(|device| device.udid == previous.udid)
                            })
                        {
                            self.select_device(index, true);
                        }
                        if previous_device.is_some() && self.selected_device.is_none() {
                            self.apps.clear();
                            self.processes.clear();
                            self.selected_app = None;
                            self.selected_process = None;
                            self.device_status = None;
                            self.has_successful_process_list = false;
                            self.has_logged_process_list = false;
                        }
                        if self.selected_device.is_none() && self.devices.len() == 1 {
                            self.select_device(0, true);
                        }
                    }
                    Err(error) => self.logs.push(format!(
                        "Device refresh failed: {}",
                        format_device_refresh_error(&error)
                    )),
                },
                WorkerEvent::DeviceStatus {
                    udid,
                    connection,
                    status,
                } => {
                    if self
                        .selected_device
                        .and_then(|index| self.devices.get(index))
                        .is_some_and(|device| {
                            device.udid == udid && device.connection == connection
                        })
                    {
                        self.device_status = Some(status);
                    }
                }
                WorkerEvent::Apps(result) => match result {
                    Ok(apps) => {
                        self.logs
                            .push(format!("Loaded {} app(s) with get-task-allow", apps.len()));
                        self.apps = apps;
                        targets::filter_debuggable_processes(&mut self.processes, &self.apps);
                        self.selected_app = None;
                    }
                    Err(error) => self.logs.push(format!("App listing failed: {error}")),
                },
                WorkerEvent::Processes(result) => match result {
                    Ok(mut processes) => {
                        targets::filter_debuggable_processes(&mut processes, &self.apps);
                        let previous_pid = self.selected_process;
                        if !self.has_logged_process_list {
                            self.logs
                                .push(format!("Loaded {} matching process(es)", processes.len()));
                            self.has_logged_process_list = true;
                        }
                        self.has_successful_process_list = true;
                        self.last_pid_refresh = Instant::now();
                        self.selected_process = previous_pid
                            .filter(|pid| processes.iter().any(|process| process.pid == *pid));
                        self.processes = processes;
                    }
                    Err(error) => {
                        self.has_successful_process_list = false;
                        self.logs.push(format!("Process listing failed: {error}"));
                    }
                },
                WorkerEvent::DeveloperDiskImage {
                    udid,
                    connection,
                    action,
                    result,
                } => {
                    self.disk_image_pending = false;
                    let selected = self
                        .selected_device
                        .and_then(|index| self.devices.get(index))
                        .is_some_and(|device| device.udid == udid && device.connection == connection);
                    let (disk_image_status, log) = match (action, result) {
                        (DiskImageAction::Mount, Ok(())) => (
                            Some(CheckStatus::Success),
                            "Developer Disk Image mounted".to_string(),
                        ),
                        (DiskImageAction::Mount, Err(error)) => {
                            let log = format!("Developer Disk Image mount failed: {error}");
                            (Some(CheckStatus::Failed(error)), log)
                        }
                        (DiskImageAction::Unmount, Ok(())) => (
                            Some(CheckStatus::Disabled),
                            "Developer Disk Image unmounted".to_string(),
                        ),
                        (DiskImageAction::Unmount, Err(error)) => (
                            None,
                            format!("Developer Disk Image unmount failed: {error}"),
                        ),
                    };
                    self.logs.push(log);
                    if selected
                        && let Some(new_status) = disk_image_status
                        && let Some(Ok(status)) = self.device_status.as_mut()
                    {
                        let mounted = matches!(new_status, CheckStatus::Success);
                        status.developer_disk_image = new_status;
                        if mounted {
                            self.refresh_processes();
                        }
                    }
                }
                WorkerEvent::Launched(result) => match result {
                    Ok(pid) => {
                        self.manual_pid = pid.to_string();
                        self.selected_process = Some(pid);
                        self.logs.push(format!("Launched process pid={pid}"));
                    }
                    Err(error) => self.logs.push(format!("Launch failed: {error}")),
                },
                WorkerEvent::Attached(result) => match result {
                    Ok(response) => {
                        self.attached = true;
                        self.logs.push(format!("Attached: {response}"));
                    }
                    Err(error) => {
                        self.script_running = false;
                        self.logs.push(format!("Attach failed: {error}"));
                    }
                },
                WorkerEvent::DebugResponse(result) => match result {
                    Ok(response) => {
                        self.attached = false;
                        self.logs.push(format!("debugserver: {response}"));
                    }
                    Err(error) => self.logs.push(format!("Command failed: {error}")),
                },
                WorkerEvent::ScriptFinished(result) => match result {
                    Ok(()) => {
                        self.script_running = false;
                        self.logs.push("Script finished".to_string());
                    }
                    Err(error) => {
                        self.script_running = false;
                        self.logs.push(format!("Script failed: {error}"));
                    }
                },
                WorkerEvent::Log(message) => self.logs.push(message),
            }
        }
    }

    fn selected_udid(&self) -> Option<String> {
        self.selected_device
            .and_then(|index| self.devices.get(index))
            .map(|device| device.udid.clone())
    }

    fn refresh_scripts(&mut self) {
        let previously_selected = self
            .selected_script
            .and_then(|index| self.script_entries.get(index))
            .map(|entry| entry.name.clone());
        match read_script_entries(&self.scripts_dir) {
            Ok(entries) => {
                self.script_entries = entries;
                self.selected_script = previously_selected.and_then(|name| {
                    self.script_entries
                        .iter()
                        .position(|entry| entry.name == name)
                });

                if self.selected_script.is_none() && !self.script_entries.is_empty() {
                    self.load_script(0);
                } else if let Some(index) = self.selected_script {
                    self.load_script(index);
                }
            }
            Err(error) => self.logs.push(format!("Failed to load scripts: {error}")),
        }
    }

    fn load_script(&mut self, index: usize) {
        let Some(entry) = self.script_entries.get(index).cloned() else {
            return;
        };
        match read_script_source(&entry) {
            Ok(source) => {
                self.selected_script = Some(index);
                self.script_name = entry.name;
                self.script_source = source;
            }
            Err(error) => self
                .logs
                .push(format!("Failed to read script {}: {error}", entry.name)),
        }
    }

    fn select_script_by_name(&mut self, script_name: &str) -> bool {
        let Some(index) = self
            .script_entries
            .iter()
            .position(|entry| entry.name.eq_ignore_ascii_case(script_name))
        else {
            return false;
        };
        self.load_script(index);
        true
    }

    fn selected_script_entry(&self) -> Option<&ScriptEntry> {
        self.selected_script
            .and_then(|index| self.script_entries.get(index))
    }

    fn selected_script_is_editable(&self) -> bool {
        self.selected_script_entry()
            .map(|entry| entry.editable)
            .unwrap_or(false)
    }

    fn has_script(&self) -> bool {
        !self.script_name.trim().is_empty()
    }

    fn current_script(&self) -> Option<(String, String)> {
        let name = self.script_name.trim();
        if !self.has_script() {
            return None;
        }
        Some((name.to_string(), self.script_source.clone()))
    }

    fn import_script(&mut self) {
        if let Err(error) = fs::create_dir_all(&self.scripts_dir) {
            self.logs.push(format!(
                "Failed to create scripts directory {}: {error}",
                self.scripts_dir.display()
            ));
            return;
        }

        let Some(source_path) = FileDialog::new()
            .add_filter("JavaScript", &["js"])
            .set_title("Import Script")
            .pick_file()
        else {
            return;
        };

        let source = match fs::read_to_string(&source_path) {
            Ok(source) => source,
            Err(error) => {
                self.logs.push(format!(
                    "Failed to read imported script {}: {error}",
                    source_path.display()
                ));
                return;
            }
        };

        let file_name = match source_path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_string(),
            None => {
                self.logs
                    .push("Imported script must have a valid file name".to_string());
                return;
            }
        };

        if is_built_in_script_name(&file_name) {
            self.logs.push(format!(
                "Cannot import {} because that name is reserved for a built-in script",
                file_name
            ));
            return;
        }

        let target_path = self.scripts_dir.join(&file_name);
        if target_path.exists() {
            self.logs.push(format!(
                "Cannot import {} because a script with that name already exists",
                file_name
            ));
            return;
        }

        if let Err(error) = fs::write(&target_path, source) {
            self.logs.push(format!(
                "Failed to import script to {}: {error}",
                target_path.display()
            ));
            return;
        }

        self.logs.push(format!(
            "Imported script {}",
            target_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("custom.js")
        ));
        self.refresh_scripts();
        self.selected_script = self.script_entries.iter().position(
            |entry| matches!(&entry.location, ScriptLocation::File(path) if *path == target_path),
        );
        if let Some(index) = self.selected_script {
            self.load_script(index);
        }
    }

    fn delete_selected_script(&mut self) {
        let Some(entry) = self.selected_script_entry().cloned() else {
            self.logs.push("Select a script first".to_string());
            return;
        };

        if !entry.editable {
            self.logs
                .push(format!("Built-in script {} cannot be deleted", entry.name));
            return;
        }

        let path = match &entry.location {
            ScriptLocation::File(path) => path,
            ScriptLocation::Bundled(_) => {
                self.logs
                    .push(format!("Built-in script {} cannot be deleted", entry.name));
                return;
            }
        };

        if let Err(error) = fs::remove_file(path) {
            self.logs.push(format!(
                "Failed to delete script {}: {error}",
                path.display()
            ));
            return;
        }

        self.logs.push(format!("Deleted script {}", entry.name));
        self.selected_script = None;
        self.refresh_scripts();
    }

    fn select_device(&mut self, index: usize, background: bool) {
        self.selected_device = Some(index);
        self.logs.clear();
        self.apps.clear();
        self.processes.clear();
        self.selected_app = None;
        self.selected_process = None;
        self.device_status = Some(Ok(DeviceStatus::default()));
        self.has_successful_process_list = false;
        self.has_logged_process_list = false;
        self.last_pid_refresh = Instant::now();
        if let Some(device) = self.devices.get(index) {
            self.send(WorkerCommand::ListApps {
                udid: device.udid.clone(),
                connection: device.connection,
                background,
            });
        }
    }

    fn selected_device_label(&self) -> String {
        self.selected_device
            .and_then(|index| self.devices.get(index))
            .map(device_label)
            .unwrap_or_else(|| "No device".to_string())
    }

    fn send(&mut self, command: WorkerCommand) {
        let manages_disk_image = matches!(&command, WorkerCommand::ManageDeveloperDiskImage { .. });
        if matches!(
            &command,
            WorkerCommand::LaunchAndAttachAndRunScript { .. } | WorkerCommand::AttachAndRun { .. }
        ) {
            self.script_running = true;
        }
        if matches!(&command, WorkerCommand::Stop) {
            self.script_running = false;
            self.attached = false;
        }
        if self.command_tx.send(command).is_err() {
            self.script_running = false;
            self.logs.push("Worker is unavailable".to_string());
        } else if manages_disk_image {
            self.disk_image_pending = true;
        }
    }

    fn refresh_selected_device_lists(&mut self) {
        if let Some(device) = self.selected_device.and_then(|index| self.devices.get(index)) {
            self.send(WorkerCommand::ListApps {
                udid: device.udid.clone(),
                connection: device.connection,
                background: false,
            });
        } else {
            self.logs.push("Select a device first".to_string());
        }
    }

    fn refresh_processes(&mut self) {
        if let Some(udid) = self.selected_udid() {
            self.send(WorkerCommand::ListProcesses { udid });
            self.last_pid_refresh = Instant::now();
        }
    }

    fn maybe_refresh_processes(&mut self) {
        if !matches!(self.worker_activity, WorkerActivity::Idle)
            || self.selected_device.is_none()
            || !self.has_successful_process_list
        {
            return;
        }

        if self.last_pid_refresh.elapsed() >= PROCESS_REFRESH_INTERVAL {
            self.refresh_processes();
        }
    }

    fn jit_actions(&self, ui: &mut egui::Ui, has_target: bool) -> Option<JitAction> {
        let requires_scripts = matches!(
            &self.device_status,
            Some(Ok(status)) if matches!(status.requires_scripts, RequiresScriptsStatus::Yes)
        );
        let can_stop = self.attached || self.script_running;
        ui::device_jit_actions(ui, requires_scripts, has_target, self.has_script(), can_stop, false)
    }

    fn auto_select_script_for_target(&mut self, target_name: &str) {
        let Some(script_name) = BundledScript::recommended(target_name) else {
            return;
        };

        if self.select_script_by_name(script_name.name()) {
            self.logs.push(format!(
                "Auto-selected script {} for {}",
                script_name.name(), target_name
            ));
        }
    }

    fn maybe_log_livecontainer_selected(&mut self, target_name: &str) {
        if target_name.eq_ignore_ascii_case("LiveContainer") {
            self.logs.push(
                "LiveContainer selected. Ensure you select the corresponding script for your containerized app."
                    .to_string(),
            );
        }
    }
}

impl eframe::App for DebuggerApp {
    #[cfg(target_os = "macos")]
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|input| input.viewport().close_requested()) {
            // Keep the native view alive until AppKit quits to avoid the Touch Bar observer crash.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            dispatch2::DispatchQueue::main().exec_async(|| {
                let mtm = objc2::MainThreadMarker::new().expect("running on the main queue");
                objc2_app_kit::NSApplication::sharedApplication(mtm).terminate(None);
            });
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.maybe_refresh_processes();

        let mut chosen = None;
        let selected_label = self.selected_device_label();
        ui::device_bar(ui, &mut self.show_logs, |ui| {
            egui::ComboBox::from_id_salt("device_selector")
                .selected_text(selected_label)
                .width(280.0)
                .show_ui(ui, |ui| {
                    if self.devices.is_empty() {
                        ui.label("No devices found");
                    }
                    for (index, device) in self.devices.iter().enumerate() {
                        if ui
                            .selectable_label(
                                self.selected_device == Some(index),
                                device_label(device),
                            )
                            .clicked()
                        {
                            chosen = Some(index);
                        }
                    }
                });
        });
        if let Some(index) = chosen {
            self.select_device(index, false);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            let devices_height = 150.0;
            let top_spacing = 8.0;

            ui::workspace(ui, 960.0, |columns| {
                        columns[0].vertical(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), devices_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let device = self.selected_device.and_then(|index| self.devices.get(index));
                            ui.horizontal(|ui| {
                                ui.heading(device.map_or("Device Status", |device| device.name.as_str()));
                                ui::debug_indicators(ui, self.attached, self.script_running);
                                if matches!(self.worker_activity, WorkerActivity::Foreground { started }
                                    if started.elapsed() >= Duration::from_millis(250))
                                {
                                    ui.spinner();
                                }
                            });

                            if let Some(device) = device {
                                ui::device_identity(
                                    ui,
                                    device.model.as_deref(),
                                    device.version.as_deref(),
                                    device.build.as_deref(),
                                    Some(&device.udid),
                                );
                                if let Some(version) = &device.version {
                                    ui::device_version_warning(ui, version);
                                }
                            }
                            if let (Some(status), Some(device)) = (&self.device_status, device) {
                                let can_manage = !matches!(self.worker_activity, WorkerActivity::Foreground { .. })
                                    && !self.disk_image_pending
                                    && !self.attached
                                    && !self.script_running;
                                if let Some(action) = render_device_status(
                                    ui, status, device.connection == DeviceConnection::Usb, can_manage,
                                ) {
                                    self.send(WorkerCommand::ManageDeveloperDiskImage {
                                        udid: device.udid.clone(),
                                        connection: device.connection,
                                        action,
                                    });
                                }
                            }
                        },
                    );

                    ui.separator();
                    let lower_left_height = (ui.available_height() - top_spacing).max(240.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), lower_left_height),
                        egui::Layout::left_to_right(egui::Align::Min),
                        |ui| {
                            let left_width = (ui.available_width() - 8.0).max(0.0) * 0.5;
                            ui.allocate_ui_with_layout(
                                egui::vec2(left_width, lower_left_height),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    if ui::list_heading(ui, "Apps", true) {
                                        self.refresh_selected_device_lists();
                                    }
                                    let list_height = (ui.available_height() - 92.0).max(120.0);
                                    if let Some(app) = ui::app_list(ui, &self.apps, self.selected_app.as_deref(), &mut self.app_filter, list_height) {
                                        let name = app.name.clone();
                                        self.selected_app = Some(app.bundle_id.clone());
                                        self.auto_select_script_for_target(&name);
                                        self.maybe_log_livecontainer_selected(&name);
                                    }
                                    let target = self.selected_udid().zip(self.selected_app.clone());
                                    match self.jit_actions(ui, target.is_some()) {
                                        Some(JitAction::Enable) => {
                                            if let Some((udid, bundle_id)) = target {
                                                self.send(WorkerCommand::LaunchAndAttach { udid, bundle_id });
                                            }
                                        }
                                        Some(JitAction::RunScript) => {
                                            if let (Some((udid, bundle_id)), Some((name, source))) = (target, self.current_script()) {
                                                self.send(WorkerCommand::LaunchAndAttachAndRunScript { udid, bundle_id, name, source });
                                            }
                                        }
                                        Some(JitAction::Stop) => self.send(WorkerCommand::Stop),
                                        None => {}
                                    }
                                },
                            );
                            ui.separator();
                            let pid_width = ui.available_width().max(0.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(pid_width, lower_left_height),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    if ui::list_heading(ui, "PID", true) {
                                        self.refresh_selected_device_lists();
                                    }
                                    let list_height = (ui.available_height() - 115.0).max(80.0);
                                    if let Some(process) = ui::process_list(ui, &self.processes, self.selected_process, &mut self.process_filter, list_height) {
                                        let name = process.name.clone();
                                        self.selected_process = Some(process.pid);
                                        self.manual_pid = process.pid.to_string();
                                        self.auto_select_script_for_target(&name);
                                        self.maybe_log_livecontainer_selected(&name);
                                    }
                                    if ui::pid_field(ui, &mut self.manual_pid) {
                                        self.selected_process = ui::parse_pid(&self.manual_pid);
                                    }
                                    let target = self.selected_udid().zip(ui::parse_pid(&self.manual_pid));
                                    match self.jit_actions(ui, target.is_some()) {
                                        Some(JitAction::Enable) => {
                                            if let Some((udid, pid)) = target {
                                                self.send(WorkerCommand::Attach { udid, pid });
                                            }
                                        }
                                        Some(JitAction::RunScript) => {
                                            if let (Some((udid, pid)), Some((name, source))) = (target, self.current_script()) {
                                                self.send(WorkerCommand::AttachAndRun { udid, pid, name, source });
                                            }
                                        }
                                        Some(JitAction::Stop) => self.send(WorkerCommand::Stop),
                                        None => {}
                                    }
                                },
                            );
                        },
                    );
                });

                        columns[1].vertical(|ui| {
                        ui::script_section(ui, self.show_logs, |ui| {
                            ui.horizontal(|ui| {
                                ui.heading("Scripts");
                                if ui
                                    .add_enabled(!self.script_running, egui::Button::new("Import Script"))
                                    .clicked()
                                {
                                    self.import_script();
                                }
                                if ui
                                    .add_enabled(
                                        !self.script_running && self.selected_script_is_editable(),
                                        egui::Button::new("Delete Script"),
                                    )
                                    .clicked()
                                {
                                    self.delete_selected_script();
                                }
                            });
                            ui.label("Select a bundled script or import your own .js file.");
                            ui.label("Scripts are required to enable JIT on TXM (A15+/M2+ chip) devices running iOS 26+.");

                            if let Some(index) = ui::script_picker(ui, !self.script_running, self.selected_script, self.script_entries.iter().map(|entry| entry.name.as_str())) {
                                self.load_script(index);
                            }
                            if self.script_running {
                                ui.small("Script switching is disabled while a script is running.");
                            }
                            if self.selected_script_is_editable() {
                                ui.small("Imported scripts can be deleted.");
                            } else {
                                ui.small("Built-in scripts cannot be deleted.");
                            }
                        });
                        if self.show_logs {
                            ui::logs(ui, &mut self.logs);
                        }
                        });
            });
        });

        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn device_label(device: &DeviceInfo) -> String {
    let transport = match device.connection {
        DeviceConnection::Network => "Wi-Fi",
        connection => connection.label(),
    };
    format!("{} · {transport}", device.name)
}

fn render_device_check(ui: &mut egui::Ui, label: &str, status: &CheckStatus, disabled: &str) {
    ui::device_check(ui, label, |ui| match status {
        CheckStatus::Checking | CheckStatus::Downloading => {
            ui.weak("Checking");
        }
        CheckStatus::Success => ui::device_check_value(ui, Some(true), disabled),
        CheckStatus::Disabled => ui::device_check_value(ui, Some(false), disabled),
        CheckStatus::Failed(error) => {
            ui.label(egui::RichText::new("Failed").color(ui.visuals().error_fg_color))
                .on_hover_text(error);
        }
    });
}

fn render_device_status(
    ui: &mut egui::Ui,
    status: &Result<DeviceStatus, String>,
    is_usb: bool,
    can_manage: bool,
) -> Option<DiskImageAction> {
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            ui.label(egui::RichText::new(error).color(ui.visuals().error_fg_color));
            return None;
        }
    };
    if is_usb {
        render_device_check(ui, "Wireless debugging", &status.wireless_debugging, "Off");
    }
    render_device_check(ui, "Developer mode", &status.developer_mode, "Disabled");
    let mut disk_image_action = None;
    ui::device_check(ui, "Developer disk image", |ui| {
        match &status.developer_disk_image {
            CheckStatus::Checking => {
                ui.label("Checking");
            }
            CheckStatus::Downloading => {
                ui.label("Downloading");
            }
            CheckStatus::Success => {
                if ui::mounted_ddi_label(ui) && can_manage {
                    disk_image_action = Some(DiskImageAction::Unmount);
                }
            }
            CheckStatus::Disabled => {
                ui.label(egui::RichText::new("Unmounted").color(ui.visuals().warn_fg_color));
            }
            CheckStatus::Failed(e) => {
                ui.label(
                    egui::RichText::new("Mount failed").color(ui.visuals().error_fg_color),
                ).on_hover_text(e);
            }
        }
        if matches!(
            status.developer_disk_image,
            CheckStatus::Disabled | CheckStatus::Failed(_)
        ) && ui
            .add_enabled(
                can_manage && !matches!(status.developer_mode, CheckStatus::Disabled),
                egui::Button::new("Mount"),
            )
            .clicked()
        {
            disk_image_action = Some(DiskImageAction::Mount);
        }
    });
    ui::requires_scripts_status(ui, &status.requires_scripts);
    disk_image_action
}


fn read_script_entries(dir: &Path) -> std::io::Result<Vec<ScriptEntry>> {
    let mut entries = BundledScript::ALL
        .into_iter()
        .map(|script| ScriptEntry {
            editable: false,
            name: script.name().to_owned(),
            location: ScriptLocation::Bundled(script.source()),
        })
        .collect::<Vec<_>>();

    if dir.exists() {
        entries.extend(
            fs::read_dir(dir)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("js"))
                .map(|path| {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("script.js")
                        .to_string();
                    ScriptEntry {
                        editable: !is_built_in_script_name(&name),
                        name,
                        location: ScriptLocation::File(path),
                    }
                }),
        );
    }

    let mut entries = entries;
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(entries)
}

fn read_script_source(entry: &ScriptEntry) -> std::io::Result<String> {
    match &entry.location {
        ScriptLocation::Bundled(source) => Ok((*source).to_string()),
        ScriptLocation::File(path) => fs::read_to_string(path),
    }
}

fn custom_scripts_dir() -> PathBuf {
    app_support_dir().join("scripts")
}

fn format_device_refresh_error(error: &str) -> String {
    #[allow(unused_mut)]
    let mut message = error.to_string();
    if error.contains("device socket io failed") {
        #[cfg(target_os = "windows")]
        {
            message.push_str(". Make sure you have Apple Devices/iTunes installed and that you have accepted Apple's Terms of Service.");
        }
        #[cfg(target_os = "linux")]
        {
            message.push_str(". Make sure you have usbmuxd installed and running.");
        }
    }
    message
}

fn app_support_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("JITserver");
        }
    }

    if cfg!(target_os = "windows") {
        if let Some(appdata) = env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("JITserver");
        }
    }

    if let Some(xdg_data_home) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg_data_home).join("JITserver");
    }

    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("JITserver");
    }

    PathBuf::from(".jitserver")
}

fn is_built_in_script_name(name: &str) -> bool {
    let key = script_name_key(name);
    BundledScript::ALL.into_iter().any(|script| script_name_key(script.name()) == key)
}
