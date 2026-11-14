//! Icons for the menus: provider glyphs from simple-icons (CC0 1.0: Claude, OpenAI, Google Gemini)
//! plus a neutral placeholder, and line glyphs from lucide (ISC) for the pages. All are rendered on
//! demand in the colour of the widget they sit in,
//! so they read light on dark themes and dark on light ones. (GTK's own symbolic recolouring only
//! covers icons from an icon theme; a texture made here follows the live foreground instead.)

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

/// (icon name, 24×24 path data)
const GLYPHS: &[(&str, &str)] = &[
    ("sill-claude-symbolic", "m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z"),
    ("sill-openai-symbolic", "M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z"),
    ("sill-gemini-symbolic", "M11.04 19.32Q12 21.51 12 24q0-2.49.93-4.68.96-2.19 2.58-3.81t3.81-2.55Q21.51 12 24 12q-2.49 0-4.68-.93a12.3 12.3 0 0 1-3.81-2.58 12.3 12.3 0 0 1-2.58-3.81Q12 2.49 12 0q0 2.49-.96 4.68-.93 2.19-2.55 3.81a12.3 12.3 0 0 1-3.81 2.58Q2.49 12 0 12q2.49 0 4.68.96 2.19.93 3.81 2.55t2.55 3.81"),
    // placeholder: a ring with a dot — "some agent"
    ("sill-agent-symbolic", "M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 2a8 8 0 1 1 0 16 8 8 0 0 1 0-16zm0 5a3 3 0 1 0 0 6 3 3 0 0 0 0-6z"),
];

/// Line glyphs for the page menu (lucide, ISC): (name, path data drawn as a 2px stroke)
const STROKES: &[(&str, &str)] = &[
    ("sill-sessions", "M3 5h.01M3 12h.01M3 19h.01M8 5h13M8 12h13M8 19h13"),
    ("sill-changes", "M12 3v14M5 10h14M5 21h14"),
    ("sill-files", "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"),
    ("sill-settings", "M10 5H3M12 19H3M14 3v4M16 17v4M21 12h-9M21 19h-5M21 5h-7M8 10v4M8 12H3"),
    // a prompt: chevron and underscore, no window chrome
    ("sill-terminal", "M3 19l7-7-7-7M12 20h9"),
    ("sill-agents", "M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2M14 2h6a2 2 0 0 1 2 2v6a2 2 0 0 1-2 2h-6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2z"),
    ("sill-code", "m16 18 6-6-6-6M8 6l-6 6 6 6"),
    ("sill-link", "M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"),
    // controls drawn by sill rather than taken from the icon theme: themes differ in how much of
    // the canvas a glyph fills (Papirus's 22/24 px symbolic set is a 16 px glyph with margins, its
    // plus even sits in a corner), so a theme icon changes size with the lookup size the display
    // scale lands on
    ("sill-plus", "M5 12h14M12 5v14"),
    ("sill-minus", "M5 12h14"),
    ("sill-lock", "M7 11V7a5 5 0 0 1 10 0v4M5 11h14a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a2 2 0 0 1 2-2z"),
    ("sill-lock-open", "M7 11V7a5 5 0 0 1 9.9-1M5 11h14a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a2 2 0 0 1 2-2z"),
];

/// Filled glyphs for the send button (lucide send-horizontal, a plain square for stop).
const FILLED: &[(&str, &str)] = &[
    ("sill-send", "M3.714 3.048a.498.498 0 0 0-.683.627l2.843 7.627a2 2 0 0 1 0 1.396l-2.842 7.627a.498.498 0 0 0 .682.627l18-8.5a.5.5 0 0 0 0-.904z"),
    ("sill-stop", "M6 6h12v12H6z"),
];

pub const PLACEHOLDER: &str = "sill-agent-symbolic";

fn hex(color: gdk::RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (color.red() * 255.0).round() as u8,
        (color.green() * 255.0).round() as u8,
        (color.blue() * 255.0).round() as u8
    )
}

