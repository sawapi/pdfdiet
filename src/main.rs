#![forbid(unsafe_code)]

use std::io::Write;
use std::path::{Path, PathBuf};

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
    input: &Path,
    output: &Path,
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
    let data = std::fs::read(input)?;
    let (pdf, stats) = compress_bytes(&data, &opts)?;
    write_replacing(output, &pdf)?;

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
    let (in_size, out_size) = (data.len(), pdf.len());
    println!(
        "{} -> {} bytes ({:.1}%)",
        in_size,
        out_size,
        out_size as f64 / in_size as f64 * 100.0
    );
    Ok(())
}

/// Write `data` to `path` through a temporary file in the same directory,
/// renamed over the target only once it is complete, so a failed write
/// (disk full, I/O error) never leaves a truncated file, even when the
/// output is the input. A symlink is followed and the target file keeps
/// its permissions.
fn write_replacing(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "output must name a file")
    })?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".pdfdiet-{}.tmp", std::process::id()));
    let tmp = target.with_file_name(tmp_name);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(data)?;
        if let Ok(meta) = std::fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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
    #[test]
    fn write_replacing_follows_symlinks_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("pdfdiet-wr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("out.pdf");
        super::write_replacing(&file, b"one").unwrap();
        super::write_replacing(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
            let link = dir.join("link.pdf");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            super::write_replacing(&link, b"three").unwrap();
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read(&file).unwrap(), b"three");
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o640);
        }
        assert!(super::write_replacing(&dir, b"x").is_err());
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
