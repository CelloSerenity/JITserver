use crate::device_services::{self, DDI_FILES, RequiresScriptsStatus, configure_no_ack_mode, connect_rsd, ddi_asset_url, ddi_directory, launch_application};
use crate::targets::{self, AppInfo, ProcessInfo};

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use idevice::{
    IdeviceError, IdeviceService, ReadWrite, RsdService,
    core_device::AppServiceClient,
    cryptexd::{Cryptex1Assets, install_ddi, installed_ddi},
    debug_proxy::{DebugProxyClient, DebugserverCommand},
    installation_proxy::InstallationProxyClient,
    mobile_image_mounter::ImageMounter,
    provider::{IdeviceProvider, UsbmuxdProvider},
    services::lockdown::LockdownClient,
    tcp::handle::AdapterHandle,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection},
};
use tokio::{io::AsyncWriteExt, runtime::Runtime, sync::Mutex};

use crate::scripts;

const LABEL: &str = "JITserver";

#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub model: Option<String>,
    pub version: Option<String>,
    pub build: Option<String>,
    pub connection: DeviceConnection,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeviceConnection {
    Usb,
    Network,
    #[default]
    Other,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceStatus {
    pub wireless_debugging: CheckStatus,
    pub developer_mode: CheckStatus,
    pub developer_disk_image: CheckStatus,
    pub requires_scripts: RequiresScriptsStatus,
}

#[derive(Clone, Debug)]
pub enum CheckStatus {
    Checking,
    Downloading,
    Success,
    Disabled,
    Failed(String),
}

impl Default for CheckStatus {
    fn default() -> Self {
        Self::Checking
    }
}

#[derive(Clone, Copy, Debug)]
pub enum DiskImageAction {
    Mount,
    Unmount,
}

#[derive(Debug)]
pub enum WorkerCommand {
    ListApps {
        udid: String,
        connection: DeviceConnection,
        background: bool,
    },
    ListProcesses {
        udid: String,
    },
    ManageDeveloperDiskImage {
        udid: String,
        connection: DeviceConnection,
        action: DiskImageAction,
    },
    LaunchAndAttach {
        udid: String,
        bundle_id: String,
    },
    LaunchAndAttachAndRunScript {
        udid: String,
        bundle_id: String,
        name: String,
        source: String,
    },
    Attach {
        udid: String,
        pid: u32,
    },
    AttachAndRun {
        udid: String,
        pid: u32,
        name: String,
        source: String,
    },
    Stop,
}

#[derive(Debug)]
pub enum WorkerActivity {
    Idle,
    Background,
    Foreground { started: Instant },
}

#[derive(Debug)]
pub enum WorkerEvent {
    Activity(WorkerActivity),
    Devices(Result<Vec<DeviceInfo>, String>),
    DeviceStatus {
        udid: String,
        connection: DeviceConnection,
        status: Result<DeviceStatus, String>,
    },
    Apps(Result<Vec<AppInfo>, String>),
    Processes(Result<Vec<ProcessInfo>, String>),
    DeveloperDiskImage {
        udid: String,
        connection: DeviceConnection,
        action: DiskImageAction,
        result: Result<(), String>,
    },
    Launched(Result<u32, String>),
    Attached(Result<String, String>),
    DebugResponse(Result<String, String>),
    ScriptFinished(Result<(), String>),
    Log(String),
}

struct ActiveDebugSession {
    pid: u32,
    _adapter: AdapterHandle,
    debug_proxy: Arc<Mutex<DebugProxyClient<Box<dyn ReadWrite>>>>,
    stop_requested: Arc<AtomicBool>,
}

pub fn spawn_worker() -> (mpsc::Sender<WorkerCommand>, mpsc::Receiver<WorkerEvent>) {
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();

    let device_event_tx = event_tx.clone();
    thread::spawn(move || {
        let runtime = Runtime::new().expect("failed to create tokio runtime");
        runtime.block_on(watch_devices(device_event_tx));
    });

    thread::spawn(move || {
        let runtime = Runtime::new().expect("failed to create tokio runtime");
        let mut worker = DeviceWorker {
            event_tx,
            active_debug: None,
        };

        while let Ok(command) = command_rx.recv() {
            let activity = match &command {
                WorkerCommand::ListProcesses { .. }
                | WorkerCommand::ListApps { background: true, .. } => WorkerActivity::Background,
                _ => WorkerActivity::Foreground { started: Instant::now() },
            };
            let _ = worker.event_tx.send(WorkerEvent::Activity(activity));
            runtime.block_on(worker.handle(command));
            let _ = worker
                .event_tx
                .send(WorkerEvent::Activity(WorkerActivity::Idle));
        }
    });

    (command_tx, event_rx)
}

struct DeviceWorker {
    event_tx: mpsc::Sender<WorkerEvent>,
    active_debug: Option<ActiveDebugSession>,
}

impl DeviceWorker {
    async fn handle(&mut self, command: WorkerCommand) {
        match command {
            WorkerCommand::ListApps { udid, connection, .. } => {
                inspect_device_status(&udid, connection, &self.event_tx).await;
                let result = list_apps(&udid).await.map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::Apps(result));
                let processes = list_processes(&udid).await.map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::Processes(processes));
            }
            WorkerCommand::ListProcesses { udid } => {
                let processes = list_processes(&udid).await.map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::Processes(processes));
            }
            WorkerCommand::ManageDeveloperDiskImage {
                udid,
                connection,
                action,
            } => {
                let result = async {
                    let provider = provider_for_udid_with_connection(&udid, connection).await?;
                    match action {
                        DiskImageAction::Mount => {
                            ensure_developer_disk_image(&provider, true, || {}).await?;
                        }
                        DiskImageAction::Unmount => {
                            let (version, _) = ddi_device_info(&provider).await?;
                            device_services::unmount_ddi(&provider, &version).await?;
                        }
                    }
                    Ok(())
                }
                .await
                .map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::DeveloperDiskImage {
                    udid,
                    connection,
                    action,
                    result,
                });
            }
            WorkerCommand::LaunchAndAttach { udid, bundle_id } => {
                let result = async {
                    let pid = launch_app(&udid, &bundle_id, true).await?;
                    let session = attach_debugger(&udid, pid).await?;
                    let (attach_response, detach_response) = attach_and_detach(&session).await?;
                    Ok((pid, attach_response, detach_response))
                }
                .await
                .map_err(format_error);

                match result {
                    Ok((pid, attach_response, detach_response)) => {
                        let _ = self.event_tx.send(WorkerEvent::Launched(Ok(pid)));
                        let _ = self
                            .event_tx
                            .send(WorkerEvent::Attached(Ok(attach_response)));
                        let _ = self
                            .event_tx
                            .send(WorkerEvent::DebugResponse(Ok(detach_response)));
                    }
                    Err(error) => {
                        let _ = self.event_tx.send(WorkerEvent::Attached(Err(error)));
                    }
                }
            }
            WorkerCommand::LaunchAndAttachAndRunScript {
                udid,
                bundle_id,
                name,
                source,
            } => {
                let result = async {
                    let pid = launch_app(&udid, &bundle_id, true).await?;
                    let session = attach_debugger(&udid, pid).await?;
                    let response = send_debug_command(&session, format!("vAttach;{pid:x}")).await?;
                    Ok((pid, session, response))
                }
                .await
                .map_err(format_error);

                match result {
                    Ok((pid, session, response)) => {
                        let _ = self.event_tx.send(WorkerEvent::Launched(Ok(pid)));
                        let _ = self.event_tx.send(WorkerEvent::Attached(Ok(response)));
                        self.active_debug = Some(session);
                        if let Some(session) = self.active_debug.as_ref() {
                            self.start_script_with_session(session, name, source);
                        }
                    }
                    Err(error) => {
                        let _ = self.event_tx.send(WorkerEvent::Attached(Err(error)));
                    }
                }
            }
            WorkerCommand::Attach { udid, pid } => {
                let result = async {
                    let session = attach_debugger(&udid, pid).await?;
                    attach_and_detach(&session).await
                }
                .await
                .map_err(format_error);
                match result {
                    Ok((attach_response, detach_response)) => {
                        let _ = self.event_tx.send(WorkerEvent::Attached(Ok(attach_response)));
                        let _ = self.event_tx.send(WorkerEvent::DebugResponse(Ok(detach_response)));
                    }
                    Err(error) => {
                        let _ = self.event_tx.send(WorkerEvent::Attached(Err(error)));
                    }
                }
            }
            WorkerCommand::AttachAndRun {
                udid,
                pid,
                name,
                source,
            } => {
                let result = async {
                    let session = attach_debugger(&udid, pid).await?;
                    let response = send_debug_command(&session, format!("vAttach;{pid:x}")).await?;
                    Ok((session, response))
                }
                .await
                .map_err(format_error);

                match result {
                    Ok((session, response)) => {
                        let _ = self.event_tx.send(WorkerEvent::Attached(Ok(response)));
                        self.active_debug = Some(session);
                        if let Some(session) = self.active_debug.as_ref() {
                            self.start_script_with_session(session, name, source);
                        }
                    }
                    Err(error) => {
                        let _ = self.event_tx.send(WorkerEvent::Attached(Err(error)));
                    }
                }
            }
            WorkerCommand::Stop => {
                let result = async {
                    let Some(session) = self.active_debug.take() else {
                        return Ok("Stopped. No active debug session remained.".to_string());
                    };

                    session.stop_requested.store(true, Ordering::SeqCst);

                    match send_debug_command(&session, "D").await {
                        Ok(response) if response.is_empty() => {
                            Ok("Stopped script and detached debugger.".to_string())
                        }
                        Ok(response) => Ok(response),
                        Err(error) => {
                            let _ = self.event_tx.send(WorkerEvent::Log(format!(
                                "Stop detached local session after debugserver error: {error:#}"
                            )));
                            Ok("Stopped script and cleared debug session.".to_string())
                        }
                    }
                }
                .await
                .map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::DebugResponse(result));
            }
        }
    }

    fn start_script_with_session(
        &self,
        session: &ActiveDebugSession,
        name: String,
        source: String,
    ) {
        let context = scripts::ScriptContext {
            name,
            pid: session.pid,
            debug_proxy: Arc::clone(&session.debug_proxy),
            event_tx: self.event_tx.clone(),
            tokio_handle: tokio::runtime::Handle::current(),
            stop_requested: Arc::clone(&session.stop_requested),
        };
        let event_tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = scripts::run_script(context, source).map_err(format_error);
            let _ = event_tx.send(WorkerEvent::ScriptFinished(result));
        });
    }
}

