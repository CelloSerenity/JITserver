use std::{
    cell::Cell,
    rc::Rc,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

use eframe::egui;
use futures::future::{AbortHandle, Abortable};
use netmuxd::usb::provider::UsbMuxProvider;
use tokio::sync::watch;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;

use crate::{
    Device, DeviceInfo,
    device_services::{self, RequiresScriptsStatus},
    script_catalog::BundledScript as Script,
    targets::{self, AppInfo, PROCESS_REFRESH_INTERVAL, ProcessInfo},
    ui::{self, JitAction},
};

#[derive(Default)]
struct Session {
    device: Option<Device>,
    paired: Rc<Cell<bool>>,
    device_info: Option<DeviceInfo>,
    apps: Rc<[AppInfo]>,
    processes: Rc<[ProcessInfo]>,
    has_process_list: bool,
    developer_mode: Option<bool>,
    disk_image: Option<Result<bool, String>>,
    requires_scripts: RequiresScriptsStatus,
}

enum Command {
    Connect,
    Disconnect,
    Pair,
    ExportPairingFile,
    MountDeveloperDiskImage,
    UnmountDeveloperDiskImage,
    Refresh,
    RefreshProcesses,
    Launch(String),
    Attach(u32),
    RunScript {
        bundle_id: Option<String>,
        pid: u32,
        script: Script,
    },
}

impl Command {
    fn label(&self) -> &'static str {
        match self {
            Self::Connect => "Connecting over USB…",
            Self::Disconnect => "Disconnecting…",
            Self::Pair => "Pairing… Accept the trust prompt on your device.",
            Self::ExportPairingFile => "Creating remote pairing file…",
            Self::MountDeveloperDiskImage => "Mounting developer disk image…",
            Self::UnmountDeveloperDiskImage => "Unmounting developer disk image…",
            Self::Refresh => "Refreshing apps and processes…",
            Self::RefreshProcesses => "Refreshing processes…",
            Self::Launch(_) => "Launching and attaching debugger…",
            Self::Attach(_) => "Attaching debugger…",
            Self::RunScript { .. } => "Running JIT script…",
        }
    }

    async fn run(
        self,
        session: &mut Session,
        log: Rc<dyn Fn(String)>,
        stop: Option<watch::Receiver<bool>>,
        attached: Rc<Cell<bool>>,
    ) -> Result<String, JsValue> {
        if matches!(self, Self::Connect) {
            session.device = Some(Device::connect().await?);
        }
        match self {
            Self::Connect
                if session
                    .device
                    .as_ref()
                    .is_some_and(|device| device.pairing.is_none()) =>
            {
                if let Some(device) = &session.device {
                    session.device_info = device.info().await.ok();
                }
                return Ok("USB device connected. Click Pair and accept the trust prompt.".into());
            }
            Self::Disconnect => {
                if let Some(device) = session.device.take() {
                    device.disconnect().await;
                }
                *session = Session::default();
                return Ok("Device disconnected.".into());
            }
            _ => {}
        }

        let device = session
            .device
            .as_mut()
            .ok_or("Connect a USB device first.")?;
        let pairing_status = match self {
            Self::Connect => "Using saved pairing. ",
            Self::Pair => {
                let saved = device.pair().await?;
                session.paired.set(true);
                if saved {
                    "Pairing saved. "
                } else {
                    "Paired for this session. The browser could not save the record. "
                }
            }
            Self::ExportPairingFile => {
                device.export_pairing_file().await?;
                return Ok("Download started for pairingFile.plist.".into());
            }
            Self::MountDeveloperDiskImage => {
                if let Err(error) = mount_disk_image(device, log.as_ref()).await {
                    session.disk_image = Some(Err(error_text(&error)));
                    return Err(error);
                }
                session.disk_image = Some(Ok(true));
                match list_processes(device.provider()?, &session.apps).await {
                    Ok(processes) => {
                        session.processes = processes.into();
                        session.has_process_list = true;
                    }
                    Err(error) => {
                        return Ok(format!(
                            "Developer disk image mounted. Process refresh failed: {}",
                            error_text(&error)
                        ));
                    }
                }
                return Ok("Developer disk image mounted.".into());
            }
            Self::UnmountDeveloperDiskImage => {
                wasmtimer::tokio::timeout(
                    Duration::from_secs(30),
                    crate::ddi::unmount(&device.provider()?),
                )
                .await
                .map_err(|_| JsValue::from_str("Developer disk image unmount timed out"))??;
                session.disk_image = Some(Ok(false));
                return Ok("Developer disk image unmounted.".into());
            }
            Self::Refresh => "",
            Self::RefreshProcesses => {
                session.has_process_list = false;
                session.processes = list_processes(device.provider()?, &session.apps)
                    .await?
                    .into();
                session.has_process_list = true;
                return Ok(format!(
                    "Found {} matching process(es).",
                    session.processes.len()
                ));
            }
            Self::Attach(pid) => {
                device.attach_to_pid(pid).await?;
                return Ok(format!(
                    "Debugger attached and detached from process {pid}."
                ));
            }
            Self::Launch(bundle_id) => {
                let pid = device.launch_with_jit(bundle_id).await?;
                return Ok(format!(
                    "Debugger attached and detached from process {pid}."
                ));
            }
            Self::RunScript {
                bundle_id,
                pid,
                script,
            } => {
                let stop = stop.ok_or("Script stop control unavailable")?;
                let result = device
                    .run_script(bundle_id, pid, script, log, stop, attached)
                    .await?;
                return Ok(match result {
                    Some(pid) => format!("Script {} finished for process {pid}.", script.name()),
                    None => "Script stopped.".into(),
                });
            }
            Self::Disconnect => unreachable!(),
        };

        session.apps = Rc::default();
        session.processes = Rc::default();
        session.has_process_list = false;
        session.developer_mode = None;
        session.disk_image = None;
        session.requires_scripts = match device.info().await {
            Ok(info) => {
                session.paired.set(true);
                let status = device_services::requires_scripts(&info.model, &info.version)
                    .unwrap_or_else(|error| RequiresScriptsStatus::Unknown(format!("{error:?}")));
                session.device_info = Some(info);
                status
            }
            Err(error) => RequiresScriptsStatus::Unknown(error_text(&error)),
        };
        session.developer_mode = Some(match device.developer_mode().await {
            Ok(enabled) => {
                session.paired.set(true);
                enabled
            }
            Err(error) if matches!(self, Self::Connect) && !session.paired.get() => {
                return Err(format!(
                    "Could not use saved pairing: {}. Unlock the device and refresh, or click Pair.",
                    error_text(&error)
                ).into());
            }
            Err(error) => {
                return Err(format!(
                    "{pairing_status}Device status refresh failed: {}",
                    error_text(&error)
                )
                .into());
            }
        });
        session.disk_image = Some(if session.developer_mode == Some(true) {
            match mount_disk_image(device, log.as_ref()).await {
                Ok(()) => Ok(true),
                Err(error) => {
                    let error = error_text(&error);
                    log(format!("Developer disk image mount failed: {error}"));
                    Err(error)
                }
            }
        } else {
            Ok(false)
        });
        session.apps = device
            .apps()
            .await
            .map_err(|error| {
                JsValue::from_str(&format!(
                    "{pairing_status}App refresh failed: {}",
                    error_text(&error)
                ))
            })?
            .into();
        let apps = session.apps.len();
        log(format!("{pairing_status}Found {apps} debuggable app(s)."));
        if session.developer_mode == Some(true) {
            match list_processes(device.provider()?, &session.apps).await {
                Ok(processes) => {
                    session.processes = processes.into();
                    session.has_process_list = true;
                }
                Err(error) => {
                    return Ok(format!(
                        "Process refresh failed: {}",
                        error_text(&error)
                    ));
                }
            }
        }
        Ok(String::new())
    }
}

