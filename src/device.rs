use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::{Context, Result};
use idevice::{
    IdeviceService, ReadWrite, RsdService,
    core_device::AppServiceClient,
    core_device_proxy::CoreDeviceProxy,
    debug_proxy::{DebugProxyClient, DebugserverCommand},
    installation_proxy::InstallationProxyClient,
    mobile_image_mounter::ImageMounter,
    provider::{IdeviceProvider, UsbmuxdProvider},
    rsd::RsdHandshake,
    services::lockdown::LockdownClient,
    tcp::handle::AdapterHandle,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection},
};
use tokio::{runtime::Runtime, sync::Mutex};

use crate::scripts;

const LABEL: &str = "idevice_debugger_gui";

mod embedded_ddi {
    include!(concat!(env!("OUT_DIR"), "/ddi_bundle.rs"));
}

#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub product_version: String,
    pub connection: String,
}

#[derive(Clone, Debug, Default)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceStatus {
    pub wireless_debugging: CheckStatus,
    pub developer_mode: CheckStatus,
    pub developer_disk_image: CheckStatus,
}

#[derive(Clone, Debug)]
pub enum CheckStatus {
    Success,
    Failed(String),
}

impl Default for CheckStatus {
    fn default() -> Self {
        Self::Failed("Not checked yet".to_string())
    }
}

#[derive(Debug)]
pub enum WorkerCommand {
    RefreshDevices,
    ListApps {
        udid: String,
    },
    ListProcesses {
        udid: String,
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
pub enum WorkerEvent {
    Busy(bool),
    Devices(Result<Vec<DeviceInfo>, String>),
    DeviceStatus(DeviceStatus),
    Apps(Result<Vec<AppInfo>, String>),
    Processes(Result<Vec<ProcessInfo>, String>),
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

    thread::spawn(move || {
        let runtime = Runtime::new().expect("failed to create tokio runtime");
        let mut worker = DeviceWorker {
            event_tx,
            active_debug: None,
        };

        while let Ok(command) = command_rx.recv() {
            let _ = worker.event_tx.send(WorkerEvent::Busy(true));
            runtime.block_on(worker.handle(command));
            let _ = worker.event_tx.send(WorkerEvent::Busy(false));
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
            WorkerCommand::RefreshDevices => {
                let result = refresh_devices().await.map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::Devices(result));
            }
            WorkerCommand::ListApps { udid } => {
                let device_status = inspect_device_status(&udid).await;
                let _ = self.event_tx.send(WorkerEvent::DeviceStatus(device_status));
                let result = list_apps(&udid).await.map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::Apps(result));
                let processes = list_processes(&udid).await.map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::Processes(processes));
            }
            WorkerCommand::ListProcesses { udid } => {
                let processes = list_processes(&udid).await.map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::Processes(processes));
            }
            WorkerCommand::LaunchAndAttach { udid, bundle_id } => {
                let result = async {
                    let pid = launch_app(&udid, &bundle_id, true).await?;
                    let session = attach_debugger(&udid, pid).await?;
                    let attach_response =
                        send_debug_command(&session, format!("vAttach;{pid:x}")).await?;
                    let detach_response = send_debug_command(&session, "D").await?;
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
                    let response = send_debug_command(&session, format!("vAttach;{pid:x}")).await?;
                    self.active_debug = Some(session);
                    Ok(response)
                }
                .await
                .map_err(format_error);
                let _ = self.event_tx.send(WorkerEvent::Attached(result));
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

async fn refresh_devices() -> Result<Vec<DeviceInfo>> {
    let mut mux = UsbmuxdConnection::default().await?;
    let devices = mux.get_devices().await?;
    let mut infos = Vec::with_capacity(devices.len());

    for device in devices {
        let provider = device.to_provider(UsbmuxdAddr::default(), LABEL);
        let (name, product_version) = match LockdownClient::connect(&provider).await {
            Ok(mut lockdown) => {
                if let Ok(pairing_file) = provider.get_pairing_file().await {
                    let _ = lockdown.start_session(&pairing_file).await;
                }
                let name = lockdown
                    .get_value(Some("DeviceName"), None)
                    .await
                    .ok()
                    .and_then(|value| value.as_string().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "Apple Device".to_string());
                let product_version = lockdown
                    .get_value(Some("ProductVersion"), None)
                    .await
                    .ok()
                    .and_then(|value| value.as_string().map(ToOwned::to_owned))
                    .unwrap_or_default();
                (name, product_version)
            }
            Err(_) => ("Apple Device".to_string(), String::new()),
        };

        infos.push(DeviceInfo {
            udid: device.udid,
            name,
            product_version,
            connection: connection_label(&device.connection_type),
        });
    }

    Ok(infos)
}

async fn list_apps(udid: &str) -> Result<Vec<AppInfo>> {
    let provider = provider_for_udid(udid).await?;
    let mut client = InstallationProxyClient::connect(&provider).await?;
    let apps = client.get_apps(Some("Any"), None).await?;
    let mut infos = apps
        .into_iter()
        .filter(|(_, value)| has_get_task_allow(value))
        .map(|(bundle_id, value)| {
            let dictionary = value.as_dictionary();
            let name = dictionary
                .and_then(|dict| {
                    dict.get("CFBundleDisplayName")
                        .or_else(|| dict.get("CFBundleName"))
                        .and_then(|value| value.as_string())
                })
                .unwrap_or(&bundle_id)
                .to_string();
            AppInfo { bundle_id, name }
        })
        .collect::<Vec<_>>();

    infos.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.bundle_id.cmp(&b.bundle_id))
    });
    Ok(infos)
}