async fn watch_devices(event_tx: mpsc::Sender<WorkerEvent>) {
    let mut last_error = None;
    loop {
        let result = async {
            let mut mux = UsbmuxdConnection::default().await?;
            let mut events = mux.listen().await?;
            let devices = refresh_devices().await?;
            if event_tx.send(WorkerEvent::Devices(Ok(devices))).is_err() {
                return Ok(());
            }
            last_error = None;

            while let Some(event) = events.next().await {
                event?;
                let devices = refresh_devices().await?;
                if event_tx.send(WorkerEvent::Devices(Ok(devices))).is_err() {
                    return Ok(());
                }
            }
            Ok(())
        }
        .await;

        if let Err(error) = result {
            let error = format_error(error);
            if last_error.as_deref() != Some(error.as_str()) {
                if event_tx
                    .send(WorkerEvent::Devices(Err(error.clone())))
                    .is_err()
                {
                    return;
                }
                last_error = Some(error);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn refresh_devices() -> Result<Vec<DeviceInfo>> {
    let mut mux = UsbmuxdConnection::default().await?;
    let devices = mux.get_devices().await?;
    let mut infos = Vec::with_capacity(devices.len());

    for device in devices {
        let provider = device.to_provider(UsbmuxdAddr::default(), LABEL);
        let mut model = None;
        let mut version = None;
        let mut build = None;
        let name = match LockdownClient::connect(&provider).await {
            Ok(mut lockdown) => {
                if let Ok(pairing_file) = provider.get_pairing_file().await {
                    let _ = lockdown.start_session(&pairing_file).await;
                }
                version = lockdown
                    .get_value(Some("ProductVersion"), None)
                    .await
                    .ok()
                    .and_then(|value| value.as_string().map(ToOwned::to_owned));
                model = lockdown
                    .get_value(Some("ProductType"), None)
                    .await
                    .ok()
                    .and_then(|value| value.as_string().map(ToOwned::to_owned));
                build = lockdown
                    .get_value(Some("BuildVersion"), None)
                    .await
                    .ok()
                    .and_then(|value| value.as_string().map(ToOwned::to_owned));
                lockdown
                    .get_value(Some("DeviceName"), None)
                    .await
                    .ok()
                    .and_then(|value| value.as_string().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "Apple Device".to_string())
            }
            Err(_) => "Apple Device".to_string(),
        };

        infos.push(DeviceInfo {
            udid: device.udid,
            name,
            model,
            version,
            build,
            connection: DeviceConnection::from(&device.connection_type),
        });
    }

    Ok(infos)
}

async fn list_apps(udid: &str) -> Result<Vec<AppInfo>> {
    let provider = provider_for_udid(udid).await?;
    let mut client = InstallationProxyClient::connect(&provider).await?;
    let apps = client.get_apps(Some("Any"), None).await?;
    Ok(targets::debuggable_apps(apps))
}

async fn inspect_device_status(
    udid: &str,
    connection: DeviceConnection,
    event_tx: &mpsc::Sender<WorkerEvent>,
) {
    let mut status = DeviceStatus::default();
    let send_status = |status: Result<DeviceStatus, String>| {
        let _ = event_tx.send(WorkerEvent::DeviceStatus {
            udid: udid.to_string(),
            connection,
            status,
        });
    };
    let provider = match provider_for_udid_with_connection(udid, connection).await {
        Ok(provider) => provider,
        Err(error) => {
            send_status(Err(device_status_error(error)));
            return;
        }
    };

    if connection == DeviceConnection::Usb {
        let wireless = enable_wireless_debugging(&provider).await;
        if wireless.as_ref().err().is_some_and(is_device_locked) {
            send_status(Err("Device Locked".to_string()));
            return;
        }
        status.wireless_debugging = check_status(wireless);
        send_status(Ok(status.clone()));
    }

    let developer_mode = query_developer_mode(&provider).await;
    if developer_mode.as_ref().err().is_some_and(is_device_locked) {
        send_status(Err("Device Locked".to_string()));
        return;
    }
    status.developer_mode = match developer_mode {
        Ok(true) => CheckStatus::Success,
        Ok(false) => CheckStatus::Disabled,
        Err(error) => CheckStatus::Failed(format_error(error)),
    };
    send_status(Ok(status.clone()));

    let can_mount = !matches!(status.developer_mode, CheckStatus::Disabled);
    let ddi = ensure_developer_disk_image(&provider, can_mount, || {
        let mut downloading = status.clone();
        downloading.developer_disk_image = CheckStatus::Downloading;
        send_status(Ok(downloading));
    })
    .await;
    if ddi.as_ref().err().is_some_and(is_device_locked) {
        send_status(Err("Device Locked".to_string()));
        return;
    }
    status.developer_disk_image = match ddi {
        Ok(false) => CheckStatus::Disabled,
        other => check_status(other),
    };
    send_status(Ok(status.clone()));

    status.requires_scripts = match check_requires_scripts(&provider).await {
        Ok(status) => status,
        Err(error) if is_device_locked(&error) => {
            send_status(Err("Device Locked".to_string()));
            return;
        }
        Err(error) => RequiresScriptsStatus::Unknown(format_error(error)),
    };
    send_status(Ok(status));
}

fn is_device_locked(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<IdeviceError>(),
            Some(IdeviceError::DeviceLocked)
        )
    })
}

fn device_status_error(error: anyhow::Error) -> String {
    if is_device_locked(&error) {
        "Device Locked".to_string()
    } else {
        format_error(error)
    }
}

async fn ddi_device_info(provider: &UsbmuxdProvider) -> Result<(String, String)> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    lockdown
        .start_session(&provider.get_pairing_file().await?)
        .await?;
    let version = lockdown
        .get_value(Some("ProductVersion"), None)
        .await?
        .as_string()
        .context("missing ProductVersion")?
        .to_owned();
    let product_type = lockdown
        .get_value(Some("ProductType"), None)
        .await?
        .as_string()
        .context("missing ProductType")?
        .to_owned();
    Ok((version, product_type))
}

