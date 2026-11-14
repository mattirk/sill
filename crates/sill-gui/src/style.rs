//! Sill's look in iced: the theme palette and the conversation's rules — flat and square, no
//! bubbles or rounded corners, hierarchy by tone and a thin left rule. Every theme is here, the
//! same struct with other values.
use iced::widget::{
    button, container, overlay::menu, pick_list, rule, scrollable, svg, text_editor,
};
use iced::{Background, Border, Color, Shadow, Theme};

/// One theme: the values everything else is derived from.
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

/// The themes, by id; the daemon's `theme` setting picks one. Miasma is the one sill is developed
/// in (the person, 2026-11-05). Sill Dark and Light (2026-11-09) are Miasma's backgrounds with
/// Iceberg's highlight colours (accent, link, warning, success, error); Miasma has no light
/// variant, so Sill Light's backgrounds are light tones in Miasma's warm olive-grey.
pub const THEMES: &[(&str, Palette)] = &[
    (
        "adwaita",
        Palette {
            bg: rgb(0x242424),
            surface: rgb(0x303030),
            code_bg: rgb(0x1e1e1e),
            fg: rgb(0xffffff),
            fg_dim: rgb(0x9d9d9d),
            accent: rgb(0x78aeed),
            warning: rgb(0xffa348),
            success: rgb(0x8ff0a4),
            error: rgb(0xff7b63),
            link: rgb(0x78aeed),
            border: rgb(0x3d3d3d),
        },
    ),
    (
        "adwaita-light",
        Palette {
            bg: rgb(0xfafafa),
            surface: rgb(0xebebeb),
            code_bg: rgb(0xf0f0f0),
            fg: rgb(0x323232),
            fg_dim: rgb(0x8b8b8b),
            accent: rgb(0x1c71d8),
            warning: rgb(0x9c6e03),
            success: rgb(0x1b8553),
            error: rgb(0xc01c28),
            link: rgb(0x1c71d8),
            border: rgb(0xd8d8d8),
        },
    ),
    (
        "dracula",
        Palette {
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
        },
    ),
    (
        "solarized-dark",
        Palette {
            bg: rgb(0x002b36),
            surface: rgb(0x073642),
            code_bg: rgb(0x00212b),
            fg: rgb(0x93a1a1),
            fg_dim: rgb(0x586e75),
            accent: rgb(0x268bd2),
            warning: rgb(0xb58900),
            success: rgb(0x859900),
            error: rgb(0xdc322f),
            link: rgb(0x2aa198),
            border: rgb(0x0a3f4d),
        },
    ),
    (
        "solarized-light",
        Palette {
            bg: rgb(0xfdf6e3),
            surface: rgb(0xeee8d5),
            code_bg: rgb(0xf3ecd8),
            fg: rgb(0x586e75),
            fg_dim: rgb(0x93a1a1),
            accent: rgb(0x268bd2),
            warning: rgb(0xb58900),
            success: rgb(0x859900),
            error: rgb(0xdc322f),
            link: rgb(0x2aa198),
            border: rgb(0xe0d9c4),
        },
    ),
    (
        "nord",
        Palette {
            bg: rgb(0x2e3440),
            surface: rgb(0x3b4252),
            code_bg: rgb(0x272c36),
            fg: rgb(0xd8dee9),
            fg_dim: rgb(0x7b88a1),
            accent: rgb(0x88c0d0),
            warning: rgb(0xebcb8b),
            success: rgb(0xa3be8c),
            error: rgb(0xbf616a),
            link: rgb(0x81a1c1),
            border: rgb(0x434c5e),
        },
    ),
    (
        "miasma",
        Palette {
            bg: rgb(0x222222),
            surface: rgb(0x2b2b28),
            code_bg: rgb(0x1c1c1c),
            fg: rgb(0xc2c2b0),
            fg_dim: rgb(0x787866),
            accent: rgb(0xc9a554),
            warning: rgb(0xb36d43),
            success: rgb(0x5f875f),
            error: rgb(0xbb7744),
            link: rgb(0xd7c483),
            border: rgb(0x363632),
        },
    ),
    (
        "gruvbox-dark",
        Palette {
            bg: rgb(0x282828),
            surface: rgb(0x3c3836),
            code_bg: rgb(0x1d2021),
            fg: rgb(0xebdbb2),
            fg_dim: rgb(0xa89984),
            accent: rgb(0x83a598),
            warning: rgb(0xfabd2f),
            success: rgb(0xb8bb26),
            error: rgb(0xfb4934),
            link: rgb(0x8ec07c),
            border: rgb(0x504945),
        },
    ),
    (
        "gruvbox-light",
        Palette {
            bg: rgb(0xfbf1c7),
            surface: rgb(0xebdbb2),
            code_bg: rgb(0xf2e5bc),
            fg: rgb(0x3c3836),
            fg_dim: rgb(0x7c6f64),
            accent: rgb(0x076678),
            warning: rgb(0xb57614),
            success: rgb(0x79740e),
            error: rgb(0x9d0006),
            link: rgb(0x076678),
            border: rgb(0xd5c4a1),
        },
    ),
    (
        "iceberg-dark",
        Palette {
            bg: rgb(0x161821),
            surface: rgb(0x1e2132),
            code_bg: rgb(0x0f1117),
            fg: rgb(0xc6c8d1),
            fg_dim: rgb(0x6b7089),
            accent: rgb(0x84a0c6),
            warning: rgb(0xe2a478),
            success: rgb(0xb4be82),
            error: rgb(0xe27878),
            link: rgb(0x89b8c2),
            border: rgb(0x272c42),
        },
    ),
    (
        "iceberg-light",
        Palette {
            bg: rgb(0xe8e9ec),
            surface: rgb(0xdcdfe7),
            code_bg: rgb(0xe1e3ea),
            fg: rgb(0x33374c),
            fg_dim: rgb(0x8389a3),
            accent: rgb(0x2d539e),
            warning: rgb(0xc57339),
            success: rgb(0x668e3d),
            error: rgb(0xcc517a),
            link: rgb(0x3f83a6),
            border: rgb(0xcad0de),
        },
    ),
    (
        "sill-dark",
        Palette {
            bg: rgb(0x222222),
            surface: rgb(0x2b2b28),
            code_bg: rgb(0x1c1c1c),
            fg: rgb(0xc2c2b0),
            fg_dim: rgb(0x787866),
            accent: rgb(0x84a0c6),
            warning: rgb(0xe2a478),
            success: rgb(0xb4be82),
            error: rgb(0xe27878),
            link: rgb(0x89b8c2),
            border: rgb(0x363632),
        },
    ),
    (
        "sill-light",
        Palette {
            bg: rgb(0xe8e8df),
            surface: rgb(0xdcdcd0),
            code_bg: rgb(0xe0e0d5),
            fg: rgb(0x2b2b28),
            fg_dim: rgb(0x787866),
            accent: rgb(0x2d539e),
            warning: rgb(0xc57339),
            success: rgb(0x668e3d),
            error: rgb(0xcc517a),
            link: rgb(0x3f83a6),
            border: rgb(0xcfcfc1),
        },
    ),
];