/// `px` as callers give it is for the normal text size; textures don't follow CSS font sizes, so the
/// text scale is applied here (CSS-sized icons get it from their `em` units).
fn scaled(px: i32) -> i32 {
    (px as f64 * super::theme::text_scale()).round().max(1.0) as i32
}

fn texture(svg: String) -> gtk::Image {
    match gdk::Texture::from_bytes(&glib::Bytes::from_owned(svg.into_bytes())) {
        Ok(t) => gtk::Image::from_paintable(Some(&t)),
        Err(_) => gtk::Image::from_icon_name("image-missing-symbolic"),
    }
}

fn texture_image(svg: String, px: i32) -> gtk::Image {
    let i = texture(svg);
    i.set_pixel_size(scaled(px));
    i
}

fn stroke_svg(d: &str, color: gdk::RGBA, size: i32) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 24 24"><path fill="none" stroke="{stroke}" stroke-opacity="{a}" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" d="{d}"/></svg>"##,
        stroke = hex(color),
        a = color.alpha()
    )
}

fn fill_svg(d: &str, color: gdk::RGBA, size: i32) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 24 24"><path fill="{fill}" fill-opacity="{a}" d="{d}"/></svg>"##,
        fill = hex(color),
        a = color.alpha()
    )
}

/// A sill glyph (stroke or filled) with no size of its own: the widget's CSS `-gtk-icon-size` sizes
/// it, so a control sized in `em` keeps its glyph in proportion on every display and text scale.
/// Rendered large enough to stay crisp at 2× for any sensible em.
pub fn scalable(name: &str, color: gdk::RGBA) -> gtk::Image {
    const SIZE: i32 = 96;
    if let Some((_, d)) = STROKES.iter().find(|(n, _)| *n == name) {
        return texture(stroke_svg(d, color, SIZE));
    }
    if let Some((_, d)) = FILLED.iter().find(|(n, _)| *n == name) {
        return texture(fill_svg(d, color, SIZE));
    }
    gtk::Image::from_icon_name(name)
}

/// A sill line glyph (`sill-sessions`, `sill-files`, …) in colour `color`.
pub fn named(name: &str, color: gdk::RGBA, px: i32) -> gtk::Image {
    let Some((_, d)) = STROKES.iter().find(|(n, _)| *n == name) else {
        return gtk::Image::from_icon_name(name);
    };
    texture_image(stroke_svg(d, color, px * 4), px)
}

fn glyph(name: &str) -> Option<&'static str> {
    GLYPHS.iter().find(|(n, _)| *n == name).map(|(_, d)| *d)
}

/// An image for an agent's icon in colour `color` (the containing widget's foreground): a sill glyph
/// rendered as a texture, an SVG file (registry icons) recoloured, any other name from the icon
/// theme, the placeholder when unknown.
pub fn image(agent: Option<&sill_core::agents::Agent>, color: gdk::RGBA, px: i32) -> gtk::Image {
    let name = agent.and_then(|a| a.icon.clone()).unwrap_or_else(|| PLACEHOLDER.to_string());
    if name.starts_with('/') {
        return from_file(std::path::Path::new(&name), color, px);
    }
    if glyph(&name).is_none() {
        let known = gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d).has_icon(&name)).unwrap_or(false);
        if known {
            let i = gtk::Image::from_icon_name(&name);
            i.set_pixel_size(scaled(px));
            return i;
        }
    }
    let d = glyph(&name).or_else(|| glyph(PLACEHOLDER)).unwrap_or("");
    // rendered at 4× for crisp edges on any scale factor, shown at `px`
    let size = px * 4;
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 24 24"><path fill="{fill}" fill-opacity="{a}" d="{d}"/></svg>"##,
        fill = hex(color),
        a = color.alpha()
    );
    texture_image(svg, px)
}