async fn unique_chip_id(provider: &UsbmuxdProvider) -> Result<u64> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    lockdown
        .start_session(&provider.get_pairing_file().await?)
        .await?;
    lockdown
        .get_value(Some("UniqueChipID"), None)
        .await?
        .as_unsigned_integer()
        .context("missing UniqueChipID")
}

fn ddi_cache_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?);
    #[cfg(target_os = "macos")]
    let base =
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join("Library/Caches");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".cache")
    };
    Ok(base.join("JITserver/DDI"))
}

async fn cached_ddi(directory: &str, cryptex: bool, on_download: impl FnOnce()) -> Result<PathBuf> {
    let path = ddi_cache_dir()?.join(directory);
    tokio::fs::create_dir_all(&path).await?;
    let client = reqwest::Client::new();
    let count = if cryptex { DDI_FILES.len() } else { 3 };
    let mut on_download = Some(on_download);
    for name in &DDI_FILES[..count] {
        let destination = path.join(name);
        if tokio::fs::metadata(&destination)
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
        {
            continue;
        }
        if let Some(callback) = on_download.take() {
            callback();
        }
        let url = ddi_asset_url(directory, name);
        let mut response = client.get(url).send().await?.error_for_status()?;
        let partial = path.join(format!("{name}.part"));
        let mut file = tokio::fs::File::create(&partial).await?;
        let mut size = 0;
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
            size += chunk.len();
        }
        file.flush().await?;
        drop(file);
        anyhow::ensure!(size > 0, "downloaded empty DDI file: {name}");
        if destination.exists() {
            tokio::fs::remove_file(&destination).await?;
        }
        tokio::fs::rename(&partial, &destination).await?;
    }
    Ok(path)
}

