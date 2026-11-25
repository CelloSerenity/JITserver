use std::time::Duration;

use idevice::{
    IdeviceService, ReadWrite, RsdService,
    core_device::AppServiceClient,
    debug_proxy::{DebugProxyClient, DebugserverCommand},
    installation_proxy::InstallationProxyClient,
    mobile_image_mounter::ImageMounter,
    pairing_file::PairingFile,
    remote_pairing::{RemotePairingLockdownService, RpPairingFile},
    rsd::RsdHandshake,
    services::lockdown::LockdownClient,
    tcp::handle::AdapterHandle,
};
use netmuxd::usb::{
    apple::{self, APPLE_VID},
    provider::UsbMuxProvider,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{UsbDevice, UsbDeviceFilter, UsbDeviceRequestOptions};

const LABEL: &str = "JITserver Web";

mod app;
mod ddi;
#[path = "../../src/device_services.rs"]
mod device_services;
use device_services::{configure_no_ack_mode, connect_rsd, launch_application};
#[path = "../../src/jit_memory.rs"]
mod jit_memory;
#[path = "../../src/script_catalog.rs"]
mod script_catalog;
mod scripts;
#[path = "../../src/targets.rs"]
mod targets;
#[path = "../../src/ui.rs"]
mod ui;
mod usb;

#[wasm_bindgen(inline_js = r#"
export function downloadPairingFile(bytes) {
    const url = URL.createObjectURL(new Blob([bytes], { type: 'application/xml' }));
    const link = document.createElement('a');
    link.href = url;
    link.download = 'pairingFile.plist';
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
}
"#)]
extern "C" {
    #[wasm_bindgen(catch, js_name = downloadPairingFile)]
    fn download_pairing_file(bytes: &[u8]) -> Result<(), JsValue>;
}

#[wasm_bindgen]
pub async fn start(canvas: web_sys::HtmlCanvasElement) -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(|cc| {
                ui::setup(&cc.egui_ctx);
                Ok(Box::new(app::BrowserApp::new()))
            }),
        )
        .await
}

struct Device {
    transport: usb::Transport,
    pairing: Option<PairingFile>,
    pairing_key: Option<String>,
}

#[derive(Clone)]
struct DeviceInfo {
    name: String,
    model: String,
    version: String,
    build: Option<String>,
    udid: Option<String>,
    wireless_debugging: Option<bool>,
}

fn pairing_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn js_error(error: impl std::fmt::Debug) -> JsValue {
    JsValue::from_str(&format!("{error:?}"))
}

impl Device {
    pub async fn connect() -> Result<Device, JsValue> {
        let navigator = web_sys::window()
            .ok_or("Browser window unavailable")?
            .navigator();
        if !js_sys::Reflect::has(navigator.as_ref(), &"usb".into())? {
            return Err(
                "WebUSB is unavailable. Use a Chromium-based browser with WebUSB over HTTPS.".into(),
            );
        }
        let filter = UsbDeviceFilter::new();
        filter.set_vendor_id(APPLE_VID);
        let options = UsbDeviceRequestOptions::new(&[filter]);
        let selected: UsbDevice = JsFuture::from(navigator.usb().request_device(&options))
            .await?
            .dyn_into()?;
        let serial = selected.serial_number();
        let mut matches = nusb::list_devices()
            .await
            .map_err(js_error)?
            .filter(|info| {
                apple::is_apple_mux(info)
                    && info.product_id() == selected.product_id()
                    && serial.as_deref() == info.serial_number()
            });
        let info = matches
            .next()
            .ok_or("The selected device has no available USB mux interface.")?;
        if matches.next().is_some() {
            return Err("Disconnect the other matching Apple device and try again.".into());
        }

        let serial = info.serial_number().unwrap_or_default();
        let pairing_key = (!serial.is_empty())
            .then(|| format!("jitserver:pairing:v1:{:04x}:{serial}", info.product_id()));
        let pairing = pairing_key
            .as_deref()
            .and_then(|key| pairing_storage()?.get_item(key).ok().flatten())
            .and_then(|xml| PairingFile::from_bytes(xml.as_bytes()).ok());
        let transport = usb::Transport::open(&info).await?;
        Ok(Self {
            transport,
            pairing,
            pairing_key,
        })
    }

