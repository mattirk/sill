//! Icons as SVG handles, made once: the herring (`packaging/icons/herring/herring-silhouette.svg`),
//! the agents' provider glyphs (simple-icons, CC0 1.0 — the same paths as `shelf/icons.rs`) and
//! the composer's send and stop shapes, and the shelf's lucide line glyphs. All are
//! single-colour; the `svg` widget recolours them.
use std::sync::OnceLock;

use iced::widget::svg::Handle;

const HERRING: &str = "M1.5 12C4.5 6.5 12 5 17.5 10.5L22.5 6.5 20.5 12 22.5 17.5 17.5 13.5C12 19 4.5 17.5 1.5 12ZM4.7 11a1.3 1.3 0 1 0 2.6 0a1.3 1.3 0 1 0-2.6 0";
const CLAUDE: &str = "m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z";
const OPENAI: &str = "M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z";
const GEMINI: &str = "M11.04 19.32Q12 21.51 12 24q0-2.49.93-4.68.96-2.19 2.58-3.81t3.81-2.55Q21.51 12 24 12q-2.49 0-4.68-.93a12.3 12.3 0 0 1-3.81-2.58 12.3 12.3 0 0 1-2.58-3.81Q12 2.49 12 0q0 2.49-.96 4.68-.93 2.19-2.55 3.81a12.3 12.3 0 0 1-3.81 2.58Q2.49 12 0 12q2.49 0 4.68.96 2.19.93 3.81 2.55t2.55 3.81";
/// "Some agent": a ring with a dot.
const AGENT: &str = "M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 2a8 8 0 1 1 0 16 8 8 0 0 1 0-16zm0 5a3 3 0 1 0 0 6 3 3 0 0 0 0-6z";
/// A ghost: a dome with a wavy hem and two eyes (a ghost session leaves nothing behind).
const GHOST: &str = "M12 2.5a7 7 0 0 0-7 7V21l2.4-1.8 2.3 1.8 2.3-1.8 2.3 1.8 2.3-1.8L19 21V9.5a7 7 0 0 0-7-7zM9.5 8a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3zm5 0a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3z";
/// A plus, for the new-session menu.
const PLUS: &str = "M11 4h2v7h7v2h-7v7h-2v-7H4v-2h7z";
/// The send arrow and the stop square, on the shelf's 24-unit grid.
const SEND: &str = "M5 4.5 20 12 5 19.5 7.5 12Z";
const STOP: &str = "M6 6H18V18H6Z";

