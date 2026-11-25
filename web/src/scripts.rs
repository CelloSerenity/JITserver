use std::{cell::Cell, rc::Rc, time::Duration};

use futures::future::{Either, select};
use tokio::sync::watch;

use idevice::{
    ReadWrite,
    debug_proxy::{DebugProxyClient, DebugserverCommand},
};
use wasm_bindgen::{closure::Closure, prelude::*};
use wasm_bindgen_futures::{JsFuture, future_to_promise};

use crate::{jit_memory, js_error, script_catalog::BundledScript};

#[wasm_bindgen(inline_js = r#"
export async function executeScript(source, pid, command, prepare, log) {
    const get_pid = () => pid;
    const send_command = command;
    const hasTXM = () => true;
    const prepare_memory_region = (start, size) => prepare(BigInt(start), BigInt(size));
    const adapt = text => text
        .replace(/\b(send_command|prepare_memory_region)\s*\(/g, 'await $1(')
        .replace(/function (attach|JIT26\w+|runScriptAndCapture)\(/g, 'async function $1(')
        .replace(/\bcommand\(brkResponse\)/g, 'await command(brkResponse)')
        .replace(/const res = runScriptAndCapture\(/g, 'const res = await runScriptAndCapture(')
        .replace('const value = eval(scriptText);', 'const value = await eval("(async () => {" + adapt(scriptText) + "\\n})()");')
        .replace(/^attach\((\d+)\);/gm, 'await attach($1);');
    await eval('(async () => {' + adapt(source) + '\n})()');
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = executeScript)]
    fn execute_script(
        source: &str,
        pid: u32,
        command: &js_sys::Function,
        prepare: &js_sys::Function,
        log: &js_sys::Function,
    ) -> js_sys::Promise;
}

impl BundledScript {
    pub async fn run(
        self,
        proxy: DebugProxyClient<Box<dyn ReadWrite>>,
        pid: u32,
        log: Rc<dyn Fn(String)>,
        stop: watch::Receiver<bool>,
        attached: Rc<Cell<bool>>,
    ) -> Result<bool, JsValue> {
        let proxy = Rc::new(tokio::sync::Mutex::new(proxy));
        let detached = Rc::new(Cell::new(false));
        let command_proxy = Rc::clone(&proxy);
        let command_detached = Rc::clone(&detached);
        let command_stop = stop.clone();
        let command_attached = Rc::clone(&attached);
        let running = Rc::new(Cell::new(false));
        let command_running = Rc::clone(&running);
        let command = Closure::<dyn Fn(String) -> js_sys::Promise>::new(move |command: String| {
            let proxy = Rc::clone(&command_proxy);
            let detached = Rc::clone(&command_detached);
            let mut stop = command_stop.clone();
            let attached = Rc::clone(&command_attached);
            let running = Rc::clone(&command_running);
            future_to_promise(async move {
                if *stop.borrow() && !command.starts_with("vAttach;") {
                    return Err("Script stopped".into());
                }
                let mut proxy = proxy.lock().await;
                let request = DebugserverCommand::from(command.clone());
                let response = if command == "c" || command.starts_with("vCont;") {
                    running.set(true);
                    match select(
                        Box::pin(proxy.send_command(request)),
                        Box::pin(stop.wait_for(|requested| *requested)),
                    )
                    .await
                    {
                        Either::Left((response, _)) => {
                            running.set(false);
                            response
                        }
                        Either::Right(_) => return Err("Script stopped".into()),
                    }
                } else {
                    proxy.send_command(request).await
                }
                .map_err(js_error)?
                .unwrap_or_default();
                if command == "D" && response == "OK" {
                    detached.set(true);
                    attached.set(false);
                }
                if command.starts_with("vAttach;") {
                    if !response.starts_with(['T', 'S']) {
                        return Err(format!("debugserver did not attach: {response}").into());
                    }
                    attached.set(true);
                }
                if command == "D" && response != "OK" {
                    return Err(format!("debugserver did not detach: {response}").into());
                }
                if matches!(response.as_bytes().first(), Some(b'W' | b'X')) {
                    return Err(format!("Target process exited: {response}").into());
                }
                Ok(response.into())
            })
        });
        let prepare_proxy = Rc::clone(&proxy);
        let prepare_stop = stop.clone();
        let prepare = Closure::<dyn Fn(u64, u64) -> js_sys::Promise>::new(move |start, size| {
            let proxy = Rc::clone(&prepare_proxy);
            let stop = prepare_stop.clone();
            future_to_promise(async move {
                if *stop.borrow() {
                    return Err("Script stopped".into());
                }
                let mut proxy = proxy.lock().await;
                jit_memory::prepare(&mut *proxy, start, size)
                    .await
                    .map_err(js_error)?;
                Ok("OK".into())
            })
        });
        log(format!("Running script: {}", self.name()));
        let logger = Closure::<dyn Fn(String)>::new(move |message| log(message));
        let result = JsFuture::from(execute_script(
            self.source(),
            pid,
            command.as_ref().unchecked_ref(),
            prepare.as_ref().unchecked_ref(),
            logger.as_ref().unchecked_ref(),
        ))
        .await;
        let cleanup = if !detached.get() && attached.get() {
            wasmtimer::tokio::timeout(Duration::from_secs(5), async {
                let mut proxy = proxy.lock().await;
                if running.get() {
                    proxy.send_raw(b"\x03").await.map_err(js_error)?;
                    loop {
                        if let Some(response) = proxy.read_response().await.map_err(js_error)? {
                            if response.starts_with(['W', 'X']) {
                                return Ok::<_, JsValue>(());
                            }
                            if response.starts_with(['T', 'S']) {
                                break;
                            }
                        }
                    }
                }
                let response = proxy
                    .send_command(DebugserverCommand::from("D"))
                    .await
                    .map_err(js_error)?;
                if response.as_deref() != Some("OK") {
                    return Err(format!("debugserver did not detach: {response:?}").into());
                }
                Ok(())
            })
            .await
            .map_err(|_| JsValue::from_str("Timed out detaching the debugger"))
            .and_then(|result| result)
        } else {
            Ok(())
        };
        attached.set(false);
        if *stop.borrow() {
            cleanup?;
            Ok(false)
        } else {
            result?;
            cleanup?;
            Ok(true)
        }
    }
}