    pub async fn pair(&mut self) -> Result<bool, JsValue> {
        let mut client = wasmtimer::tokio::timeout(Duration::from_secs(5), self.lockdown())
            .await
            .map_err(
                |_| "The device did not open a pairing connection. Unlock it and reconnect.",
            )??;
        let host = uuid::Uuid::new_v4().to_string().to_uppercase();
        let system = uuid::Uuid::new_v4().to_string().to_uppercase();
        let pairing =
            wasmtimer::tokio::timeout(Duration::from_secs(120), client.pair(host, system, None))
                .await
                .map_err(|_| "Pairing timed out. Unlock your device, accept Trust, and try again.")?
                .map_err(js_error)?;
        let cached = self.pairing_key.as_deref().is_some_and(|key| {
            let Some(storage) = pairing_storage() else {
                return false;
            };
            pairing
                .clone()
                .serialize()
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .is_some_and(|xml| storage.set_item(key, &xml).is_ok())
        });
        self.pairing = Some(pairing);
        Ok(cached)
    }

    pub async fn export_pairing_file(&self) -> Result<(), JsValue> {
        let provider = self.provider()?;
        let mut file = RpPairingFile::generate(LABEL);
        wasmtimer::tokio::timeout(Duration::from_secs(30), async {
            for _ in 0..2 {
                RemotePairingLockdownService::connect(&provider)
                    .await?
                    .into_client(LABEL)?
                    .connect(&mut file, || async { "000000".to_owned() })
                    .await?;
            }
            Ok::<_, idevice::IdeviceError>(())
        })
        .await
        .map_err(|_| "Remote pairing timed out. Unlock the device and try again.")?
        .map_err(js_error)?;
        download_pairing_file(&file.to_bytes())
    }

    pub async fn developer_mode(&self) -> Result<bool, JsValue> {
        let provider = self.provider()?;
        let mut mounter = ImageMounter::connect(&provider).await.map_err(js_error)?;
        mounter
            .query_developer_mode_status()
            .await
            .map_err(js_error)
    }

    pub async fn info(&self) -> Result<DeviceInfo, JsValue> {
        let read_info = async {
            let mut lockdown = self.lockdown().await?;
            if let Some(pairing) = &self.pairing {
                lockdown.start_session(pairing).await.map_err(js_error)?;
            }
            let model = ddi::lockdown_text(&mut lockdown, "ProductType").await?;
            let version = ddi::lockdown_text(&mut lockdown, "ProductVersion").await?;
            let name = ddi::lockdown_text(&mut lockdown, "DeviceName")
                .await
                .unwrap_or_else(|_| "Apple Device".into());
            let build = ddi::lockdown_text(&mut lockdown, "BuildVersion").await.ok();
            let udid = ddi::lockdown_text(&mut lockdown, "UniqueDeviceID").await.ok();
            let wireless_debugging = if self.pairing.is_some() {
                lockdown
                    .get_value(
                        Some("EnableWifiDebugging"),
                        Some("com.apple.mobile.wireless_lockdown"),
                    )
                    .await
                    .ok()
                    .and_then(|value| value.as_boolean())
            } else {
                None
            };
            Ok(DeviceInfo {
                name,
                model,
                version,
                build,
                udid,
                wireless_debugging,
            })
        };
        wasmtimer::tokio::timeout(Duration::from_secs(5), read_info)
            .await
            .map_err(|_| JsValue::from_str("Device information request timed out"))?
    }

    pub async fn apps(&self) -> Result<Vec<targets::AppInfo>, JsValue> {
        let provider = self.provider()?;
        let mut proxy = InstallationProxyClient::connect(&provider)
            .await
            .map_err(js_error)?;
        let apps = proxy.get_apps(Some("Any"), None).await.map_err(js_error)?;
        Ok(targets::debuggable_apps(apps))
    }

