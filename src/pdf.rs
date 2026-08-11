//! First-page rendering for PDFs, using the renderer that ships with Windows.
//!
//! The shell route (`IThumbnailProvider`) only works when something registered
//! a thumbnail handler for `.pdf` — Acrobat, or a reader that installs one. On a
//! machine where the browser took the file association without registering a
//! handler, `IShellItemImageFactory` quietly falls back to the generic file
//! icon, which is exactly what a PDF preview looked like.
//!
//! `Windows.Data.Pdf` is the renderer behind Edge and the built-in PDF viewer.
//! It is part of the OS, needs nothing installed, and does not care about file
//! associations at all.

use std::time::Duration;

use windows::core::{Interface, HSTRING};
use windows::Data::Pdf::PdfDocument;
use windows::Storage::StorageFile;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows_future::{AsyncStatus, IAsyncInfo};

use crate::winshell::Thumb;

pub fn is_pdf(name: &str) -> bool {
    // `rsplit` on a name with no dot hands back the whole name, so a file
    // called "pdf" would otherwise be taken for one.
    name.rsplit_once('.')
        .is_some_and(|(_, e)| e.eq_ignore_ascii_case("pdf"))
}

/// Renders page one, scaled to fit a `px` box. `None` for anything that is not
/// a readable PDF — an encrypted or damaged file included.
pub fn first_page(path: &str, px: u32) -> Option<Thumb> {
    let bytes = render_bytes(path).ok()?;
    // The stream holds an encoded bitmap; which encoding is up to Windows, so
    // let the decoder work it out rather than assuming one.
    let img = image::load_from_memory(&bytes).ok()?;
    let img = if img.width().max(img.height()) > px {
        img.thumbnail(px, px)
    } else {
        img
    };
    let img = img.to_rgba8();
    let (w, h) = (img.width(), img.height());
    let mut rgba = img.into_raw();
    // The rest of the preview path works in premultiplied alpha, matching what
    // GDI hands back for shell thumbnails.
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * a) / 255) as u8;
            }
        }
    }
    Some(Thumb { w, h, rgba })
}

/// Blocks until a WinRT async operation finishes.
///
/// `windows-future` keeps its blocking `join` private and offers only
/// `IntoFuture`, which would drag in an async runtime for three calls. Polling
/// the status is honest and costs nothing here: these operations finish in
/// milliseconds and the caller is already a background worker.
fn wait(op: &impl Interface) -> windows::core::Result<()> {
    let info: IAsyncInfo = op.cast()?;
    loop {
        match info.Status()? {
            AsyncStatus::Started => std::thread::sleep(Duration::from_millis(1)),
            AsyncStatus::Completed => return Ok(()),
            _ => return Err(info.ErrorCode().err().unwrap_or_else(windows::core::Error::empty)),
        }
    }
}

fn render_bytes(path: &str) -> windows::core::Result<Vec<u8>> {
    let op = StorageFile::GetFileFromPathAsync(&HSTRING::from(path))?;
    wait(&op)?;
    let file = op.GetResults()?;

    let op = PdfDocument::LoadFromFileAsync(&file)?;
    wait(&op)?;
    let doc = op.GetResults()?;
    if doc.PageCount()? == 0 {
        return Err(windows::core::Error::empty());
    }
    let page = doc.GetPage(0)?;

    // Rendered at the page's natural size: the overload that takes render
    // options is not in these bindings, and scaling afterwards costs less than
    // it would to work around that.
    let stream = InMemoryRandomAccessStream::new()?;
    let op = page.RenderToStreamAsync(&stream)?;
    wait(&op)?;
    op.GetResults()?;

    let size = stream.Size()? as u32;
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
    let op = reader.LoadAsync(size)?;
    wait(&op)?;
    op.GetResults()?;

    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #[test]
    fn extension_check_is_case_insensitive() {
        assert!(super::is_pdf("Report.PDF"));
        assert!(super::is_pdf("a.pdf"));
        assert!(!super::is_pdf("a.pdfx"));
        assert!(!super::is_pdf("pdf"));
    }

    /// `cargo test --lib -- --ignored renders_a_pdf --nocapture`
    #[test]
    #[ignore = "needs a PDF on disk"]
    fn renders_a_pdf() {
        crate::winshell::init_com();
        let path = std::env::var("DKZ_TEST_PDF").expect("set DKZ_TEST_PDF");
        let t = super::first_page(&path, 256).expect("must render");
        println!("{path} -> {}x{} px, {} bytes", t.w, t.h, t.rgba.len());
        assert!(t.w > 0 && t.h > 0);
        assert!(t.w <= 256 && t.h <= 256, "not scaled to the requested box");
        assert_eq!(t.rgba.len() as u32, t.w * t.h * 4);
        // An all-white result would mean the page never made it into the bitmap.
        assert!(
            t.rgba
                .chunks_exact(4)
                .any(|p| p[0] != 255 || p[1] != 255 || p[2] != 255),
            "rendered page is entirely white"
        );
    }
}
