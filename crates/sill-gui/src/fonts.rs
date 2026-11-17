//! Sill's faces: Source Sans 3 for prose, Source Code Pro for
//! code, both bundled as variable fonts.
//!
//! fontdb registers a variable font once, at its default instance's weight — ExtraLight, 200, for
//! both. cosmic-text takes the requested family as its first choice only at an exact weight match,
//! so ordinary (400) and bold (700) text skipped the face and fell back to another font: Source
//! Code Pro was never used (found 2026-11-04 in the preview's renders). Registering the same data
//! again at 400 and 700 makes those matches exact; cosmic-text sets the `wght` axis from the
//! requested weight when it loads the face, so the glyphs come out at the right weight.
use std::borrow::Cow;
use std::sync::OnceLock;

use iced::advanced::graphics::text::{cosmic_text::fontdb, font_system};

const SANS: &[u8] = include_bytes!("../fonts/SourceSans3.ttf");
const SANS_ITALIC: &[u8] = include_bytes!("../fonts/SourceSans3-Italic.ttf");
const MONO: &[u8] = include_bytes!("../fonts/SourceCodePro.ttf");

/// The family names the views ask for.
pub const SANS_FAMILY: &str = "Source Sans 3";
pub const MONO_FAMILY: &str = "Source Code Pro";

/// Load the faces into iced's (process-wide) font system, once, before any text is laid out.
pub fn install() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let mut system = font_system().write().expect("the font system lock");
        for bytes in [SANS, SANS_ITALIC, MONO] {
            system.load_font(Cow::Borrowed(bytes));
        }
        let db = system.raw().db_mut();
        let variable: Vec<fontdb::FaceInfo> =
            db.faces().filter(|face| face.families.iter().any(|(name, _)| name == SANS_FAMILY || name == MONO_FAMILY)).cloned().collect();
        for face in variable {
            for weight in [fontdb::Weight::NORMAL, fontdb::Weight::BOLD] {
                if face.weight != weight {
                    let mut copy = face.clone();
                    copy.weight = weight;
                    db.push_face_info(copy);
                }
            }
        }
    });
}
