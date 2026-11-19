use idevice::{IdeviceError, ReadWrite, debug_proxy::DebugProxyClient};

pub async fn prepare(
    proxy: &mut DebugProxyClient<Box<dyn ReadWrite>>,
    start: u64,
    size: u64,
) -> Result<(), IdeviceError> {
    const PAGE_SIZE: u64 = 16 * 1024;
    const BATCH_SIZE: u64 = 128;

    let pages = size.div_ceil(PAGE_SIZE);
    if pages == 0 {
        return Ok(());
    }
    start.checked_add((pages - 1) * PAGE_SIZE).ok_or_else(|| {
        IdeviceError::UnexpectedResponse("JIT memory region exceeds the address range".into())
    })?;

    let mut packets = Vec::new();
    for first in (0..pages).step_by(BATCH_SIZE as usize) {
        let end = (first + BATCH_SIZE).min(pages);
        packets.clear();
        for page in first..end {
            let address = start + page * PAGE_SIZE;
            let body = format!("M{address:x},1:69");
            let checksum = body.bytes().fold(0u8, |sum, byte| sum.wrapping_add(byte));
            packets.extend_from_slice(format!("${body}#{checksum:02x}").as_bytes());
        }
        proxy.send_raw(&packets).await?;
        for _ in first..end {
            proxy.read_response().await?.ok_or_else(|| {
                IdeviceError::UnexpectedResponse(
                    "Missing debugserver response while preparing JIT memory".into(),
                )
            })?;
        }
    }
    Ok(())
}
