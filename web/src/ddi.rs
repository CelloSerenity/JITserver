use idevice::{
    IdeviceError, IdeviceService, RsdService,
    cryptexd::{Cryptex1Assets, install_ddi, installed_ddi},
    mobile_image_mounter::ImageMounter,
    provider::IdeviceProvider,
    services::lockdown::LockdownClient,
    tss::select_cryptex_build_identity,
};
use netmuxd::usb::provider::UsbMuxProvider;
use wasm_bindgen::prelude::*;

use crate::{
    device_services::{DDI_FILES, connect_rsd, ddi_asset_url, ddi_directory, unmount_ddi},
    js_error,
};

#[wasm_bindgen(inline_js = r#"
export async function downloadDdiAsset(url) {
    const response = await fetch(url, {
        credentials: "omit",
        signal: AbortSignal.timeout(90000),
    });
    if (!response.ok) throw new Error("Image download returned HTTP " + response.status);
    const data = new Uint8Array(await response.arrayBuffer());
    if (!data.length) throw new Error("Image download was empty");
    return data;
}
"#)]
extern "C" {
    #[wasm_bindgen(catch, js_name = downloadDdiAsset)]
    async fn download_asset(url: &str) -> Result<js_sys::Uint8Array, JsValue>;
}

pub async fn lockdown_text(lockdown: &mut LockdownClient, key: &str) -> Result<String, JsValue> {
    lockdown
        .get_value(Some(key), None)
        .await
        .map_err(js_error)?
        .as_string()
        .map(str::to_owned)
        .ok_or_else(|| format!("Device did not report {key}").into())
}

async fn device_lockdown(provider: &UsbMuxProvider) -> Result<LockdownClient, JsValue> {
    let mut lockdown = LockdownClient::connect(provider).await.map_err(js_error)?;
    let pairing = provider.get_pairing_file().await.map_err(js_error)?;
    lockdown.start_session(&pairing).await.map_err(js_error)?;
    Ok(lockdown)
}

async fn personalized_image_mounted(provider: &UsbMuxProvider) -> Result<bool, JsValue> {
    let mut mounter = ImageMounter::connect(provider).await.map_err(js_error)?;
    match mounter.copy_devices().await {
        Ok(devices) => Ok(!devices.is_empty()),
        Err(IdeviceError::GetProhibited) => {
            let mut mounter = ImageMounter::connect(provider).await.map_err(js_error)?;
            match mounter.lookup_image("Personalized").await {
                Ok(_) => Ok(true),
                Err(IdeviceError::NotFound) => Ok(false),
                Err(error) => Err(js_error(error)),
            }
        }
        Err(error) => Err(js_error(error)),
    }
}

async fn image_mounted(provider: &UsbMuxProvider, cryptex: bool) -> Result<bool, JsValue> {
    if cryptex {
        let (mut adapter, mut handshake) = connect_rsd(provider).await.map_err(js_error)?;
        return Ok(installed_ddi(&mut adapter, &mut handshake)
            .await
            .map_err(js_error)?
            .is_some());
    }
    personalized_image_mounted(provider).await
}

pub async fn unmount(provider: &UsbMuxProvider) -> Result<(), JsValue> {
    let mut lockdown = device_lockdown(provider).await?;
    let version = lockdown_text(&mut lockdown, "ProductVersion").await?;
    drop(lockdown);
    unmount_ddi(provider, &version).await.map_err(js_error)
}

pub async fn mount(provider: &UsbMuxProvider, log: &dyn Fn(String)) -> Result<(), JsValue> {
    let mut lockdown = device_lockdown(provider).await?;
    let version = lockdown_text(&mut lockdown, "ProductVersion").await?;
    let product = lockdown_text(&mut lockdown, "ProductType").await?;
    let (directory, cryptex) = ddi_directory(&version, &product)
        .ok_or("Unsupported device type for developer disk images")?;
    if image_mounted(provider, cryptex).await? {
        return Ok(());
    }
    if cryptex
        && option_env!("JITSERVER_TSS_PROXY_URL")
            .unwrap_or_default()
            .is_empty()
    {
        return Err("No DDI signing proxy configured for this build".into());
    }
    let chip_id = if cryptex {
        None
    } else {
        Some(
            lockdown
                .get_value(Some("UniqueChipID"), None)
                .await
                .map_err(js_error)?
                .as_unsigned_integer()
                .ok_or("Device did not report its chip ID")?,
        )
    };
    drop(lockdown);
    let count = if cryptex { DDI_FILES.len() } else { 3 };
    let mut files = Vec::with_capacity(count);
    for name in &DDI_FILES[..count] {
        log(format!("Downloading {name}"));
        let data = download_asset(&ddi_asset_url(directory, name)).await?;
        files.push(data.to_vec());
    }
    let mut files = files.into_iter();
    let manifest = files.next().ok_or("Missing build manifest")?;
    let image = files.next().ok_or("Missing disk image")?;
    let trust_cache = files.next().ok_or("Missing trust cache")?;

    log("Personalizing and mounting developer disk image".into());
    let (mut adapter, mut handshake) = connect_rsd(provider).await.map_err(js_error)?;
    if cryptex {
        let manifest: plist::Dictionary = plist::from_bytes(&manifest).map_err(js_error)?;
        let identity = select_cryptex_build_identity(&manifest)
            .map_err(js_error)?
            .clone();
        let assets = Cryptex1Assets::from_parts(
            image,
            trust_cache,
            files.next().ok_or("Missing Cryptex image info")?,
            files.next().ok_or("Missing Cryptex root hash")?,
            identity,
        );
        install_ddi(&mut adapter, &mut handshake, &assets)
            .await
            .map_err(js_error)?;
    } else {
        let chip_id = chip_id.ok_or("Device did not report its chip ID")?;
        let mut mounter = ImageMounter::connect_rsd(&mut adapter, &mut handshake)
            .await
            .map_err(js_error)?;
        mounter
            .mount_personalized_rsd(
                &mut adapter,
                &mut handshake,
                image,
                trust_cache,
                &manifest,
                None,
                chip_id,
            )
            .await
            .map_err(js_error)?;
    }
    Ok(())
}