async fn check_requires_scripts(provider: &UsbmuxdProvider) -> Result<RequiresScriptsStatus> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    if let Ok(pairing_file) = provider.get_pairing_file().await {
        let _ = lockdown.start_session(&pairing_file).await;
    }
    Ok(device_services::check_requires_scripts(&mut lockdown).await?)
}

async fn enable_wireless_debugging(provider: &UsbmuxdProvider) -> Result<bool> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    lockdown
        .start_session(&provider.get_pairing_file().await?)
        .await?;
    lockdown
        .set_value(
            "EnableWifiDebugging",
            true.into(),
            Some("com.apple.mobile.wireless_lockdown"),
        )
        .await?;
    Ok(true)
}

async fn query_developer_mode(provider: &UsbmuxdProvider) -> Result<bool> {
    let mut mounter = ImageMounter::connect(provider).await?;
    Ok(mounter.query_developer_mode_status().await?)
}

async fn ensure_developer_disk_image(
    provider: &UsbmuxdProvider,
    can_mount: bool,
    on_download: impl FnOnce(),
) -> Result<bool> {
    let (version, product_type) = ddi_device_info(provider).await?;
    let (directory, cryptex) = ddi_directory(&version, &product_type)
        .with_context(|| format!("unsupported device type: {product_type}"))?;
    let path = cached_ddi(directory, cryptex, on_download).await?;
    if !can_mount {
        return Ok(false);
    }
    let (mut adapter, mut handshake) = connect_rsd(provider).await?;
    if cryptex && installed_ddi(&mut adapter, &mut handshake).await?.is_some() {
        return Ok(true);
    }

    let mut mounter = ImageMounter::connect(provider).await?;
    match mounter.copy_devices().await {
        Ok(devices) if !devices.is_empty() => return Ok(true),
        Err(IdeviceError::GetProhibited) => {}
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }

    if cryptex {
        let ddi = load_ddi_bundle(&path).await?;
        install_ddi(&mut adapter, &mut handshake, &ddi).await?;
    } else {
        let chip_id = unique_chip_id(provider).await?;
        let mut mounter = ImageMounter::connect_rsd(&mut adapter, &mut handshake).await?;
        let manifest = tokio::fs::read(path.join("BuildManifest.plist")).await?;
        mounter
            .mount_personalized_rsd(
                &mut adapter,
                &mut handshake,
                tokio::fs::read(path.join("Image.dmg")).await?,
                tokio::fs::read(path.join("Image.dmg.trustcache")).await?,
                &manifest,
                None,
                chip_id,
            )
            .await?;
    }
    Ok(true)
}

