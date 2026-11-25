use idevice::{
    IdeviceError, IdeviceService, ReadWrite, RsdService,
    core_device::AppServiceClient,
    core_device_proxy::CoreDeviceProxy,
    cryptexd::{CryptexdClient, installed_ddi},
    debug_proxy::{DebugProxyClient, DebugserverCommand},
    mobile_image_mounter::ImageMounter,
    provider::IdeviceProvider,
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
};

pub async fn configure_no_ack_mode(
    debug_proxy: &mut DebugProxyClient<Box<dyn ReadWrite>>,
) -> Result<(), IdeviceError> {
    // Peer implementations prime the proxy with two ACKs before asking
    // debugserver to stop using the per-packet handshake.
    debug_proxy.send_ack().await?;
    debug_proxy.send_ack().await?;
    debug_proxy
        .send_command(DebugserverCommand::from("QStartNoAckMode"))
        .await?;
    debug_proxy.set_ack_mode(false);
    Ok(())
}

pub async fn connect_rsd(
    provider: &dyn IdeviceProvider,
) -> Result<(AdapterHandle, RsdHandshake), IdeviceError> {
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = proxy.create_software_tunnel()?;
    let mut adapter = adapter.to_async_handle();
    let stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(stream).await?;
    Ok((adapter, handshake))
}

pub async fn launch_application<R: ReadWrite>(
    app_service: &mut AppServiceClient<R>,
    bundle_id: &str,
    start_suspended: bool,
) -> Result<u32, IdeviceError> {
    let process = app_service
        .launch_application(bundle_id, &[], true, start_suspended, None, None, None)
        .await?;
    Ok(process.pid)
}

#[derive(Clone, Debug)]
pub enum RequiresScriptsStatus {
    Checking,
    Yes,
    No(String),
    Unknown(String),
}

impl Default for RequiresScriptsStatus {
    fn default() -> Self {
        Self::Checking
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn check_requires_scripts(
    lockdown: &mut idevice::services::lockdown::LockdownClient,
) -> Result<RequiresScriptsStatus, IdeviceError> {
    let product_type = lockdown.get_value(Some("ProductType"), None).await?;
    let product_version = lockdown.get_value(Some("ProductVersion"), None).await?;
    requires_scripts(
        product_type.as_string().unwrap_or_default(),
        product_version.as_string().unwrap_or_default(),
    )
}

pub fn requires_scripts(
    product_type: &str,
    product_version: &str,
) -> Result<RequiresScriptsStatus, IdeviceError> {
    if product_type.is_empty() {
        return Err(IdeviceError::UnexpectedResponse(
            "Missing ProductType".into(),
        ));
    }
    let version = ios_major_version(product_version).ok_or_else(|| {
        IdeviceError::UnexpectedResponse("Missing or invalid ProductVersion".into())
    })?;

    let has_txm = has_txm_hardware(product_type, version >= 27);
    let mut reasons = Vec::new();
    if !has_txm {
        reasons.push("No TXM detected");
    }
    if version < 26 {
        reasons.push("iOS is older than 26");
    }
    if reasons.is_empty() {
        Ok(RequiresScriptsStatus::Yes)
    } else {
        Ok(RequiresScriptsStatus::No(reasons.join(", ")))
    }
}

fn has_txm_hardware(product_type: &str, is_ios_27_or_newer: bool) -> bool {
    // On iOS 27+, every device has TXM except these two non-TXM iPad models.
    if is_ios_27_or_newer {
        return product_type != "iPad8,11" && product_type != "iPad8,12";
    }

    let num_start = match product_type.find(|c: char| c.is_ascii_digit()) {
        Some(i) => i,
        None => return false,
    };
    let prefix = product_type[..num_start].to_lowercase();
    let nums = &product_type[num_start..];
    let (major_str, rest) = match nums.split_once(',') {
        Some(pair) => pair,
        None => return false,
    };
    let minor_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let major: u32 = match major_str.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let minor: u32 = match minor_str.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    match prefix.as_str() {
        // A15 Bionic = iPhone14,2 (iPhone 13 Pro); anything higher also has TXM
        "iphone" => major > 14 || (major == 14 && minor >= 2),
        // M2 iPad = iPad14,5; anything higher also has TXM
        "ipad" => major > 14 || (major == 14 && minor >= 5),
        _ => false,
    }
}

fn ios_major_version(product_version: &str) -> Option<u32> {
    product_version
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
}

pub fn unsupported_ios_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().unwrap_or("0").parse::<u32>().ok();
    matches!(major, Some(major) if major < 17)
        || matches!((major, minor), (Some(17), Some(0..=3)))
}

pub const DDI_FILES: [&str; 5] = [
    "BuildManifest.plist",
    "Image.dmg",
    "Image.dmg.trustcache",
    "Image.dmg.cryptex_info",
    "Image.dmg.root_hash",
];

pub fn uses_cryptex_ddi(product_version: &str) -> bool {
    let mut parts = product_version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    matches!((major, minor), (Some(major), _) if major > 26)
        || matches!((major, minor), (Some(26), Some(minor)) if minor >= 4)
}

pub async fn unmount_ddi(
    provider: &dyn IdeviceProvider,
    version: &str,
) -> Result<(), IdeviceError> {
    let (mut adapter, mut handshake) = connect_rsd(provider).await?;
    if uses_cryptex_ddi(version) {
        let installed = installed_ddi(&mut adapter, &mut handshake)
            .await?
            .ok_or(IdeviceError::ImageNotMounted)?;
        let client = CryptexdClient::connect_rsd(&mut adapter, &mut handshake).await?;
        client
            .uninstall(&installed.identifier, Some(&installed.version))
            .await?;
    } else {
        let mut mounter = ImageMounter::connect_rsd(&mut adapter, &mut handshake).await?;
        mounter.unmount_image("/System/Developer").await?;
    }
    Ok(())
}

pub fn ddi_directory(version: &str, product_type: &str) -> Option<(&'static str, bool)> {
    let cryptex = uses_cryptex_ddi(version);
    if !["iPhone", "iPad", "iPod", "AppleTV"]
        .iter()
        .any(|prefix| product_type.starts_with(prefix))
    {
        return None;
    }
    let directory = match (product_type.starts_with("AppleTV"), cryptex) {
        (false, false) => "Xcode_iOS_DDI_Personalized",
        (false, true) => "Xcode_iOS_DDI_Cryptex",
        (true, false) => "Xcode_tvOS_DDI_Personalized",
        (true, true) => "Xcode_tvOS_DDI_Cryptex",
    };
    Some((directory, cryptex))
}

pub fn ddi_asset_url(directory: &str, name: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/PersonalizedImages/{directory}/{name}"
    )
}