async fn mount_disk_image(device: &Device, log: &dyn Fn(String)) -> Result<(), JsValue> {
    wasmtimer::tokio::timeout(
        Duration::from_secs(240),
        crate::ddi::mount(&device.provider()?, log),
    )
    .await
    .map_err(|_| JsValue::from_str("Developer disk image mount timed out"))?
}

async fn list_processes(
    provider: UsbMuxProvider,
    apps: &[AppInfo],
) -> Result<Vec<ProcessInfo>, JsValue> {
    let mut processes =
        wasmtimer::tokio::timeout(Duration::from_secs(8), Device::processes(provider))
            .await
            .map_err(|_| JsValue::from_str("Process refresh timed out"))??;
    targets::filter_debuggable_processes(&mut processes, apps);
    Ok(processes)
}

type Completion = (Session, Result<String, JsValue>);

struct Pending {
    receiver: Receiver<Completion>,
    connected: bool,
    stop: Option<watch::Sender<bool>>,
    attached: Rc<Cell<bool>>,
}

struct ProcessRefresh {
    abort: AbortHandle,
    receiver: Receiver<Result<Vec<ProcessInfo>, JsValue>>,
}

#[derive(Default)]
pub struct BrowserApp {
    session: Session,
    pending: Option<Pending>,
    status: String,
    logs: Vec<String>,
    show_logs: bool,
    filter: String,
    selected: Option<String>,
    process_filter: String,
    pid: String,
    script: Script,
    script_logs: Option<Receiver<String>>,
    next_process_refresh: Option<f64>,
    process_refresh: Option<ProcessRefresh>,
}

