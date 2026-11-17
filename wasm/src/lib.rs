//! wasm-bindgen bindings for pdfdiet.
//! Used by the npm package and the landing page demo (web/).

use pdfdiet::CompressOptions;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub struct CompressOutput {
    output: Vec<u8>,
    log: String,
    images_total: u32,
    images_recompressed: u32,
}

#[wasm_bindgen]
impl CompressOutput {
    #[wasm_bindgen(getter)]
    pub fn output(&self) -> Vec<u8> {
        self.output.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn log(&self) -> String {
        self.log.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn images_total(&self) -> u32 {
        self.images_total
    }

    #[wasm_bindgen(getter)]
    pub fn images_recompressed(&self) -> u32 {
        self.images_recompressed
    }
}

/// Compress a PDF and return the new PDF bytes plus statistics.
/// The trailing flags opt into destructive processing (stripping
/// owner-password protection / invalidating digital signatures) and into
/// dropping unused glyph outlines (CompressOptions::subset_outlines) and
/// JPEG metadata (CompressOptions::strip_jpeg_metadata); all default to
/// false when omitted.
#[wasm_bindgen]
pub fn compress_pdf(
    input: &[u8],
    dpi: f64,
    quality: u8,
    strip_protection: Option<bool>,
    break_signatures: Option<bool>,
    subset_outlines: Option<bool>,
    strip_jpeg_metadata: Option<bool>,
) -> Result<CompressOutput, JsValue> {
    let opts = CompressOptions {
        target_dpi: dpi,
        jpeg_quality: quality,
        strip_protection: strip_protection.unwrap_or(false),
        break_signatures: break_signatures.unwrap_or(false),
        subset_outlines: subset_outlines.unwrap_or(false),
        strip_jpeg_metadata: strip_jpeg_metadata.unwrap_or(false),
    };
    let (output, stats) =
        pdfdiet::compress_bytes(input, &opts).map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(CompressOutput {
        output,
        log: stats.log.join("\n"),
        images_total: stats.images_total as u32,
        images_recompressed: stats.images_recompressed as u32,
    })
}

/// Return the list of images in a PDF as a formatted table string.
#[wasm_bindgen]
pub fn inspect_pdf(input: &[u8]) -> Result<String, JsValue> {
    let images = pdfdiet::inspect_bytes(input).map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(pdfdiet::format_inspect_table(&images))
}
