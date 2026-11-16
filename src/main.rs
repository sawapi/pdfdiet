use std::path::PathBuf;

use clap::{Parser, Subcommand};
use lopdf::Document;
use pdfdiet::{CompressOptions, collect_images, compress_document};

#[derive(Parser)]
#[command(
    name = "pdfdiet",
    version,
    about = "Put PDFs on a diet: downsample and recompress embedded images (permissively licensed)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List embedded images with size, encoding and measured effective DPI
    Inspect { input: PathBuf },
    /// Downsample and recompress images, then rewrite the PDF
    Compress {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Target effective DPI; images rendered above this are downsampled
        #[arg(long, default_value_t = 150.0)]
        dpi: f64,
        /// JPEG quality for re-encoded images (1-100)
        #[arg(long, default_value_t = 80)]
        quality: u8,
        /// Process a document protected only by an owner password,
        /// stripping its permission restrictions
        #[arg(long)]
        strip_protection: bool,
        /// Process a digitally signed document, invalidating its signatures
        #[arg(long)]
        break_signatures: bool,
        /// Also drop the outlines of embedded TrueType glyphs that no text
        /// in the document uses (smaller, but the fonts can no longer show
        /// new text added later)
        #[arg(long)]
        subset_outlines: bool,
        /// Remove EXIF (GPS, camera, dates), XMP and other metadata from
        /// embedded JPEG images, losslessly
        #[arg(long)]
        strip_jpeg_metadata: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Inspect { input } => inspect(&input),
        Cmd::Compress {
            input,
            output,
            dpi,
            quality,
            strip_protection,
            break_signatures,
            subset_outlines,
            strip_jpeg_metadata,
        } => compress(
            &input,
            &output,
            dpi,
            quality,
            strip_protection,
            break_signatures,
            subset_outlines,
            strip_jpeg_metadata,
        ),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

type AnyError = Box<dyn std::error::Error>;

fn inspect(input: &PathBuf) -> Result<(), AnyError> {
    let doc = Document::load_with_options(input, pdfdiet::load_options())?;
    let images = collect_images(&doc);
    println!("{}", pdfdiet::format_inspect_table(&images));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compress(
    input: &PathBuf,
    output: &PathBuf,
    dpi: f64,
    quality: u8,
    strip_protection: bool,
    break_signatures: bool,
    subset_outlines: bool,
    strip_jpeg_metadata: bool,
) -> Result<(), AnyError> {
    let mut doc = Document::load_with_options(input, pdfdiet::load_options())?;
    let stats = compress_document(
        &mut doc,
        &CompressOptions {
            target_dpi: dpi,
            jpeg_quality: quality,
            strip_protection,
            break_signatures,
            subset_outlines,
            strip_jpeg_metadata,
        },
    )?;
    let mut out = std::fs::File::create(output)?;
    doc.save_with_options(&mut out, pdfdiet::compact_save_options())?;

    for line in &stats.log {
        println!("{line}");
    }
    println!(
        "\n{}/{} images recompressed, {} font subsets merged, {} fonts subsetted, {} fonts converted to CFF, {} bytes saved in streams",
        stats.images_recompressed,
        stats.images_total,
        stats.fonts_merged,
        stats.fonts_subset,
        stats.fonts_converted,
        stats.stream_bytes_saved
    );
    let in_size = std::fs::metadata(input)?.len();
    let out_size = std::fs::metadata(output)?.len();
    println!(
        "{} -> {} bytes ({:.1}%)",
        in_size,
        out_size,
        out_size as f64 / in_size as f64 * 100.0
    );
    Ok(())
}
