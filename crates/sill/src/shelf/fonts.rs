//! Sill's own faces (2026-11-01): Source Sans 3 for everything proportional, Source Code Pro for
//! everything fixed-width — terminals, code, badges, the editor. Both by Adobe, SIL Open Font
//! License 1.1 (`fonts/OFL-*.txt`, shipped unmodified, so the Reserved Font Name is untouched).
//! The files are embedded in the binary, written out under the state dir and registered with
//! fontconfig at start, so the shelf never reads the desktop's font setting and two machines draw
//! the same text the same way; the rasteriser settings are pinned for the same reason. The
//! families are named in [`super::theme::font`].

use std::path::PathBuf;

use gtk4 as gtk;

/// (file name, bytes): the production pair, variable fonts (every weight in one file) plus italics.
const FILES: &[(&str, &[u8])] = &[
    ("SourceSans3.ttf", include_bytes!("../../fonts/SourceSans3.ttf")),
    ("SourceSans3-Italic.ttf", include_bytes!("../../fonts/SourceSans3-Italic.ttf")),
    ("SourceCodePro.ttf", include_bytes!("../../fonts/SourceCodePro.ttf")),
    ("SourceCodePro-Italic.ttf", include_bytes!("../../fonts/SourceCodePro-Italic.ttf")),
    ("OFL-SourceSans3.txt", include_bytes!("../../fonts/OFL-SourceSans3.txt")),
    ("OFL-SourceCodePro.txt", include_bytes!("../../fonts/OFL-SourceCodePro.txt")),
];

/// Where the embedded files are written: `~/.local/state/sill/fonts/`.
fn dir() -> Option<PathBuf> {
    sill_core::store::default_path().ok().map(|p| p.with_file_name("fonts"))
}

/// Write the font files out (only what differs) and register the directory with fontconfig.
/// Before GTK makes its first font map, so the families are there when the stylesheet names them.
pub fn init() {
    let Some(dir) = dir() else { return };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("sill: fonts dir {}: {e}", dir.display());
        return;
    }
    for (name, bytes) in FILES {
        let path = dir.join(name);
        if std::fs::read(&path).map(|cur| cur == *bytes).unwrap_or(false) {
            continue;
        }
        if let Err(e) = std::fs::write(&path, bytes) {
            eprintln!("sill: write {}: {e}", path.display());
        }
    }
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else { return };
    // SAFETY: a NUL-terminated path for the current (null = default) fontconfig config.
    let ok = unsafe { fontconfig_sys::FcConfigAppFontAddDir(std::ptr::null_mut(), c.as_ptr().cast()) };
    if ok == 0 {
        eprintln!("sill: fonts in {} could not be registered", dir.display());
    }
}

/// Pin how text is rasterised — antialiased, lightly hinted, no subpixel colouring, a point being
/// 1/72 in at 96 dpi — instead of taking the machine's Xft/fontconfig preferences and text-scaling
/// factor, which differ from desktop to desktop. HiDPI is GDK's integer scale, applied on top.
pub fn pin_rendering() {
    if let Some(s) = gtk::Settings::default() {
        s.set_gtk_xft_antialias(1);
        s.set_gtk_xft_hinting(1);
        s.set_gtk_xft_hintstyle(Some("hintslight"));
        s.set_gtk_xft_rgba(Some("none"));
        s.set_gtk_xft_dpi(96 * 1024);
    }
}
