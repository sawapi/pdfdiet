//! wasm-bindgen bindings for pdfdiet.
//!
//! These are internal: the npm package wraps them in a typed API
//! (npm/src), and the landing page demo (web/) uses that wrapper's browser
//! build. Results come back as classes holding wasm memory; the wrapper
//! copies them into plain objects and frees them.

use pdfdiet::{CompressOptions, Error, Refusal};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub struct CompressOutput {
    output: Vec<u8>,
    log: Vec<String>,
    images_total: u32,
    images_recompressed: u32,
}

#[wasm_bindgen]
impl CompressOutput {
    /// Move the PDF out (it is not copied inside wasm memory first, which
    /// matters for large documents); a second call returns nothing.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    #[wasm_bindgen(getter)]
    pub fn log(&self) -> Vec<String> {
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

/// Why a document was not processed, thrown in place of a result.
#[wasm_bindgen]
pub struct Failure {
    reason: &'static str,
    message: String,
}

#[wasm_bindgen]
impl Failure {
    /// One of "invalid-pdf", "encrypted", "protected", "signed", "empty".
    #[wasm_bindgen(getter)]
    pub fn reason(&self) -> String {
        self.reason.to_string()
    }

    #[wasm_bindgen(getter)]
    pub fn message(&self) -> String {
        self.message.clone()
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        let reason = match &e {
            Error::Pdf(_) => "invalid-pdf",
            Error::Refused(Refusal::Encrypted) => "encrypted",
            Error::Refused(Refusal::Protected) => "protected",
            Error::Refused(Refusal::Signed) => "signed",
            Error::Refused(Refusal::Empty) => "empty",
            // Error and Refusal are non_exhaustive; a new kind added
            // without updating this is still reported, as unreadable.
            _ => "invalid-pdf",
        };
        Failure {
            reason,
            message: e.to_string(),
        }
    }
}

/// Compress a PDF and return the new PDF bytes plus statistics.
/// The trailing flags opt into destructive processing (stripping
/// owner-password protection / invalidating digital signatures) and into
/// dropping unused glyph outlines (CompressOptions::subset_outlines) and
/// JPEG metadata (CompressOptions::strip_jpeg_metadata).
#[wasm_bindgen]
pub fn compress_pdf(
    input: &[u8],
    dpi: f64,
    quality: u8,
    strip_protection: bool,
    break_signatures: bool,
    subset_outlines: bool,
    strip_jpeg_metadata: bool,
) -> Result<CompressOutput, Failure> {
    let opts = CompressOptions::default()
        .target_dpi(dpi)
        .jpeg_quality(quality)
        .strip_protection(strip_protection)
        .break_signatures(break_signatures)
        .subset_outlines(subset_outlines)
        .strip_jpeg_metadata(strip_jpeg_metadata);
    let (output, stats) = pdfdiet::compress_bytes(input, &opts)?;
    Ok(CompressOutput {
        output,
        log: stats.log,
        images_total: stats.images_total as u32,
        images_recompressed: stats.images_recompressed as u32,
    })
}

/// One image XObject, as listed by `inspect_images`.
#[wasm_bindgen]
pub struct ImageEntry {
    info: pdfdiet::ImageInfo,
}

#[wasm_bindgen]
impl ImageEntry {
    #[wasm_bindgen(getter)]
    pub fn object_number(&self) -> u32 {
        self.info.object_id().0
    }

    #[wasm_bindgen(getter)]
    pub fn generation(&self) -> u16 {
        self.info.object_id().1
    }

    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.info.width()
    }

    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.info.height()
    }

    #[wasm_bindgen(getter)]
    pub fn bits_per_component(&self) -> u8 {
        self.info.bits_per_component()
    }

    #[wasm_bindgen(getter)]
    pub fn color_space(&self) -> String {
        self.info.color_space().to_string()
    }

    #[wasm_bindgen(getter)]
    pub fn components(&self) -> Option<u8> {
        self.info.components()
    }

    #[wasm_bindgen(getter)]
    pub fn filters(&self) -> Vec<String> {
        self.info.filters().to_vec()
    }

    #[wasm_bindgen(getter)]
    pub fn bytes(&self) -> f64 {
        self.info.stream_len() as f64
    }

    #[wasm_bindgen(getter)]
    pub fn width_inch(&self) -> Option<f64> {
        self.info.drawn_size().map(|d| d.0)
    }

    #[wasm_bindgen(getter)]
    pub fn height_inch(&self) -> Option<f64> {
        self.info.drawn_size().map(|d| d.1)
    }

    #[wasm_bindgen(getter)]
    pub fn dpi_x(&self) -> Option<f64> {
        self.info.dpi().map(|d| d.0)
    }

    #[wasm_bindgen(getter)]
    pub fn dpi_y(&self) -> Option<f64> {
        self.info.dpi().map(|d| d.1)
    }
}

/// List the images in a PDF with how large each is drawn.
#[wasm_bindgen]
pub fn inspect_images(input: &[u8]) -> Result<Vec<ImageEntry>, Failure> {
    let images = pdfdiet::inspect_bytes(input)?;
    Ok(images.into_iter().map(|info| ImageEntry { info }).collect())
}

/// The same list as a formatted table (what the CLI prints).
#[wasm_bindgen]
pub fn inspect_table(input: &[u8]) -> Result<String, Failure> {
    let images = pdfdiet::inspect_bytes(input)?;
    Ok(pdfdiet::format_inspect_table(&images))
}
