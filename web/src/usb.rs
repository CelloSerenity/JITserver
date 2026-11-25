use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use idevice::{Idevice, services::lockdown::LockdownClient};
use netmuxd::usb::{
    apple,
    mux::{self, UsbMuxHandle},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::oneshot,
};
use wasm_bindgen::JsValue;

use crate::{LABEL, js_error};

const CLAIM_ATTEMPTS: usize = 4;
const START_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Transport {
    pub mux: UsbMuxHandle,
    exit: oneshot::Receiver<u64>,
    _stop_reader: oneshot::Sender<()>,
}

impl Transport {
    pub async fn open(info: &nusb::DeviceInfo) -> Result<Self, JsValue> {
        let mut last_error = JsValue::UNDEFINED;
        for attempt in 0..2 {
            if attempt > 0 {
                wasmtimer::tokio::sleep(Duration::from_millis(300)).await;
            }
            let opened = wasmtimer::tokio::timeout(
                START_TIMEOUT,
                apple::open_mux_with_retries(info, CLAIM_ATTEMPTS),
            )
            .await
            .map_err(|_| "Timed out opening the USB interface. Unplug and reconnect the device.")?
            .map_err(|error| {
                format!("Could not open the Apple USB interface: {error}. Close other apps using the device and reconnect.")
            })?;
            let (exit_tx, exit) = oneshot::channel();
            let (stop_reader, stop) = oneshot::channel();
            let mux = mux::spawn(
                1,
                info.serial_number().unwrap_or_default().to_owned(),
                StoppableReader {
                    reader: opened.reader,
                    stop,
                },
                opened.writer,
                exit_tx,
            );
            let transport = Self {
                mux,
                exit,
                _stop_reader: stop_reader,
            };
            let probe = async {
                let mut client = transport.lockdown().await?;
                client
                    .get_value(Some("ProductVersion"), None)
                    .await
                    .map_err(js_error)?;
                Ok::<_, JsValue>(())
            };
            match wasmtimer::tokio::timeout(START_TIMEOUT, probe).await {
                Ok(Ok(())) => return Ok(transport),
                Ok(Err(error)) => last_error = error,
                Err(_) => last_error = "The device did not respond to the USB handshake.".into(),
            }
        }
        Err(format!(
            "Could not reach the device over USB. Unlock it and reconnect the cable. {}",
            last_error.as_string().unwrap_or_default()
        )
        .into())
    }

    pub async fn lockdown(&self) -> Result<LockdownClient, JsValue> {
        let stream = self
            .mux
            .connect(LockdownClient::LOCKDOWND_PORT)
            .await
            .map_err(js_error)?;
        Ok(LockdownClient::new(Idevice::new(Box::new(stream), LABEL)))
    }

    pub fn ended(&mut self) -> bool {
        !matches!(
            self.exit.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        )
    }

    pub async fn disconnect(self) {
        self.mux.shutdown().await;
    }
}

// Cancel the USB read even if the mux task has yet to reach its command loop.
struct StoppableReader<R> {
    reader: R,
    stop: oneshot::Receiver<()>,
}

impl<R: AsyncRead + Unpin> AsyncRead for StoppableReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if Pin::new(&mut self.stop).poll(cx).is_ready() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}