/// The shelf's line glyphs (lucide, ISC; `shelf/icons.rs`), drawn as a 2.2 unit stroke: (name,
/// 24×24 path data). Looked up by name with [`line`].
const LINES: &[(&str, &str)] = &[
    // the + menu's views: drawer shown, the conversation alone, full width (lucide panel-right,
    // square, maximize-2; sill-gui's own)
    (
        "panel-right",
        "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2zM15 3v18",
    ),
    (
        // the shelf: a narrow upright panel, as the shelf stands at the screen's edge
        "panel",
        "M9 3h6a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H9a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z",
    ),
    ("maximize", "M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"),
    // the conversation as one text, to select across messages (lucide text-cursor; sill-gui's own)
    (
        "text-cursor",
        "M17 22h-1a4 4 0 0 1-4-4V6a4 4 0 0 1 4-4h1M7 22h1a4 4 0 0 0 4-4v-1M7 2h1a4 4 0 0 1 4 4v1",
    ),
    (
        "sessions",
        "M3 5h.01M3 12h.01M3 19h.01M8 5h13M8 12h13M8 19h13",
    ),
    ("changes", "M12 3v14M5 10h14M5 21h14"),
    (
        "files",
        "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z",
    ),
    (
        "processes",
        "M2 2h20v8h-20zM2 14h20v8h-20zM6 6L6.01 6M6 18L6.01 18",
    ),
    (
        "list-tree",
        "M21 12h-8M21 6H8M21 18h-8M3 6v4c0 1.1.9 2 2 2h3M3 10v6c0 1.1.9 2 2 2h3",
    ),
    (
        "git-compare",
        "M15 18a3 3 0 1 0 6 0a3 3 0 1 0 -6 0M3 6a3 3 0 1 0 6 0a3 3 0 1 0 -6 0M13 6h3a2 2 0 0 1 2 2v7M11 18H8a2 2 0 0 1-2-2V9",
    ),
    (
        "folder-open",
        "m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2",
    ),
    ("x", "M18 6 6 18M0 0m6 6 12 12"),
    (
        "search",
        "M11 3a8 8 0 1 0 0 16 8 8 0 0 0 0-16M21 21l-4.3-4.3",
    ),
    (
        "refresh",
        "M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8M21 3v5h-5M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16M8 16H3v5",
    ),
    (
        "trash",
        "M10 11v6M14 11v6M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2",
    ),
    (
        "brain",
        "M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18ZM12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18ZM15 13a4.5 4.5 0 0 1-3-4 4.5 4.5 0 0 1-3 4M17.599 6.5a3 3 0 0 0 .399-1.375M6.003 5.125A3 3 0 0 0 6.401 6.5M3.477 10.896a4 4 0 0 1 .585-.396M19.938 10.5a4 4 0 0 1 .585.396M6 18a4 4 0 0 1-1.967-.516M19.967 17.484A4 4 0 0 1 18 18",
    ),
    (
        "ghost",
        "M9 10h.01M15 10h.01M12 2a8 8 0 0 0-8 8v12l3-3 2.5 2.5L12 19l2.5 2.5L17 19l3 3V10a8 8 0 0 0-8-8z",
    ),
    ("chevron-right", "m9 18 6-6-6-6"),
    ("chevron-left", "m15 18-6-6 6-6"),
    ("chevron-down", "m6 9 6 6 6-6"),
    ("check", "M20 6 9 17l-5-5"),
    (
        "copy",
        "M10 8h10a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H10a2 2 0 0 1-2-2V10a2 2 0 0 1 2-2zM4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2",
    ),
    (
        "file",
        "M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2zM14 2v5a1 1 0 0 0 1 1h5",
    ),
    (
        "file-text",
        "M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2zM14 2v5a1 1 0 0 0 1 1h5M10 9H8M16 13H8M16 17H8",
    ),
    (
        "image",
        "M3 3h18v18h-18zM7 9a2 2 0 1 0 4 0a2 2 0 1 0 -4 0M0 0m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21",
    ),
    (
        "file-archive",
        "M13.659 22H18a2 2 0 0 0 2-2V8a2.4 2.4 0 0 0-.706-1.706l-3.588-3.588A2.4 2.4 0 0 0 14 2H6a2 2 0 0 0-2 2v11.5M14 2v5a1 1 0 0 0 1 1h5M8 12v-1M8 18v-2M8 7V6M6 20a2 2 0 1 0 4 0a2 2 0 1 0 -4 0",
    ),
    (
        "file-code",
        "M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2zM14 2v5a1 1 0 0 0 1 1h5M10 12.5 8 15l2 2.5M0 0m14 12.5 2 2.5-2 2.5",
    ),
    (
        "file-cog",
        "M15 8a1 1 0 0 1-1-1V2a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8zM20 8v12a2 2 0 0 1-2 2h-4.182M0 0m3.305 19.53.923-.382M4 10.592V4a2 2 0 0 1 2-2h8M0 0m4.228 16.852-.924-.383M0 0m5.852 15.228-.383-.923M0 0m5.852 20.772-.383.924M0 0m8.148 15.228.383-.923M0 0m8.53 21.696-.382-.924M0 0m9.773 16.852.922-.383M0 0m9.773 19.148.922.383M4 18a3 3 0 1 0 6 0a3 3 0 1 0 -6 0",
    ),
    (
        "file-terminal",
        "M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2zM14 2v5a1 1 0 0 0 1 1h5M0 0m8 16 2-2-2-2M12 18h4",
    ),
    (
        "settings",
        "M10 5H3M12 19H3M14 3v4M16 17v4M21 12h-9M21 19h-5M21 5h-7M8 10v4M8 12H3",
    ),
    ("terminal", "M3 19l7-7-7-7M12 20h9"),
    (
        "agents",
        "M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2M14 2h6a2 2 0 0 1 2 2v6a2 2 0 0 1-2 2h-6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2z",
    ),
    ("code", "m16 18 6-6-6-6M8 6l-6 6 6 6"),
    (
        "link",
        "M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6",
    ),
    ("plus", "M5 12h14M12 5v14"),
    ("minus", "M5 12h14"),
    (
        "lock",
        "M7 11V7a5 5 0 0 1 10 0v4M5 11h14a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a2 2 0 0 1 2-2z",
    ),
    (
        "lock-open",
        "M7 11V7a5 5 0 0 1 9.9-1M5 11h14a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a2 2 0 0 1 2-2z",
    ),
];