    pub async fn processes(provider: UsbMuxProvider) -> Result<Vec<targets::ProcessInfo>, JsValue> {
        let (mut adapter, mut rsd) = connect_rsd(&provider).await.map_err(js_error)?;
        let mut app_service = AppServiceClient::connect_rsd(&mut adapter, &mut rsd)
            .await
            .map_err(js_error)?;
        let processes = app_service.list_processes().await.map_err(js_error)?;
        Ok(targets::running_processes(processes))
    }

    pub async fn launch_with_jit(&self, bundle_id: String) -> Result<u32, JsValue> {
        let (mut adapter, mut rsd) = connect_rsd(&self.provider()?).await.map_err(js_error)?;
        let mut proxy = open_debugger(&mut adapter, &mut rsd).await?;
        let mut app_service = AppServiceClient::connect_rsd(&mut adapter, &mut rsd)
            .await
            .map_err(js_error)?;
        let pid = launch_application(&mut app_service, &bundle_id, true)
            .await
            .map_err(js_error)?;
        attach_and_detach(&mut proxy, pid).await?;
        Ok(pid)
    }

    pub async fn attach_to_pid(&self, pid: u32) -> Result<(), JsValue> {
        let (mut adapter, mut rsd) = connect_rsd(&self.provider()?).await.map_err(js_error)?;
        let mut proxy = open_debugger(&mut adapter, &mut rsd).await?;
        attach_and_detach(&mut proxy, pid).await
    }

    pub async fn run_script(
        &self,
        bundle_id: Option<String>,
        pid: u32,
        script: script_catalog::BundledScript,
        log: std::rc::Rc<dyn Fn(String)>,
        stop: tokio::sync::watch::Receiver<bool>,
        attached: std::rc::Rc<std::cell::Cell<bool>>,
    ) -> Result<Option<u32>, JsValue> {
        let (mut adapter, mut rsd) = connect_rsd(&self.provider()?).await.map_err(js_error)?;
        let proxy = open_debugger(&mut adapter, &mut rsd).await?;
        if *stop.borrow() {
            return Ok(None);
        }
        let pid = if let Some(bundle_id) = bundle_id {
            let mut app_service = AppServiceClient::connect_rsd(&mut adapter, &mut rsd)
                .await
                .map_err(js_error)?;
            launch_application(&mut app_service, &bundle_id, true)
                .await
                .map_err(js_error)?
        } else {
            pid
        };
        let completed = script.run(proxy, pid, log, stop, attached).await?;
        Ok(completed.then_some(pid))
    }

    pub async fn disconnect(self) {
        self.transport.disconnect().await;
    }
}

impl Device {
    async fn lockdown(&self) -> Result<LockdownClient, JsValue> {
        self.transport.lockdown().await
    }

    fn provider(&self) -> Result<UsbMuxProvider, JsValue> {
        let pairing = self.pairing.as_ref().ok_or("Pair the device first.")?;
        Ok(UsbMuxProvider::new(
            self.transport.mux.clone(),
            pairing.clone(),
            LABEL,
        ))
    }
}

async fn open_debugger(
    adapter: &mut AdapterHandle,
    rsd: &mut RsdHandshake,
) -> Result<DebugProxyClient<Box<dyn ReadWrite>>, JsValue> {
    let mut proxy = DebugProxyClient::connect_rsd(adapter, rsd)
        .await
        .map_err(|error| format!("Could not start debugserver. Make sure the developer disk image is mounted. {error:?}"))?;
    configure_no_ack_mode(&mut proxy).await.map_err(js_error)?;
    Ok(proxy)
}

async fn attach_and_detach(
    proxy: &mut DebugProxyClient<Box<dyn ReadWrite>>,
    pid: u32,
) -> Result<(), JsValue> {
    let attached = proxy
        .send_command(DebugserverCommand::from(format!("vAttach;{pid:x}")))
        .await
        .map_err(js_error)?;
    if !attached
        .as_deref()
        .is_some_and(|reply| reply.starts_with(['T', 'S']))
    {
        return Err(format!("debugserver did not attach: {attached:?}").into());
    }
    let detached = proxy
        .send_command(DebugserverCommand::from("D"))
        .await
        .map_err(js_error)?;
    if detached.as_deref() != Some("OK") {
        return Err(format!("debugserver did not detach: {detached:?}").into());
    }
    Ok(())
}