async fn inspect_device_status(udid: &str) -> DeviceStatus {
    let provider = match provider_for_udid(udid).await {
        Ok(provider) => provider,
        Err(error) => {
            let error = format_error(error);
            return DeviceStatus {
                wireless_debugging: CheckStatus::Failed(error.clone()),
                developer_mode: CheckStatus::Failed(error.clone()),
                developer_disk_image: CheckStatus::Failed(error),
            };
        }
    };

    DeviceStatus {
        wireless_debugging: check_status(enable_wireless_debugging(&provider).await),
        developer_mode: check_status(query_developer_mode(&provider).await),
        developer_disk_image: check_status(ensure_developer_disk_image(&provider).await),
    }
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

async fn ensure_developer_disk_image(provider: &UsbmuxdProvider) -> Result<bool> {
    let mut mounter = ImageMounter::connect(provider).await?;
    if !mounter.copy_devices().await?.is_empty() {
        return Ok(true);
    }

    let ddi = load_ddi_bundle()?;
    let mut lockdown = LockdownClient::connect(provider).await?;
    let unique_chip_id = lockdown
        .get_value(Some("UniqueChipID"), None)
        .await?
        .as_unsigned_integer()
        .context("missing UniqueChipID in lockdown response")?;

    mounter
        .mount_personalized(
            provider,
            ddi.image,
            ddi.trust_cache,
            &ddi.build_manifest,
            None,
            unique_chip_id,
        )
        .await?;
    Ok(true)
}

struct DdiBundle {
    build_manifest: Vec<u8>,
    image: Vec<u8>,
    trust_cache: Vec<u8>,
}

fn load_ddi_bundle() -> Result<DdiBundle> {
    Ok(DdiBundle {
        build_manifest: embedded_ddi::BUILD_MANIFEST.to_vec(),
        image: embedded_ddi::IMAGE_DMG.to_vec(),
        trust_cache: embedded_ddi::IMAGE_TRUSTCACHE.to_vec(),
    })
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
    let mut infos = processes
        .into_iter()
        .map(|process| ProcessInfo {
            pid: process.pid,
            name: process_name(&process),
        })
        .collect::<Vec<_>>();

    infos.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.pid.cmp(&b.pid))
    });
    Ok(infos)
}

fn process_name(process: &idevice::core_device::ProcessToken) -> String {
    process
        .executable_url
        .as_ref()
        .and_then(|url| url.relative.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("Unknown Process")
        .to_string()
}

fn has_get_task_allow(value: &plist::Value) -> bool {
    value
        .as_dictionary()
        .and_then(|dictionary| dictionary.get("Entitlements"))
        .and_then(|entitlements| entitlements.as_dictionary())
        .and_then(|entitlements| entitlements.get("get-task-allow"))
        .and_then(|flag| flag.as_boolean())
        .unwrap_or(false)
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
    let response = app_service
        .launch_application(bundle_id, &[], true, start_suspended, None, None, None)
        .await
        .with_context(|| format!("launch: launch application {bundle_id}"))?;
    Ok(response.pid)
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

async fn configure_no_ack_mode(
    debug_proxy: &mut DebugProxyClient<Box<dyn ReadWrite>>,
) -> Result<()> {
    // Peer implementations prime the proxy with two ACKs before asking
    // debugserver to stop using the per-packet handshake.
    debug_proxy.send_ack().await?;
    debug_proxy.send_ack().await?;
    debug_proxy
        .send_command(DebugserverCommand::from("QStartNoAckMode"))
        .await?
        .unwrap_or_default();
    debug_proxy.set_ack_mode(false);
    Ok(())
}

async fn connect_rsd(provider: &UsbmuxdProvider) -> Result<(AdapterHandle, RsdHandshake)> {
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = proxy.create_software_tunnel()?;
    let mut adapter = adapter.to_async_handle();
    let stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(stream).await?;
    Ok((adapter, handshake))
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

pub fn prepare_memory_region_packets(start_addr: u64, region_size: u64) -> Vec<u8> {
    const JIT_PAGE_SIZE: u64 = 16 * 1024;
    if region_size == 0 {
        return Vec::new();
    }

    let command_count = region_size.div_ceil(JIT_PAGE_SIZE) as usize;
    let mut packets = Vec::with_capacity(command_count * 19);

    for index in 0..command_count {
        let address = start_addr + (index as u64 * JIT_PAGE_SIZE);
        let body = format!("M{address:x},1:69");
        let checksum = body.bytes().fold(0u8, |acc, byte| acc.wrapping_add(byte));
        let packet = format!("${body}#{checksum:02x}");
        packets.extend_from_slice(packet.as_bytes());
    }

    packets
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn connection_label(connection: &Connection) -> String {
    match connection {
        Connection::Usb => "USB".to_string(),
        Connection::Network(_) => "Network".to_string(),
        Connection::Unknown(_) => "Other".to_string(),
    }
}

fn check_status<T>(result: Result<T>) -> CheckStatus {
    match result {
        Ok(_) => CheckStatus::Success,
        Err(error) => CheckStatus::Failed(format_error(error)),
    }
}