async fn load_ddi_bundle(path: &Path) -> Result<Cryptex1Assets> {
    let manifest: plist::Dictionary =
        plist::from_bytes(&tokio::fs::read(path.join("BuildManifest.plist")).await?)?;
    let build_identity = idevice::tss::select_cryptex_build_identity(&manifest)?.clone();
    Ok(Cryptex1Assets::from_parts(
        tokio::fs::read(path.join("Image.dmg")).await?,
        tokio::fs::read(path.join("Image.dmg.trustcache")).await?,
        tokio::fs::read(path.join("Image.dmg.cryptex_info")).await?,
        tokio::fs::read(path.join("Image.dmg.root_hash")).await?,
        build_identity,
    ))
}

async fn list_processes(udid: &str) -> Result<Vec<ProcessInfo>> {
    let provider = provider_for_udid(udid)
        .await
        .with_context(|| format!("process listing: resolve provider for device {udid}"))?;
    let (mut adapter, mut handshake) = connect_rsd(&provider)
        .await
        .context("process listing: establish CoreDevice/RSD tunnel")?;
    let mut app_service = AppServiceClient::connect_rsd(&mut adapter, &mut handshake)
        .await
        .context("process listing: connect to com.apple.coredevice.appservice")?;
    let processes = app_service
        .list_processes()
        .await
        .context("process listing: invoke com.apple.coredevice.feature.listprocesses")?;
    Ok(targets::running_processes(processes))
}

