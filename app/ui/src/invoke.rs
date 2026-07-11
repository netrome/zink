//! The hand-rolled Tauri bridge: ~30 lines of wasm-bindgen externs instead
//! of a `tauri-sys` dependency (it has lagged Tauri majors before, and this
//! is all we need). `withGlobalTauri` puts `invoke` on the window object.

use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke, catch)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    /// Global `setInterval` — enough for the C3b foreground poll (C4
    /// replaces polling with forward-now push).
    #[wasm_bindgen(js_name = setInterval)]
    fn js_set_interval(handler: &js_sys::Function, timeout_ms: i32) -> i32;
}

/// Call a Tauri command with serializable args, deserialize the response.
/// Command errors (`Err(String)` on the Rust side) come back as `Err` here.
pub async fn invoke<R: DeserializeOwned>(cmd: &str, args: &impl Serialize) -> Result<R, String> {
    let args = serde_wasm_bindgen::to_value(args).map_err(|e| e.to_string())?;
    let result = tauri_invoke(cmd, args).await.map_err(display)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

/// Run `f` every `ms` milliseconds for the page's lifetime.
pub fn every(ms: i32, f: impl FnMut() + 'static) {
    let closure: Closure<dyn FnMut()> = Closure::new(f);
    js_set_interval(closure.as_ref().unchecked_ref(), ms);
    closure.forget(); // lives as long as the page — intentional leak
}

fn display(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}
