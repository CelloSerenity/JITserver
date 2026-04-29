#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod device;
mod scripts;

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use device::{
    AppInfo, CheckStatus, DeviceInfo, DeviceStatus, ProcessInfo, WorkerCommand, WorkerEvent,
};
use eframe::egui;
use rfd::FileDialog;

fn main() -> eframe::Result {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1120.0, 760.0]),
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

    eframe::run_native(
        &format!("JITserver v{}", env!("CARGO_PKG_VERSION")),
        options,
        Box::new(|_cc| Ok(Box::new(DebuggerApp::new()))),
    )
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
    selected_app: Option<usize>,
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
    busy: bool,
    attached: bool,
    script_running: bool,
    last_pid_refresh: Instant,
    has_successful_process_list: bool,
    has_logged_process_list: bool,
}

impl DebuggerApp {
    fn new() -> Self {
        let (command_tx, event_rx) = device::spawn_worker();
        let _ = command_tx.send(WorkerCommand::RefreshDevices);

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
            busy: false,
            attached: false,
            script_running: false,
            last_pid_refresh: Instant::now(),
            has_successful_process_list: false,
            has_logged_process_list: false,
        };
        app.refresh_scripts();
        app.select_script_by_name(DEFAULT_SCRIPT_NAME);
        app
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                WorkerEvent::Busy(busy) => self.busy = busy,
                WorkerEvent::Devices(result) => match result {
                    Ok(devices) => {
                        let previous_udid = self.selected_udid();
                        self.devices = devices;
                        self.selected_device = previous_udid.as_ref().and_then(|udid| {
                            self.devices.iter().position(|device| &device.udid == udid)
                        });
                        if self.selected_device.is_none() && !self.devices.is_empty() {
                            self.select_device(0);
                        }
                    }
                    Err(error) => self.logs.push(format!(
                        "Device refresh failed: {}",
                        format_device_refresh_error(&error)
                    )),
                },
                WorkerEvent::DeviceStatus(status) => {
                    self.device_status = Some(Ok(status));
                }
                WorkerEvent::Apps(result) => match result {
                    Ok(apps) => {
                        self.logs
                            .push(format!("Loaded {} app(s) with get-task-allow", apps.len()));
                        self.apps = apps;
                        self.selected_app = None;
                    }
                    Err(error) => self.logs.push(format!("App listing failed: {error}")),
                },
                WorkerEvent::Processes(result) => match result {
                    Ok(processes) => {
                        let previous_pid = self.selected_process;
                        if !self.has_logged_process_list {
                            self.logs
                                .push(format!("Loaded {} running process(es)", processes.len()));
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

    fn selected_bundle_id(&self) -> Option<String> {
        self.selected_app
            .and_then(|index| self.filtered_apps().get(index).cloned())
            .map(|app| app.bundle_id)
    }

    fn filtered_apps(&self) -> Vec<AppInfo> {
        let filter = self.app_filter.trim().to_lowercase();
        if filter.is_empty() {
            return self.apps.clone();
        }

        self.apps
            .iter()
            .filter(|app| {
                app.name.to_lowercase().contains(&filter)
                    || app.bundle_id.to_lowercase().contains(&filter)
            })
            .cloned()
            .collect()
    }

    fn filtered_processes(&self) -> Vec<ProcessInfo> {
        let filter = self.process_filter.trim().to_lowercase();
        if filter.is_empty() {
            return self.processes.clone();
        }

        self.processes
            .iter()
            .filter(|process| {
                process.name.to_lowercase().contains(&filter)
                    || process.pid.to_string().contains(&filter)
            })
            .cloned()
            .collect()
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

    fn current_script(&self) -> Option<(String, String)> {
        let name = self.script_name.trim();
        if name.is_empty() {
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

    fn select_device(&mut self, index: usize) {
        self.selected_device = Some(index);
        self.apps.clear();
        self.processes.clear();
        self.selected_app = None;
        self.selected_process = None;
        self.device_status = None;
        self.has_successful_process_list = false;
        self.has_logged_process_list = false;
        self.last_pid_refresh = Instant::now();
        if let Some(udid) = self.selected_udid() {
            self.send(WorkerCommand::ListApps { udid });
        }
    }

    fn selected_device_label(&self) -> String {
        self.selected_device
            .and_then(|index| self.devices.get(index))
            .map(device_label)
            .unwrap_or_else(|| "Select a device".to_string())
    }

    fn send(&mut self, command: WorkerCommand) {
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
        }
    }

    fn refresh_selected_device_lists(&mut self) {
        if let Some(udid) = self.selected_udid() {
            self.send(WorkerCommand::ListApps { udid });
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
        if self.busy || self.selected_device.is_none() || !self.has_successful_process_list {
            return;
        }

        if self.last_pid_refresh.elapsed() >= Duration::from_secs(3) {
            self.refresh_processes();
        }
    }

    fn auto_select_script_for_target(&mut self, target_name: &str) {
        let Some(script_name) = recommended_script_for_target(target_name) else {
            return;
        };

        if self.select_script_by_name(script_name) {
            self.logs.push(format!(
                "Auto-selected script {} for {}",
                script_name, target_name
            ));
        }
    }

    fn render_status_indicator(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, color);
            ui.label(text);
        });
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.maybe_refresh_processes();

        egui::CentralPanel::default().show(ctx, |ui| {
            let devices_height = 180.0;
            let log_height = 180.0;
            let top_spacing = 8.0;

            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_min_width(960.0);
                    ui.columns(2, |columns| {
                        columns[0].vertical(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), devices_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.heading("Devices");
                                if ui.button("Refresh Devices").clicked() {
                                    self.send(WorkerCommand::RefreshDevices);
                                }
                                Self::render_status_indicator(
                                    ui,
                                    if self.attached {
                                        egui::Color32::from_rgb(80, 200, 120)
                                    } else {
                                        egui::Color32::from_rgb(140, 140, 140)
                                    },
                                    "Attached",
                                );
                                Self::render_status_indicator(
                                    ui,
                                    if self.script_running {
                                        egui::Color32::from_rgb(255, 196, 64)
                                    } else {
                                        egui::Color32::from_rgb(140, 140, 140)
                                    },
                                    "Script Running",
                                );
                                if self.busy {
                                    ui.spinner();
                                }
                            });
                            egui::ComboBox::from_id_salt("device_selector")
                                .selected_text(self.selected_device_label())
                                .show_ui(ui, |ui| {
                                    let mut chosen = None;
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
                                    if let Some(index) = chosen {
                                        self.select_device(index);
                                    }
                                });

                            if let Some(status) = &self.device_status {
                                render_device_status(ui, status);
                            } else if self.selected_device.is_some() {
                                ui.label("Checking wireless debugging, Developer Mode, and DDI...");
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
                                    ui.horizontal(|ui| {
                                        ui.heading("Apps");
                                        if ui.button("Refresh").clicked() {
                                            self.refresh_selected_device_lists();
                                        }
                                    });
                                    ui.label("Only apps with get-task-allow are shown.");
                                    ui.horizontal(|ui| {
                                        ui.label("Filter");
                                        ui.text_edit_singleline(&mut self.app_filter);
                                    });

                                    let actions_height = 34.0;
                                    let apps_list_height = (ui.available_height() - actions_height).max(120.0);
                                    egui::ScrollArea::vertical()
                                        .id_salt("apps")
                                        .max_height(apps_list_height)
                                        .show(ui, |ui| {
                                            let apps = self.filtered_apps();
                                            for (index, app) in apps.iter().enumerate() {
                                                let selected = self.selected_app == Some(index);
                                                if ui.selectable_label(selected, &app.name).clicked() {
                                                    self.selected_app = Some(index);
                                                    self.auto_select_script_for_target(&app.name);
                                                    self.maybe_log_livecontainer_selected(&app.name);
                                                }
                                                ui.small(&app.bundle_id);
                                                ui.add_space(4.0);
                                            }
                                        });

                                    let can_launch = self.selected_udid().is_some()
                                        && self.selected_bundle_id().is_some();
                                    let can_launch_and_run =
                                        can_launch && self.current_script().is_some();
                                    ui.horizontal_wrapped(|ui| {
                                        if ui
                                            .add_enabled(
                                                can_launch,
                                                egui::Button::new("Enable JIT"),
                                            )
                                            .clicked()
                                        {
                                            if let (Some(udid), Some(bundle_id)) =
                                                (self.selected_udid(), self.selected_bundle_id())
                                            {
                                                self.send(WorkerCommand::LaunchAndAttach {
                                                    udid,
                                                    bundle_id,
                                                });
                                            }
                                        }
                                        if ui
                                            .add_enabled(
                                                can_launch_and_run,
                                                egui::Button::new("Enable JIT via Script"),
                                            )
                                            .clicked()
                                        {
                                            if let (Some(udid), Some(bundle_id), Some((name, source))) = (
                                                self.selected_udid(),
                                                self.selected_bundle_id(),
                                                self.current_script(),
                                            ) {
                                                self.send(WorkerCommand::LaunchAndAttachAndRunScript {
                                                    udid,
                                                    bundle_id,
                                                    name,
                                                    source,
                                                });
                                            }
                                        }
                                        if ui
                                            .add_enabled(
                                                self.attached || self.script_running,
                                                egui::Button::new("Stop"),
                                            )
                                            .clicked()
                                        {
                                            self.send(WorkerCommand::Stop);
                                        }
                                    });
                                },
                            );
                            ui.separator();
                            let pid_width = ui.available_width().max(0.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(pid_width, lower_left_height),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.horizontal(|ui| {
                                        ui.heading("PID");
                                        if ui.button("Refresh").clicked() {
                                            self.refresh_selected_device_lists();
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Filter");
                                        ui.text_edit_singleline(&mut self.process_filter);
                                    });
                                    let process_list_height = (ui.available_height() - 84.0).max(80.0);
                                    egui::ScrollArea::vertical()
                                        .id_salt("processes")
                                        .max_height(process_list_height)
                                        .show(ui, |ui| {
                                            for process in self.filtered_processes() {
                                                let selected = self.selected_process == Some(process.pid);
                                                if ui
                                                    .selectable_label(
                                                        selected,
                                                        format!("{} ({})", process.name, process.pid),
                                                    )
                                                    .clicked()
                                                {
                                                    self.selected_process = Some(process.pid);
                                                    self.manual_pid = process.pid.to_string();
                                                    self.auto_select_script_for_target(&process.name);
                                                    self.maybe_log_livecontainer_selected(&process.name);
                                                }
                                            }
                                        });

                                    ui.horizontal(|ui| {
                                        ui.label("PID");
                                        let response = ui.text_edit_singleline(&mut self.manual_pid);
                                        if response.changed() {
                                            self.selected_process =
                                                self.manual_pid.trim().parse::<u32>().ok();
                                        }
                                    });

                                    let parsed_pid = self.manual_pid.trim().parse::<u32>().ok();
                                    ui.horizontal_wrapped(|ui| {
                                        if ui
                                            .add_enabled(
                                                self.selected_udid().is_some() && parsed_pid.is_some(),
                                                egui::Button::new("Enable JIT"),
                                            )
                                            .clicked()
                                        {
                                            match (self.selected_udid(), parsed_pid) {
                                                (Some(udid), Some(pid)) => {
                                                    self.send(WorkerCommand::Attach { udid, pid })
                                                }
                                                (None, _) => {
                                                    self.logs.push("Select a device first".to_string())
                                                }
                                                (_, None) => self.logs.push("Enter a numeric PID".to_string()),
                                            }
                                        }

                                        if ui
                                            .add_enabled(
                                                self.selected_udid().is_some()
                                                    && parsed_pid.is_some()
                                                    && self.current_script().is_some(),
                                                egui::Button::new("Enable JIT via Script"),
                                            )
                                            .clicked()
                                        {
                                            match (self.selected_udid(), parsed_pid, self.current_script()) {
                                                (Some(udid), Some(pid), Some((name, source))) => {
                                                    self.send(WorkerCommand::AttachAndRun {
                                                        udid,
                                                        pid,
                                                        name,
                                                        source,
                                                    });
                                                }
                                                (None, _, _) => {
                                                    self.logs.push("Select a device first".to_string())
                                                }
                                                (_, None, _) => {
                                                    self.logs.push("Enter a numeric PID".to_string())
                                                }
                                                (_, _, None) => self
                                                    .logs
                                                    .push("Select or name a script first".to_string()),
                                            }
                                        }

                                        if ui
                                            .add_enabled(
                                                self.attached || self.script_running,
                                                egui::Button::new("Stop"),
                                            )
                                            .clicked()
                                        {
                                            self.send(WorkerCommand::Stop);
                                        }
                                    });
                                },
                            );
                        },
                    );
                });

                        columns[1].vertical(|ui| {
                    let script_height = (ui.available_height() - log_height - 12.0).clamp(120.0, 340.0);

                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), script_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
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

                            ui.add_enabled_ui(!self.script_running, |ui| {
                                let popup_max_height =
                                    (ui.ctx().content_rect().bottom() - ui.next_widget_position().y
                                        - 16.0)
                                        .max(120.0);
                                egui::ComboBox::from_label("Selected Script")
                                    .height(popup_max_height)
                                    .selected_text(
                                        self.selected_script
                                            .and_then(|index| self.script_entries.get(index))
                                            .map(|entry| entry.name.clone())
                                            .unwrap_or_else(|| "No script selected".to_string()),
                                    )
                                    .show_ui(ui, |ui| {
                                        let mut chosen = None;
                                        for (index, entry) in self.script_entries.iter().enumerate() {
                                            if ui
                                                .selectable_label(
                                                    self.selected_script == Some(index),
                                                    &entry.name,
                                                )
                                                .clicked()
                                            {
                                                chosen = Some(index);
                                            }
                                        }
                                        if let Some(index) = chosen {
                                            self.load_script(index);
                                        }
                                    });
                            });
                            if self.script_running {
                                ui.small("Script switching is disabled while a script is running.");
                            }
                            if self.selected_script_is_editable() {
                                ui.small("Imported scripts can be deleted.");
                            } else {
                                ui.small("Built-in scripts cannot be deleted.");
                            }
                        },
                    );

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.heading("Log");
                        if ui.button("Copy Logs").clicked() {
                            ui.ctx().copy_text(self.logs.join("\n"));
                            self.logs.push("Copied logs to clipboard".to_string());
                        }
                        if ui.button("Clear").clicked() {
                            self.logs.clear();
                        }
                    });
                    let log_text = self.logs.join("\n");
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_min_height(log_height);
                        egui::ScrollArea::vertical()
                            .id_salt("logs")
                            .max_height(log_height)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(log_text).monospace())
                                            .selectable(true)
                                            .wrap(),
                                    );
                                });
                            });
                    });
                        });
                    });
                });
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn device_label(device: &DeviceInfo) -> String {
    if device.product_version.is_empty() {
        format!("{} ({})", device.name, device.connection)
    } else {
        format!(
            "{} OS {} ({})",
            device.name, device.product_version, device.connection
        )
    }
}