/// A line glyph by its shelf name without the `sill-` prefix; an unknown name is the agent ring.
pub fn line(name: &str) -> Handle {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Handle>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().expect("the icon cache");
    if let Some(handle) = cache.get(name) {
        return handle.clone();
    }
    let made = match LINES.iter().find(|(n, _)| *n == name) {
        Some((_, d)) => Handle::from_memory(
            format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="none" stroke="#fff" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" d="{d}"/></svg>"##
            )
            .into_bytes(),
        ),
        None => agent(),
    };
    cache.insert(name.to_string(), made.clone());
    made
}

/// A line glyph `size` square in `color`.
pub fn glyph<'a>(name: &str, size: f32, color: iced::Color) -> iced::widget::Svg<'a> {
    iced::widget::svg(line(name))
        .width(size)
        .height(size)
        .style(crate::style::icon(color))
}

/// The tree's glyph for a path, as the shelf picks it (`shelf/tree.rs`): a folder, open or closed,
/// else the file's kind by its extension.
pub fn for_path(name: &str, directory: bool, expanded: bool) -> &'static str {
    if directory {
        return if expanded { "folder-open" } else { "files" };
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "avif" | "tiff" => {
            "image"
        }
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "deb" | "rpm"
        | "jar" => "file-archive",
        "toml" | "yaml" | "yml" | "json" | "ini" | "cfg" | "conf" | "lock" | "env"
        | "properties" => "file-cog",
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" => "file-terminal",
        "md" | "txt" | "rst" | "org" | "adoc" | "log" | "csv" => "file-text",
        "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "go" | "py" | "js" | "ts" | "tsx" | "jsx"
        | "java" | "kt" | "rb" | "php" | "cs" | "swift" | "lua" | "zig" | "hs" | "ml" | "css"
        | "scss" | "html" | "xml" | "sql" | "proto" | "cmake" | "mk" | "nix" => "file-code",
        _ => "file",
    }
}

fn handle(path: &str) -> Handle {
    Handle::from_memory(
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#fff" fill-rule="evenodd" d="{path}"/></svg>"##
        )
        .into_bytes(),
    )
}

macro_rules! icon {
    ($name:ident, $path:expr) => {
        pub fn $name() -> Handle {
            static H: OnceLock<Handle> = OnceLock::new();
            H.get_or_init(|| handle($path)).clone()
        }
    };
}

icon!(herring, HERRING);
icon!(ghost, GHOST);
icon!(plus, PLUS);
icon!(send, SEND);
icon!(stop, STOP);
icon!(claude, CLAUDE);
icon!(openai, OPENAI);
icon!(gemini, GEMINI);
icon!(agent, AGENT);

/// The provider glyph for a registry agent id (the shelf's choice: Codex is OpenAI's).
pub fn for_agent(id: &str) -> Handle {
    match id {
        "claude" => claude(),
        "codex" => openai(),
        "gemini" => gemini(),
        _ => agent(),
    }
}
