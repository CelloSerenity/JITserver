use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use anyhow::{Context as AnyhowContext, Result, anyhow};
use idevice::{ReadWrite, debug_proxy::DebugserverCommand};
use rquickjs::{CatchResultExt, Context, Error as JsError, Runtime, function::Func};
use tokio::sync::Mutex;

use crate::device::{self, WorkerEvent};

const PREPARE_MEMORY_REGION_PACKET_SIZE: usize = 19;
const PREPARE_MEMORY_REGION_BATCH_COMMANDS: usize = 128;

pub struct ScriptContext {
    pub name: String,
    pub pid: u32,
    pub debug_proxy: Arc<Mutex<idevice::debug_proxy::DebugProxyClient<Box<dyn ReadWrite>>>>,
    pub event_tx: mpsc::Sender<WorkerEvent>,
    pub tokio_handle: tokio::runtime::Handle,
    pub stop_requested: Arc<AtomicBool>,
}

pub fn run_script(script_context: ScriptContext, source: String) -> Result<()> {
    let runtime = Runtime::new().context("create JavaScript runtime")?;
    let context = Context::full(&runtime).context("create JavaScript context")?;
    let ScriptContext {
        name,
        pid,
        debug_proxy,
        event_tx,
        tokio_handle,
        stop_requested,
    } = script_context;

    let _ = event_tx.send(WorkerEvent::Log(format!("Running script: {name}")));

    context.with(|ctx| -> Result<()> {
        let globals = ctx.globals();

        let pid_value = pid;
        globals
            .set("get_pid", Func::from(move || pid_value))
            .context("register get_pid helper")?;

        let log_tx = event_tx.clone();
        globals
            .set(
                "log",
                Func::from(move |message: String| {
                    let _ = log_tx.send(WorkerEvent::Log(message));
                }),
            )
            .context("register log helper")?;

        let command_debug = Arc::clone(&debug_proxy);
        let command_handle = tokio_handle.clone();
        let command_stop = Arc::clone(&stop_requested);
        let command_event_tx = event_tx.clone();
        globals
            .set(
                "send_command",
                Func::from(move |command: String| -> rquickjs::Result<String> {
                    ensure_not_cancelled(&command_stop)?;
                    let cmd_clone = command.clone();
                    command_handle
                        .block_on(async {
                            let mut debug_proxy = command_debug.lock().await;
                            debug_proxy
                                .send_command(DebugserverCommand::from(command))
                                .await
                                .map(|response| response.unwrap_or_default())
                        })
                        .map(|response| {
                            if cmd_clone == "D" {
                                let _ = command_event_tx
                                    .send(WorkerEvent::DebugResponse(Ok(response.clone())));
                            }
                            response
                        })
                        .map_err(|error| {
                            JsError::new_from_js_message(
                                "debug command",
                                "string",
                                format!("{error:#}"),
                            )
                        })
                }),
            )
            .context("register send_command helper")?;

        let prepare_debug = Arc::clone(&debug_proxy);
        let prepare_handle = tokio_handle.clone();
        let prepare_stop = Arc::clone(&stop_requested);
        globals
            .set(
                "__prepare_memory_region_native",
                Func::from(
                    move |start_addr: u64, region_size: u64| -> rquickjs::Result<String> {
                        ensure_not_cancelled(&prepare_stop)?;
                        let packet_buffer =
                            device::prepare_memory_region_packets(start_addr, region_size);
                        prepare_handle
                            .block_on(async {
                                let mut debug_proxy = prepare_debug.lock().await;
                                if packet_buffer.is_empty() {
                                    return anyhow::Ok("OK".to_string());
                                }

                                let command_count =
                                    packet_buffer.len() / PREPARE_MEMORY_REGION_PACKET_SIZE;

                                for batch_start in
                                    (0..command_count).step_by(PREPARE_MEMORY_REGION_BATCH_COMMANDS)
                                {
                                    let commands_in_batch = std::cmp::min(
                                        PREPARE_MEMORY_REGION_BATCH_COMMANDS,
                                        command_count - batch_start,
                                    );
                                    let byte_start =
                                        batch_start * PREPARE_MEMORY_REGION_PACKET_SIZE;
                                    let byte_end = byte_start
                                        + (commands_in_batch * PREPARE_MEMORY_REGION_PACKET_SIZE);
                                    debug_proxy
                                        .send_raw(&packet_buffer[byte_start..byte_end])
                                        .await?;

                                    for _ in 0..commands_in_batch {
                                        debug_proxy.read_response().await?.ok_or_else(|| {
                                            anyhow!("missing debugserver response")
                                        })?;
                                    }
                                }
                                anyhow::Ok("OK".to_string())
                            })
                            .map_err(|error| {
                                JsError::new_from_js_message(
                                    "prepare memory region",
                                    "string",
                                    format!("{error:#}"),
                                )
                            })
                    },
                ),
            )
            .context("register prepare_memory_region helper")?;
        globals
            .set("hasTXM", Func::from(|| true))
            .context("register hasTXM helper")?;

        ctx.eval::<(), _>(
            r#"
globalThis.prepare_memory_region = function(start_addr, region_size) {
    return globalThis.__prepare_memory_region_native(Number(start_addr), Number(region_size));
};
"#,
        )
        .catch(&ctx)
        .map_err(|error| anyhow!("install JS helper prelude failed: {error}"))?;

        match ctx.eval::<(), _>(source.as_str()).catch(&ctx) {
            Ok(()) => Ok(()),
            Err(error) if is_stop_cancellation_error(&error) => Ok(()),
            Err(error) => Err(anyhow!("QuickJS script exception in {name}: {error}")),
        }
    })?;

    let _ = event_tx.send(WorkerEvent::Log("Script execution completed".to_string()));
    Ok(())
}

fn ensure_not_cancelled(stop_requested: &Arc<AtomicBool>) -> rquickjs::Result<()> {
    if stop_requested.load(Ordering::SeqCst) {
        Err(JsError::new_from_js_message(
            "script",
            "string",
            "script execution cancelled by stop",
        ))
    } else {
        Ok(())
    }
}

fn is_stop_cancellation_error(error: &impl ToString) -> bool {
    error
        .to_string()
        .contains("script execution cancelled by stop")
}
