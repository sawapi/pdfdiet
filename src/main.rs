#![forbid(unsafe_code)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pdfdiet::{CompressOptions, compress_bytes, format_inspect_table, inspect_bytes};

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
        #[arg(long, default_value_t = 150.0, value_parser = positive_dpi)]
        dpi: f64,
        /// JPEG quality for re-encoded images (1-100)
        #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
        quality: u8,
        /// Process a document protected only by an owner password,
        /// stripping its permission restrictions
        #[arg(long)]
        strip_protection: bool,
        /// Process a digitally signed document, invalidating its signatures
        #[arg(long)]
        break_signatures: bool,
        /// Also drop the outlines of embedded glyphs (TrueType, and CID-keyed
        /// CFF) that no text in the document uses (smaller, but the fonts
        /// can no longer show new text added later)
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
        // The library's refusals say what would happen; the CLI also says
        // which flag allows it.
        let hint = match e.downcast_ref::<pdfdiet::Error>() {
            Some(pdfdiet::Error::Refused(pdfdiet::Refusal::Protected)) => {
                " — pass --strip-protection to do that explicitly"
            }
            Some(pdfdiet::Error::Refused(pdfdiet::Refusal::Signed)) => {
                " — pass --break-signatures to do that explicitly"
            }
            _ => "",
        };
        eprintln!("error: {}{hint}", printable(&e.to_string()));
        std::process::exit(1);
    }
}

fn positive_dpi(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(v),
        _ => Err("must be a positive number".into()),
    }
}

type AnyError = Box<dyn std::error::Error>;

/// Text for the terminal with control characters (other than newline and
/// tab) escaped: names in the log and the table come from the document,
/// and a PDF name can hold any byte, including escape sequences.
fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

fn inspect(input: &PathBuf) -> Result<(), AnyError> {
    let images = inspect_bytes(&std::fs::read(input)?)?;
    println!("{}", printable(&format_inspect_table(&images)));
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
    let opts = CompressOptions::default()
        .target_dpi(dpi)
        .jpeg_quality(quality)
        .strip_protection(strip_protection)
        .break_signatures(break_signatures)
        .subset_outlines(subset_outlines)
        .strip_jpeg_metadata(strip_jpeg_metadata);
    let (pdf, stats) = compress_bytes(&std::fs::read(input)?, &opts)?;
    std::fs::write(output, pdf)?;

    for line in &stats.log {
        println!("{}", printable(line));
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

#[cfg(test)]
mod tests {
    #[test]
    fn printable_escapes_terminal_controls() {
        assert_eq!(
            super::printable("a\u{1b}[2Jb\nc\td\u{9b}"),
            "a\\u{1b}[2Jb\nc\td\\u{9b}"
        );
    }
}