fn render_device_status(ui: &mut egui::Ui, status: &Result<DeviceStatus, String>) {
    match status {
        Ok(status) => {
            ui.label(format!(
                "Wireless Debugging: {}",
                format_check_status(&status.wireless_debugging)
            ));
            ui.label(format!(
                "Developer Mode: {}",
                format_check_status(&status.developer_mode)
            ));
            ui.label(format!(
                "Developer Disk Image: {}",
                format_check_status(&status.developer_disk_image)
            ));
        }
        Err(error) => {
            ui.label(format!("Wireless Debugging: Failed: {error}"));
            ui.label(format!("Developer Mode: Failed: {error}"));
            ui.label(format!("Developer Disk Image: Failed: {error}"));
        }
    }
}

fn format_check_status(status: &CheckStatus) -> String {
    match status {
        CheckStatus::Success => "Success".to_string(),
        CheckStatus::Failed(error) => format!("Failed: {error}"),
    }
}

const BUNDLED_SCRIPTS: [(&str, &str); 4] = [
    ("Geode.js", include_str!("../scripts/Geode.js")),
    ("maciOS.js", include_str!("../scripts/maciOS.js")),
    ("universal.js", include_str!("../scripts/universal.js")),
    (
        "UTM-DolphiniOS-Flycast.js",
        include_str!("../scripts/UTM-DolphiniOS-Flycast.js"),
    ),
];