async fn launch_app(udid: &str, bundle_id: &str, start_suspended: bool) -> Result<u32> {
    let provider = provider_for_udid(udid)
        .await
        .with_context(|| format!("launch: resolve provider for device {udid}"))?;
    let (mut adapter, mut handshake) = connect_rsd(&provider)
        .await
        .context("launch: establish CoreDevice/RSD tunnel")?;
    let mut app_service = AppServiceClient::connect_rsd(&mut adapter, &mut handshake)
        .await
        .context("launch: connect to com.apple.coredevice.appservice")?;
    launch_application(&mut app_service, bundle_id, start_suspended)
        .await
        .with_context(|| format!("launch: launch application {bundle_id}"))
}

async fn attach_debugger(udid: &str, pid: u32) -> Result<ActiveDebugSession> {
    let provider = provider_for_udid(udid)
        .await
        .with_context(|| format!("attach: resolve provider for device {udid}"))?;
    let (mut adapter, mut handshake) = connect_rsd(&provider)
        .await
        .context("attach: establish CoreDevice/RSD tunnel")?;
    let mut debug_proxy = DebugProxyClient::connect_rsd(&mut adapter, &mut handshake)
        .await
        .context("attach: connect to com.apple.internal.dt.remote.debugproxy")?;
    configure_no_ack_mode(&mut debug_proxy)
        .await
        .context("attach: enable debugserver no-ack mode")?;
    Ok(ActiveDebugSession {
        pid,
        _adapter: adapter,
        debug_proxy: Arc::new(Mutex::new(debug_proxy)),
        stop_requested: Arc::new(AtomicBool::new(false)),
    })
}

async fn attach_and_detach(session: &ActiveDebugSession) -> Result<(String, String)> {
    let attach_response = send_debug_command(session, format!("vAttach;{:x}", session.pid)).await?;
    let detach_response = send_debug_command(session, "D").await?;
    Ok((attach_response, detach_response))
}

async fn send_debug_command(
    session: &ActiveDebugSession,
    command: impl Into<String>,
) -> Result<String> {
    let command = command.into();
    let mut debug_proxy = session.debug_proxy.lock().await;
    let response = debug_proxy
        .send_command(DebugserverCommand::from(command))
        .await?
        .unwrap_or_default();
    Ok(response)
}

async fn provider_for_udid(udid: &str) -> Result<UsbmuxdProvider> {
    let mut mux = UsbmuxdConnection::default().await?;
    let devices = mux.get_devices().await?;
    let device = devices
        .into_iter()
        .find(|device| device.udid == udid)
        .with_context(|| format!("device {udid} is no longer connected"))?;
    Ok(device.to_provider(UsbmuxdAddr::default(), LABEL))
}

async fn provider_for_udid_with_connection(
    udid: &str,
    connection: DeviceConnection,
) -> Result<UsbmuxdProvider> {
    let mut mux = UsbmuxdConnection::default().await?;
    let devices = mux.get_devices().await?;
    let device = devices
        .into_iter()
        .find(|device| {
            device.udid == udid && DeviceConnection::from(&device.connection_type) == connection
        })
        .with_context(|| {
            format!(
                "device {udid} is no longer connected via {}",
                connection.label()
            )
        })?;
    Ok(device.to_provider(UsbmuxdAddr::default(), LABEL))
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

impl DeviceConnection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::Network => "Network",
            Self::Other => "Other",
        }
    }
}

impl From<&Connection> for DeviceConnection {
    fn from(connection: &Connection) -> Self {
        match connection {
            Connection::Usb => Self::Usb,
            Connection::Network(_) => Self::Network,
            Connection::Unknown(_) => Self::Other,
        }
    }
}

fn check_status<T>(result: Result<T>) -> CheckStatus {
    match result {
        Ok(_) => CheckStatus::Success,
        Err(error) => CheckStatus::Failed(format_error(error)),
    }
}