/// The colour to draw a glyph in for `widget` right now: the palette's foreground when a theme is
/// active (the widget's own CSS colour may lag behind a theme switch), else the widget's colour.
pub fn fg_for(widget: &impl IsA<gtk::Widget>, theme: &super::theme::Theme) -> gdk::RGBA {
    if !theme.fg.is_empty()
        && let Ok(c) = gdk::RGBA::parse(theme.fg)
    {
        return c;
    }
    widget.color()
}

/// An SVG file drawn in one colour: every `fill`/`stroke` that isn't `none` and every
/// `currentColor` becomes `color`, so a third-party icon reads like the built-in glyphs on any
/// theme. Falls back to the placeholder when the file can't be read.
pub fn from_file(path: &std::path::Path, color: gdk::RGBA, px: i32) -> gtk::Image {
    let Ok(text) = std::fs::read_to_string(path) else {
        return named(PLACEHOLDER, color, px);
    };
    let fg = hex(color);
    let mut svg = recolour_attr(&recolour_attr(&text, "fill", &fg), "stroke", &fg).replace("currentColor", &fg);
    // an icon with no colour anywhere renders black: give the root a fill
    if !svg.contains("fill=") && !svg.contains("fill:") {
        svg = svg.replacen("<svg", &format!("<svg fill=\"{fg}\""), 1);
    }
    // our size, whatever the file says
    let size = px * 4;
    if let Some(start) = svg.find("<svg") {
        let end = svg[start..].find('>').map(|i| start + i).unwrap_or(svg.len());
        let mut head = svg[start..end].to_string();
        head = strip_attr(&head, "width");
        head = strip_attr(&head, "height");
        head.push_str(&format!(" width=\"{size}\" height=\"{size}\""));
        svg = format!("{}{}{}", &svg[..start], head, &svg[end..]);
    }
    texture_image(svg, px)
}

/// Replace the value of every `attr="…"` (and `attr:…` in style attributes) that isn't `none`.
fn recolour_attr(svg: &str, attr: &str, fg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    let needle = format!("{attr}=\"");
    while let Some(i) = rest.find(&needle) {
        // only a whole attribute name (not `stroke-width=`): the char before must not be alnum or '-'
        let before = rest[..i].chars().last();
        let whole = !matches!(before, Some(c) if c.is_alphanumeric() || c == '-');
        let vstart = i + needle.len();
        let Some(vlen) = rest[vstart..].find('"') else { break };
        let value = &rest[vstart..vstart + vlen];
        out.push_str(&rest[..vstart]);
        if whole && value != "none" && !value.starts_with("url(") {
            out.push_str(fg);
        } else {
            out.push_str(value);
        }
        rest = &rest[vstart + vlen..];
    }
    out.push_str(rest);
    // style="fill:#000" forms
    let needle = format!("{attr}:");
    let mut out2 = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find(&needle) {
        let before = rest[..i].chars().last();
        let whole = !matches!(before, Some(c) if c.is_alphanumeric() || c == '-');
        let vstart = i + needle.len();
        let vlen = rest[vstart..].find([';', '"', '}']).unwrap_or(rest.len() - vstart);
        let value = rest[vstart..vstart + vlen].trim();
        out2.push_str(&rest[..vstart]);
        if whole && value != "none" && !value.starts_with("url(") {
            out2.push_str(fg);
        } else {
            out2.push_str(&rest[vstart..vstart + vlen]);
        }
        rest = &rest[vstart + vlen..];
    }
    out2.push_str(rest);
    out2
}

fn strip_attr(head: &str, attr: &str) -> String {
    let needle = format!(" {attr}=\"");
    let mut h = head.to_string();
    while let Some(i) = h.find(&needle) {
        let vstart = i + needle.len();
        let Some(vlen) = h[vstart..].find('"') else { break };
        h.replace_range(i..vstart + vlen + 1, "");
    }
    h
}