fn read_script_entries(dir: &Path) -> std::io::Result<Vec<ScriptEntry>> {
    let mut entries = BUNDLED_SCRIPTS
        .iter()
        .map(|(name, source)| ScriptEntry {
            editable: false,
            name: (*name).to_string(),
            location: ScriptLocation::Bundled(source),
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
    let mut message = error.to_string();
    if error.contains("device socket io failed") && error.contains("os error 10061") {
        #[cfg(target_os = "windows")]
        {
            message.push_str(" Make sure you have Apple Devices/iTunes installed.");
        }
        #[cfg(target_os = "linux")]
        {
            message.push_str(" Make sure you have usbmuxd installed.");
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
    matches!(
        script_name_key(name).as_str(),
        "geode" | "macios" | "universal" | "utmdolphiniosflycast"
    )
}

fn script_name_key(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    stem.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

const DEFAULT_SCRIPT_NAME: &str = "universal.js";
const MACIOS_SCRIPT_NAME: &str = "maciOS.js";
const GEODE_SCRIPT_NAME: &str = "Geode.js";
const UNIVERSAL_SCRIPT_NAME: &str = "universal.js";
const UTM_SCRIPT_NAME: &str = "UTM-DolphiniOS-Flycast.js";

fn recommended_script_for_target(target_name: &str) -> Option<&'static str> {
    let key = script_name_key(target_name);
    match key.as_str() {
        "macios" => Some(MACIOS_SCRIPT_NAME),
        "amethyst" | "melonx" | "xenios" | "melocafe" | "manic emu" => Some(UNIVERSAL_SCRIPT_NAME),
        "geode" => Some(GEODE_SCRIPT_NAME),
        "utm" | "dolphinios" | "flycast" => Some(UTM_SCRIPT_NAME),
        _ => None,
    }
}
