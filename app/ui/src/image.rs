//! Client-side image preparation and display. Downscaling happens here, on
//! a canvas — the webview already ships an image codec, so the Rust side
//! never needs one (no `image` crate anywhere).

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, CanvasRenderingContext2d, HtmlCanvasElement, ImageBitmap};
use zink_app_dto::OutgoingImage;

/// Thumbnails preview a conversation; full-res is what you tap into. Both
/// are re-encoded JPEG — bounded size regardless of what was picked.
const THUMB_MAX_PX: f64 = 320.0;
const FULL_MAX_PX: f64 = 1600.0;
const JPEG_QUALITY: f64 = 0.85;

/// Downscale a picked image file into the thumbnail + full-res pair to
/// send, plus a preview data URL for the composer.
pub async fn prepare(file: &Blob) -> Result<(OutgoingImage, String), String> {
    let bitmap = decode(file).await?;
    let (full_b64, _) = encode_scaled(&bitmap, FULL_MAX_PX)?;
    let (thumb_b64, preview) = encode_scaled(&bitmap, THUMB_MAX_PX)?;
    Ok((
        OutgoingImage {
            thumb_b64,
            full_b64,
        },
        preview,
    ))
}

/// A data URL for received blob bytes (base64). The mime is sniffed from
/// the base64 prefix (magic bytes map 1:1 onto it) — we send JPEG, but the
/// CLI attaches arbitrary files.
pub fn data_url(b64: &str) -> String {
    let mime = if b64.starts_with("iVBOR") {
        "image/png"
    } else if b64.starts_with("R0lGOD") {
        "image/gif"
    } else if b64.starts_with("UklGR") {
        "image/webp"
    } else {
        "image/jpeg" // "/9j/" — and the default guess
    };
    format!("data:{mime};base64,{b64}")
}

async fn decode(file: &Blob) -> Result<ImageBitmap, String> {
    let promise = web_sys::window()
        .ok_or("no window")?
        .create_image_bitmap_with_blob(file)
        .map_err(|_| "not a decodable image".to_string())?;
    JsFuture::from(promise)
        .await
        .map_err(|_| "could not decode the image".to_string())?
        .dyn_into::<ImageBitmap>()
        .map_err(|_| "unexpected decode result".to_string())
}

/// Draw the bitmap onto a canvas bounded by `max_px` on its long side
/// (never upscaled), JPEG-encode. Returns (base64 without prefix, data URL).
fn encode_scaled(bitmap: &ImageBitmap, max_px: f64) -> Result<(String, String), String> {
    let (width, height) = (f64::from(bitmap.width()), f64::from(bitmap.height()));
    let scale = (max_px / width.max(height)).min(1.0);
    let scaled = |side: f64| (side * scale).round().max(1.0);

    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or("no document")?;
    let canvas: HtmlCanvasElement = document
        .create_element("canvas")
        .map_err(|_| "create canvas")?
        .dyn_into()
        .map_err(|_| "canvas element type")?;
    canvas.set_width(scaled(width) as u32);
    canvas.set_height(scaled(height) as u32);
    let context: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .ok_or("no 2d context")?
        .dyn_into()
        .map_err(|_| "context type")?;
    context
        .draw_image_with_image_bitmap_and_dw_and_dh(bitmap, 0.0, 0.0, scaled(width), scaled(height))
        .map_err(|_| "draw image")?;

    let url = canvas
        .to_data_url_with_type_and_encoder_options("image/jpeg", &JsValue::from_f64(JPEG_QUALITY))
        .map_err(|_| "encode jpeg")?;
    let b64 = url
        .split_once("base64,")
        .ok_or("unexpected data URL shape")?
        .1
        .to_string();
    Ok((b64, url))
}
