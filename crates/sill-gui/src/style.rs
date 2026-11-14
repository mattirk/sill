//! Sill's look in iced, taken from the GTK shelf: the theme palette (`shelf/theme.rs`) and the
//! conversation's rules (`shelf/conversation.rs` CSS) — flat and square, no bubbles or rounded
//! corners, hierarchy by tone and a thin left rule. Only Dracula so far; the other shelf themes
//! are the same struct with other values.
use iced::widget::{
    button, container, overlay::menu, pick_list, rule, scrollable, svg, text_editor,
};
use iced::{Background, Border, Color, Shadow, Theme};

/// One shelf theme: the values `shelf/theme.rs` derives everything from.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Window / page background.
    pub bg: Color,
    /// Slightly raised surfaces (tabs, the composer).
    pub surface: Color,
    /// Code blocks, and the person's own messages.
    pub code_bg: Color,
    pub fg: Color,
    pub fg_dim: Color,
    pub accent: Color,
    pub warning: Color,
    pub success: Color,
    pub error: Color,
    pub link: Color,
    pub border: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::from_rgb(
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
    )
}

pub const DRACULA: Palette = Palette {
    bg: rgb(0x282a36),
    surface: rgb(0x343746),
    code_bg: rgb(0x21222c),
    fg: rgb(0xf8f8f2),
    fg_dim: rgb(0x6272a4),
    accent: rgb(0xbd93f9),
    warning: rgb(0xffb86c),
    success: rgb(0x50fa7b),
    error: rgb(0xff5555),
    link: rgb(0x8be9fd),
    border: rgb(0x44475a),
};

/// The palette in use (one theme in the spike).
pub const P: Palette = DRACULA;

/// Body text, logical pixels: what the shelf's 13 pt base comes to at its "Smaller" text size on
/// this desktop, measured against `docs/reference-ui/02` — one line of agent text there is 870 px
/// wide at scale 2, and so is the same line here at 15.3 (`snapshots::calibrate`).
pub const TEXT: f32 = 15.3;
/// Tool rows, thoughts (the shelf's 90 %).
pub const SMALL: f32 = TEXT * 0.9;
/// Code, tool output and diffs: the shelf's 85–92 %, and the code face runs wide.
pub const MONO: f32 = TEXT * 0.82;
/// Captions and badges.
pub const TINY: f32 = TEXT * 0.8;

pub fn alpha(color: Color, a: f32) -> Color {
    Color {
        a: color.a * a,
        ..color
    }
}

pub fn theme() -> Theme {
    Theme::custom(
        "Sill Dracula",
        iced::theme::Palette {
            background: P.bg,
            text: P.fg,
            primary: P.accent,
            success: P.success,
            warning: P.warning,
            danger: P.error,
        },
    )
}

fn square(color: Color, width: f32) -> Border {
    Border {
        color,
        width,
        radius: 0.0.into(),
    }
}

/// A plain surface in `color`.
pub fn fill(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(color.into()),
        ..container::Style::default()
    }
}

/// The left rule of a block: the person's message (accent), a tool row (faint), a question
/// (warning), an error (error).
pub fn rule_style(color: Color) -> impl Fn(&Theme) -> rule::Style {
    move |_| rule::Style {
        color,
        radius: 0.0.into(),
        fill_mode: rule::FillMode::Full,
        snap: true,
    }
}

pub fn icon(color: Color) -> impl Fn(&Theme, svg::Status) -> svg::Style {
    move |_, _| svg::Style { color: Some(color) }
}

/// Text-only button: dim, brighter on hover, a faint hover background (`.sill-menu-item`).
pub fn quiet(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: hovered.then(|| alpha(P.fg, 0.07).into()),
        text_color: if hovered { P.fg } else { alpha(P.fg, 0.75) },
        border: square(Color::TRANSPARENT, 0.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A row that is the button (a tool line): no chrome until hovered (`.sill-tool-line`).
pub fn row_button(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: hovered.then(|| alpha(P.fg, 0.04).into()),
        text_color: if hovered { P.fg } else { alpha(P.fg, 0.75) },
        border: square(Color::TRANSPARENT, 0.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A filled square button in `color` with background-coloured text (question options, send).
pub fn solid(color: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let background = match status {
            button::Status::Hovered => Color {
                a: 1.0,
                ..mix(color, P.fg, 0.15)
            },
            button::Status::Pressed => mix(color, P.bg, 0.15),
            button::Status::Disabled => alpha(color, 0.35),
            button::Status::Active => color,
        };
        button::Style {
            background: Some(background.into()),
            text_color: P.bg,
            border: square(Color::TRANSPARENT, 0.0),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color::from_rgba(
        a.r + (b.r - a.r) * t,
        a.g + (b.g - a.g) * t,
        a.b + (b.b - a.b) * t,
        a.a,
    )
}

/// The prompt: no box of its own, it sits on the composer's surface (`.sill-prompt`).
pub fn prompt(_: &Theme, _: text_editor::Status) -> text_editor::Style {
    text_editor::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: square(Color::TRANSPARENT, 0.0),
        placeholder: P.fg_dim,
        value: P.fg,
        selection: alpha(P.accent, 0.35),
    }
}

/// A read-only text view (the transcript for selecting across messages).
pub fn reader(_: &Theme, _: text_editor::Status) -> text_editor::Style {
    text_editor::Style {
        background: Background::Color(P.code_bg),
        border: square(P.border, 1.0),
        placeholder: P.fg_dim,
        value: P.fg,
        selection: alpha(P.accent, 0.35),
    }
}

/// An ACP control in the composer row: text only, a dim handle, no frame.
pub fn control(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let hovered = matches!(
        status,
        pick_list::Status::Hovered | pick_list::Status::Opened { .. }
    );
    pick_list::Style {
        text_color: if hovered { P.fg } else { alpha(P.fg, 0.7) },
        placeholder_color: P.fg_dim,
        handle_color: alpha(P.fg, 0.45),
        background: Background::Color(if hovered {
            alpha(P.fg, 0.06)
        } else {
            Color::TRANSPARENT
        }),
        border: square(Color::TRANSPARENT, 0.0),
    }
}

pub fn control_menu(_: &Theme) -> menu::Style {
    menu::Style {
        background: P.surface.into(),
        border: square(P.border, 1.0),
        text_color: P.fg,
        selected_text_color: P.fg,
        selected_background: alpha(P.accent, 0.25).into(),
        shadow: Shadow::default(),
    }
}

/// The conversation's scrollbar: hidden at rest, a thin bar while hovered or dragged (the
/// reference's note on `02`: the shelf's overlay scrollbar autohides).
pub fn feed_scroll(_: &Theme, status: scrollable::Status) -> scrollable::Style {
    let shown = !matches!(status, scrollable::Status::Active { .. });
    let bar = if shown {
        alpha(P.fg, 0.35)
    } else {
        Color::TRANSPARENT
    };
    let rail = scrollable::Rail {
        background: None,
        border: square(Color::TRANSPARENT, 0.0),
        scroller: scrollable::Scroller {
            background: bar.into(),
            border: square(Color::TRANSPARENT, 0.0),
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: P.surface.into(),
            border: square(P.border, 1.0),
            shadow: Shadow::default(),
            icon: P.fg,
        },
    }
}

/// Selectable text in `color` (iced_selection's text has a style, not a colour of its own).
pub fn selectable(color: Color) -> impl Fn(&Theme) -> iced_selection::text::Style {
    move |_| iced_selection::text::Style {
        color: Some(color),
        selection: alpha(P.accent, 0.35),
    }
}