impl BrowserApp {
    pub fn new() -> Self {
        Self {
            show_logs: true,
            ..Self::default()
        }
    }

    fn send(&mut self, ctx: &egui::Context, command: Command) {
        if let Some(refresh) = self.process_refresh.take() {
            refresh.abort.abort();
        }
        let (tx, rx) = mpsc::channel();
        let mut session = Session {
            device: self.session.device.take(),
            paired: Rc::clone(&self.session.paired),
            device_info: self.session.device_info.clone(),
            apps: Rc::clone(&self.session.apps),
            processes: Rc::clone(&self.session.processes),
            has_process_list: self.session.has_process_list,
            developer_mode: self.session.developer_mode,
            disk_image: self.session.disk_image.clone(),
            requires_scripts: self.session.requires_scripts.clone(),
        };
        let (stop_tx, stop_rx) = if matches!(command, Command::RunScript { .. }) {
            let (tx, rx) = watch::channel(false);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let attached = Rc::new(Cell::new(false));
        let ctx = ctx.clone();
        self.status = command.label().into();
        self.pending = Some(Pending {
            receiver: rx,
            connected: session.device.is_some(),
            stop: stop_tx,
            attached: Rc::clone(&attached),
        });
        let (log_tx, log_rx) = mpsc::channel();
        self.script_logs = Some(log_rx);
        let log_ctx = ctx.clone();
        let log = Rc::new(move |message| {
            let _ = log_tx.send(message);
            log_ctx.request_repaint();
        });
        spawn_local(async move {
            let mut result = command.run(&mut session, log, stop_rx, attached).await;
            if session
                .device
                .as_mut()
                .is_some_and(|device| device.transport.ended())
            {
                session = Session::default();
                result = Err("The USB connection closed. Unlock the device, then click Connect to reconnect.".into());
            }
            let _ = tx.send((session, result));
            ctx.request_repaint();
        });
    }
    fn schedule_process_refresh(&mut self, ctx: &egui::Context) {
        self.next_process_refresh = self
            .session
            .has_process_list
            .then(|| ctx.input(|input| input.time) + PROCESS_REFRESH_INTERVAL.as_secs_f64());
    }

    fn maybe_refresh_processes(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() || self.process_refresh.is_some() {
            return;
        }
        let Some(next) = self.next_process_refresh else {
            return;
        };
        let now = ctx.input(|input| input.time);
        if now < next {
            ctx.request_repaint_after(Duration::from_secs_f64(next - now));
            return;
        }

        self.next_process_refresh = None;
        let Some(provider) = self
            .session
            .device
            .as_ref()
            .and_then(|device| device.provider().ok())
        else {
            return;
        };
        let (tx, receiver) = mpsc::channel();
        let (abort, registration) = AbortHandle::new_pair();
        self.process_refresh = Some(ProcessRefresh { abort, receiver });
        let apps = Rc::clone(&self.session.apps);
        let ctx = ctx.clone();
        spawn_local(async move {
            if let Ok(result) = Abortable::new(list_processes(provider, &apps), registration).await
            {
                let _ = tx.send(result);
                ctx.request_repaint();
            }
        });
    }

    fn finish_process_refresh(&mut self, ctx: &egui::Context) {
        let Some(result) = self
            .process_refresh
            .as_ref()
            .and_then(|refresh| refresh.receiver.try_recv().ok())
        else {
            return;
        };
        self.process_refresh = None;
        if self
            .session
            .device
            .as_mut()
            .is_some_and(|device| device.transport.ended())
        {
            self.session = Session::default();
            self.selected = None;
            self.pid.clear();
            self.status =
                "The USB connection closed. Unlock the device, then click Connect to reconnect."
                    .into();
            self.logs.push(self.status.clone());
            return;
        }
        match result {
            Ok(processes) => self.session.processes = processes.into(),
            Err(error) => {
                self.session.has_process_list = false;
                self.logs.push(format!(
                    "Automatic process refresh failed: {}",
                    error_text(&error)
                ));
            }
        }
        self.schedule_process_refresh(ctx);
    }
}

impl eframe::App for BrowserApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(receiver) = &self.script_logs {
            self.logs.extend(receiver.try_iter());
        }
        self.finish_process_refresh(ctx);
        let Some(pending) = &self.pending else {
            return;
        };
        let Ok((session, result)) = pending.receiver.try_recv() else {
            return;
        };
        self.session = session;
        self.pending = None;
        self.schedule_process_refresh(ctx);
        if self.session.device.is_none() {
            self.pid.clear();
        }
        self.status = result.unwrap_or_else(|error| error_text(&error));
        if !self.status.is_empty() {
            self.logs.push(self.status.clone());
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|id| !self.session.apps.iter().any(|app| &app.bundle_id == id))
        {
            self.selected = None;
        }
    }

    fn ui(&mut self, panel: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = panel.ctx().clone();
        let busy = self.pending.is_some();
        let connected = self.pending.as_ref().map_or_else(
            || self.session.device.is_some(),
            |pending| pending.connected,
        );
        let paired = self.session.paired.get();
        let device_info = self.session.device_info.as_ref();
        let mut command = None;

        ui::device_bar(panel, &mut self.show_logs, |ui| {
            if connected {
                let name = device_info.map_or("Apple Device", |info| info.name.as_str());
                ui.label(format!("{name} · USB"));
                if ui
                    .add_enabled(!busy, egui::Button::new("Disconnect"))
                    .clicked()
                {
                    command = Some(Command::Disconnect);
                }
                let label = if paired { "Paired" } else { "Not paired" };
                ui.label(ui::status_text(label, paired));
            } else if ui
                .add_enabled(!busy, egui::Button::new("Connect"))
                .clicked()
            {
                command = Some(Command::Connect);
            }
        });

        egui::CentralPanel::default().show(panel, |panel| {
            let mut content = |left: &mut egui::Ui, scripts: Option<&mut egui::Ui>| {
                let stop = self
                    .pending
                    .as_ref()
                    .and_then(|pending| pending.stop.as_ref());
                let script_running = stop.is_some();
                let stopping = stop.is_some_and(|signal| *signal.borrow());
                let attached = self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.attached.get());
                left.horizontal_wrapped(|ui| {
                    ui.heading(device_info.map_or("Device Status", |info| info.name.as_str()));
                    ui::debug_indicators(ui, attached, script_running);
                    if busy {
                        ui.spinner();
                    }
                });
                if self.status.is_empty() && !connected {
                    left.label("Connect an unlocked iPhone or iPad.");
                    left.label("Use a Chromium-based browser on macOS, Linux, or Android.");
                    left.horizontal_wrapped(|ui| {
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        ui.spacing_mut().item_spacing.x =
                            ui.fonts_mut(|fonts| fonts.glyph_width(&font, ' '));
                        ui.hyperlink_to("Helium", "https://helium.computer/");
                        ui.label("is recommended on desktop.");
                    });
                } else if !self.status.is_empty() {
                    left.label(&self.status);
                }
                if let Some(info) = device_info {
                    ui::device_identity(
                        left,
                        Some(&info.model),
                        Some(&info.version),
                        info.build.as_deref(),
                        info.udid.as_deref(),
                    );
                    ui::device_version_warning(left, &info.version);
                }
                if !connected {
                    left.weak("Automatic disk image mounting may send device identifiers to Apple through the signing proxy.");
                }
                if connected && paired {
                    ui::device_check(left, "Wireless debugging", |ui| {
                        ui::device_check_value(
                            ui,
                            device_info.and_then(|info| info.wireless_debugging),
                            "Off",
                        );
                    });
                    ui::device_check(left, "Developer mode", |ui| {
                        ui::device_check_value(ui, self.session.developer_mode, "Disabled");
                    });
                    ui::device_check(left, "Developer disk image", |ui| {
                        match &self.session.disk_image {
                            Some(Ok(true)) => {
                                if ui::mounted_ddi_label(ui) && !busy {
                                    command = Some(Command::UnmountDeveloperDiskImage);
                                }
                            }
                            Some(Ok(false)) => {
                                ui.label(egui::RichText::new("Unmounted").color(ui.visuals().warn_fg_color));
                            }
                            Some(Err(error)) => {
                                ui.label(egui::RichText::new("Mount failed").color(ui.visuals().error_fg_color)).on_hover_text(error);
                            }
                            None => {
                                ui.label("Unknown");
                            }
                        }
                        if matches!(self.session.disk_image, Some(Ok(false) | Err(_)))
                            && ui
                                .add_enabled(
                                    !busy && self.session.developer_mode == Some(true),
                                    egui::Button::new("Mount"),
                                )
                                .on_hover_text("If Apple needs to sign the image, the request includes device identifiers and is sent through the configured signing proxy.")
                                .clicked()
                        {
                            command = Some(Command::MountDeveloperDiskImage);
                        }
                    });
                    ui::requires_scripts_status(left, &self.session.requires_scripts);
                }
                left.horizontal_wrapped(|ui| {
                    let pair_label = if paired { "Pair again" } else { "Pair" };
                    if ui
                        .add_enabled(connected && !busy, egui::Button::new(pair_label))
                        .clicked()
                    {
                        command = Some(Command::Pair);
                    }
                    if ui
                        .add_enabled(paired && !busy, egui::Button::new("Export pairing file"))
                        .clicked()
                    {
                        command = Some(Command::ExportPairingFile);
                    }
                });
                left.separator();

                let requires_scripts =
                    matches!(self.session.requires_scripts, RequiresScriptsStatus::Yes);
                let can_launch = !busy && self.session.developer_mode == Some(true);
                let mut stop_clicked = false;
                let list_height = (left.available_height() - 145.0).clamp(120.0, 420.0);
                let panel_bottom = scripts
                    .is_some()
                    .then(|| left.max_rect().bottom() - left.spacing().item_spacing.y);
                left.columns(2, |targets| {
                    let apps = &mut targets[0];
                    if ui::list_heading(apps, "Apps", paired && !busy) {
                        command = Some(Command::Refresh);
                    }
                    if let Some(app) = ui::app_list(
                        apps,
                        &self.session.apps,
                        self.selected.as_deref(),
                        &mut self.filter,
                        list_height,
                    ) {
                        self.selected = Some(app.bundle_id.clone());
                        if !busy && let Some(script) = Script::recommended(&app.name) {
                            self.script = script;
                        }
                    }
                    let app_selected = can_launch && self.selected.is_some();
                    match ui::device_jit_actions(
                        apps,
                        requires_scripts,
                        app_selected,
                        true,
                        script_running,
                        stopping,
                    ) {
                        Some(JitAction::Enable) => {
                            command = self.selected.clone().map(Command::Launch);
                        }
                        Some(JitAction::RunScript) => {
                            command = Some(Command::RunScript {
                                bundle_id: self.selected.clone(),
                                pid: 0,
                                script: self.script,
                            });
                        }
                        Some(JitAction::Stop) => stop_clicked = true,
                        None => {}
                    }

                    let processes = &mut targets[1];
                    if ui::list_heading(processes, "PID", can_launch) {
                        command = Some(Command::RefreshProcesses);
                    }
                    if let Some(process) = ui::process_list(
                        processes,
                        &self.session.processes,
                        ui::parse_pid(&self.pid),
                        &mut self.process_filter,
                        list_height,
                    ) {
                        self.pid = process.pid.to_string();
                        if !busy && let Some(script) = Script::recommended(&process.name) {
                            self.script = script;
                        }
                    }
                    ui::pid_field(processes, &mut self.pid);
                    let pid = ui::parse_pid(&self.pid);
                    let pid_selected = can_launch && pid.is_some();
                    match ui::device_jit_actions(
                        processes,
                        requires_scripts,
                        pid_selected,
                        true,
                        script_running,
                        stopping,
                    ) {
                        Some(JitAction::Enable) => command = pid.map(Command::Attach),
                        Some(JitAction::RunScript) => {
                            command = pid.map(|pid| Command::RunScript {
                                bundle_id: None,
                                pid,
                                script: self.script,
                            });
                        }
                        Some(JitAction::Stop) => stop_clicked = true,
                        None => {}
                    }
                    let bounds = targets[0].min_rect().union(targets[1].min_rect());
                    let x =
                        targets[1].max_rect().left() - targets[1].spacing().item_spacing.x / 2.0;
                    targets[1].painter().vline(
                        x,
                        bounds.top()..=bounds.bottom().max(panel_bottom.unwrap_or(bounds.bottom())),
                        targets[1].visuals().widgets.noninteractive.bg_stroke,
                    );
                });
                if stop_clicked && let Some(stop) = stop {
                    let _ = stop.send(true);
                    self.status = "Stopping script…".into();
                    ctx.request_repaint();
                }

                let scripts = scripts.unwrap_or_else(|| {
                    left.separator();
                    left
                });
                ui::script_section(scripts, self.show_logs, |scripts| {
                    scripts.heading("Scripts");
                    scripts.label("Select a bundled script.");
                    scripts.label(
                        "Scripts are required to enable JIT on TXM devices running iOS 26+.",
                    );
                    let selected = Script::ALL.iter().position(|&script| script == self.script);
                    if let Some(index) = ui::script_picker(
                        scripts,
                        !script_running,
                        selected,
                        Script::ALL.map(Script::name),
                    ) {
                        self.script = Script::ALL[index];
                    }
                    if script_running {
                        scripts.small("Script switching is disabled while a script is running.");
                    }
                });
                if self.show_logs {
                    ui::logs(scripts, &mut self.logs);
                }
            };
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(panel, |panel| {
                    if panel.available_width() >= 560.0 {
                        panel.columns(2, |columns| {
                            let (left, right) = columns.split_at_mut(1);
                            content(&mut left[0], Some(&mut right[0]));
                        });
                    } else {
                        content(panel, None);
                    }
                });
        });
        if let Some(command) = command {
            self.send(&ctx, command);
        } else {
            self.maybe_refresh_processes(&ctx);
        }
    }
}

fn error_text(error: &JsValue) -> String {
    error
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(error, &"message".into())
                .ok()?
                .as_string()
        })
        .unwrap_or_else(|| "Device operation failed.".into())
}