/// The theme in use, an index into [`THEMES`]; Miasma until the daemon's setting says otherwise.
static CURRENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(6);

/// The palette in use, read where it is used (`P.fg`), so a theme change shows on the next frame.
pub struct Current;

impl std::ops::Deref for Current {
    type Target = Palette;

    fn deref(&self) -> &Palette {
        &THEMES[CURRENT.load(std::sync::atomic::Ordering::Relaxed)].1
    }
}

pub static P: Current = Current;

/// Use the theme `id`; false (and no change) for an id sill does not know.
pub fn set_theme(id: &str) -> bool {
    match THEMES.iter().position(|(t, _)| *t == id) {
        Some(index) => {
            CURRENT.store(index, std::sync::atomic::Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// The id of the theme in use.
pub fn theme_id() -> &'static str {
    THEMES[CURRENT.load(std::sync::atomic::Ordering::Relaxed)].0
}

/// Body text, logical pixels: what a 13 pt base comes to at the "Smaller" text size on
/// this desktop, measured against `docs/reference-ui/02` — one line of agent text there is 870 px
/// wide at scale 2, and so is the same line here at 15.3 (`snapshots::calibrate`).
pub const TEXT: f32 = 15.3;
/// Tool rows, thoughts (90 % of the body).
pub const SMALL: f32 = TEXT * 0.9;
/// Code, tool output and diffs: a little under the reference's 85–92 %, as the code face runs wide.
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
        format!("Sill {}", theme_id()),
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

/// Text-only button: dim, brighter on hover, a faint hover background (`.sill-menu-item`);
/// dimmer still when it cannot be pressed.
pub fn quiet(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: hovered.then(|| alpha(P.fg, 0.07).into()),
        text_color: match status {
            _ if hovered => P.fg,
            button::Status::Disabled => alpha(P.fg, 0.3),
            _ => alpha(P.fg, 0.75),
        },
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

/// A tree row that is open in the reader (`.sill-files > row:selected`).
pub fn row_selected(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(alpha(P.accent, if hovered { 0.3 } else { 0.25 }).into()),
        text_color: P.fg,
        border: square(Color::TRANSPARENT, 0.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Send and Stop (`button.sill-send`): a square in a quarter of `color`, more on hover.
pub fn tinted(color: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: Some(
            alpha(
                color,
                match status {
                    button::Status::Hovered | button::Status::Pressed => 0.45,
                    button::Status::Disabled => 0.15,
                    button::Status::Active => 0.25,
                },
            )
            .into(),
        ),
        text_color: P.fg,
        border: square(Color::TRANSPARENT, 0.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The send button's arrow: light on the tint, dark on the accent under the pointer.
pub fn send_arrow(_: &Theme, status: svg::Status) -> svg::Style {
    svg::Style {
        color: Some(match status {
            svg::Status::Hovered => P.bg,
            svg::Status::Idle => alpha(P.fg, 0.9),
        }),
    }
}

/// The send button: a tint of the accent at rest, the accent itself under the pointer.
pub fn send(_: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(
            match status {
                button::Status::Hovered => P.accent,
                button::Status::Pressed => mix(P.accent, P.bg, 0.2),
                button::Status::Disabled => alpha(P.accent, 0.15),
                button::Status::Active => alpha(P.accent, 0.25),
            }
            .into(),
        ),
        text_color: P.fg,
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

/// A read-only text view (a file's source).
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

/// An agent control's menu: a raised surface with a soft shadow, the choice under the pointer
/// in a tint of the accent.
pub fn control_menu(_: &Theme) -> menu::Style {
    menu::Style {
        background: P.surface.into(),
        border: square(P.border, 1.0),
        text_color: alpha(P.fg, 0.85),
        selected_text_color: P.fg,
        selected_background: alpha(P.accent, 0.22).into(),
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.45),
            offset: iced::Vector::new(0.0, 2.0),
            blur_radius: 12.0,
        },
    }
}

/// Every scrollbar's size: 3 px, 30 % of iced's default 10 (the person, 2026-11-05).
pub fn bar() -> scrollable::Scrollbar {
    scrollable::Scrollbar::new().width(3).scroller_width(3)
}

/// The conversation's scrollbar: hidden at rest, a thin bar while hovered or dragged (the
/// reference's note on `02`: the overlay scrollbar autohides).
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
        selection: selection(),
    }
}

/// Selectable text in the colour it inherits: Markdown, whose spans colour themselves. One
/// selection crosses messages, so every text is selected in the same tint.
pub fn selectable_inherited(_: &Theme) -> iced_selection::text::Style {
    iced_selection::text::Style {
        color: None,
        selection: selection(),
    }
}

/// What selected text lies on.
fn selection() -> Color {
    alpha(P.accent, 0.35)
}
