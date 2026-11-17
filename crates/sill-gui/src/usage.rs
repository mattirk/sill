//! Settings → Agent running time: the time agents worked on each of the last days, as bars
//! (docs/sill-gui.md, *Agent running time*). The daemon gives the time per UTC hour
//! (`_sill/usage.active_time`); the days here are the local ones of this machine. Sessions that
//! worked at once are added together, so a day can hold more than 24 hours.
use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::widget::{column, container, text};
use iced::{Element, Fill, Point, Rectangle, Renderer, Size, Theme, mouse};
use sill_proto::admin::ActiveHour;

use crate::Message;
use crate::style::{self, P, alpha};

/// The days the chart shows, today last.
pub const DAYS: u16 = 30;
const HEIGHT: f32 = 170.0;

/// One local day: its number since 1970-01-01 and the time worked in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day {
    pub day: i64,
    pub ms: u64,
}

/// Seconds east of UTC at `seconds`, by the machine's time zone (0 when it cannot be told).
fn local_offset(seconds: i64) -> i64 {
    let time: libc::time_t = seconds as libc::time_t;
    // SAFETY: localtime_r writes only the tm it is given and reads the time it is given.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() { 0 } else { tm.tm_gmtoff as i64 }
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

/// The last `count` local days up to the one holding `now`, each with the time of its hours.
/// An hour belongs to the day it starts in; `offset` is the zone's seconds east of UTC at a time.
pub fn days(hours: &[ActiveHour], now: i64, count: u16, offset: impl Fn(i64) -> i64) -> Vec<Day> {
    let day_of = |seconds: i64| (seconds + offset(seconds)).div_euclid(86_400);
    let today = day_of(now);
    let first = today - i64::from(count) + 1;
    let mut days: Vec<Day> = (first..=today).map(|day| Day { day, ms: 0 }).collect();
    for hour in hours {
        let day = day_of(hour.hour as i64 * 3600);
        if let Some(slot) = usize::try_from(day - first).ok().and_then(|i| days.get_mut(i)) {
            slot.ms += hour.ms;
        }
    }
    days
}

/// The demo's month: working days of a few hours, quiet weekends, today still short.
pub fn demo() -> Vec<ActiveHour> {
    let now = now_seconds() as u64 / 3600;
    (0..u64::from(DAYS))
        .flat_map(|back| {
            let day = now / 24 - back;
            let weekend = matches!(weekday(day as i64), "Sat" | "Sun");
            let hours = if weekend { day % 2 } else { 2 + (day * 7) % 6 };
            (0..hours).map(move |hour| ActiveHour { hour: day * 24 + 8 + hour, ms: 1_500_000 + (day * 31 + hour * 17) % 40 * 60_000 })
        })
        .filter(|hour| hour.hour <= now)
        .collect()
}

/// A time worked, as hours and minutes: "3 h 25 min", "25 min", "2 h".
pub fn span(ms: u64) -> String {
    let minutes = (ms + 30_000) / 60_000;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

fn weekday(day: i64) -> &'static str {
    WEEKDAYS[day.rem_euclid(7) as usize]
}

/// The axis: its top in hours and the step between its lines, so that there are two to four
/// lines at round hours (or round minutes under an hour).
fn axis(max_ms: u64) -> (f32, f32) {
    let hours = max_ms as f32 / 3_600_000.0;
    let step = [0.25, 0.5, 1.0, 2.0, 4.0, 6.0, 12.0, 24.0, 48.0, 96.0].into_iter().find(|step| hours / step <= 4.0).unwrap_or(192.0);
    let top = ((hours / step).ceil()).max(1.0) * step;
    (top, step)
}

fn tick(hours: f32) -> String {
    if hours < 1.0 {
        format!("{} min", (hours * 60.0).round())
    } else if hours.fract() == 0.0 {
        format!("{hours} h")
    } else {
        format!("{hours:.1} h")
    }
}

/// The section's body: a line of totals over the bars.
pub fn view<'a>(hours: Option<&[ActiveHour]>) -> Element<'a, Message> {
    let Some(hours) = hours else {
        return container(text("The daemon does not report running time; restart it to update it.").size(style::SMALL).color(P.fg_dim))
            .padding([12, 16])
            .into();
    };
    let days = days(hours, now_seconds(), DAYS, local_offset);
    let total: u64 = days.iter().map(|d| d.ms).sum();
    let today = days.last().map_or(0, |d| d.ms);
    let totals = format!("Today {} · last {DAYS} days {}. Agents working at the same time are added together.", span(today), span(total));
    column![text(totals).size(style::SMALL).color(P.fg_dim), iced::widget::canvas(Chart { days }).width(Fill).height(HEIGHT),]
        .spacing(10)
        .padding([12, 16])
        .into()
}

struct Chart {
    days: Vec<Day>,
}

/// Where the bars are drawn inside the canvas.
struct Plot {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
}

impl Chart {
    fn plot(&self, size: Size) -> Plot {
        let (left, top, bottom) = (44.0, 22.0, 20.0);
        Plot { left, top, width: (size.width - left).max(1.0), height: (size.height - top - bottom).max(1.0) }
    }

    /// The day whose column holds the pointer.
    fn hovered(&self, plot: &Plot, cursor: Option<Point>) -> Option<usize> {
        let at = cursor?;
        if at.x < plot.left || self.days.is_empty() {
            return None;
        }
        let slot = plot.width / self.days.len() as f32;
        let index = ((at.x - plot.left) / slot) as usize;
        (index < self.days.len()).then_some(index)
    }
}

impl canvas::Program<Message> for Chart {
    type State = ();

    fn update(&self, _state: &mut (), event: &iced::Event, _bounds: Rectangle, _cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        // the hovered bar and its label follow the pointer
        matches!(event, iced::Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft)).then(canvas::Action::request_redraw)
    }

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, cursor: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let plot = self.plot(bounds.size());
        let max = self.days.iter().map(|d| d.ms).max().unwrap_or(0);
        let (top_hours, step) = axis(max);
        let label = |content: String, position: Point, align_x, color| canvas::Text {
            content,
            position,
            color,
            size: style::SMALL.into(),
            align_x,
            align_y: iced::alignment::Vertical::Center,
            ..canvas::Text::default()
        };
        // the hour lines, quiet, with their hours at the left
        let mut at = 0.0;
        while at <= top_hours + 0.001 {
            let y = plot.top + plot.height * (1.0 - at / top_hours);
            frame.fill_rectangle(Point::new(plot.left, y.round()), Size::new(plot.width, 1.0), alpha(P.fg, if at == 0.0 { 0.22 } else { 0.08 }));
            if at > 0.0 {
                frame.fill_text(label(tick(at), Point::new(plot.left - 8.0, y), iced::widget::text::Alignment::Right, P.fg_dim));
            }
            at += step;
        }
        let hovered = self.hovered(&plot, cursor.position_in(bounds));
        let slot = plot.width / self.days.len().max(1) as f32;
        let bar = (slot - 2.0).clamp(1.0, 18.0);
        let baseline = plot.top + plot.height;
        for (index, day) in self.days.iter().enumerate() {
            let x = plot.left + slot * index as f32 + (slot - bar) / 2.0;
            if day.ms > 0 {
                // a day with any work shows, however little
                let height = (plot.height * day.ms as f32 / (top_hours * 3_600_000.0)).max(2.0);
                let radius = (bar / 2.0).min(4.0).min(height);
                let shape = Path::rounded_rectangle(
                    Point::new(x, baseline - height),
                    Size::new(bar, height),
                    iced::border::Radius { top_left: radius, top_right: radius, bottom_right: 0.0, bottom_left: 0.0 },
                );
                frame.fill(&shape, alpha(P.accent, if hovered == Some(index) { 1.0 } else { 0.7 }));
            }
            // Mondays are dated under the axis
            if weekday(day.day) == "Mon" && hovered.is_none() {
                frame.fill_text(label(
                    crate::menu::date(day.day * 86_400)[5..].to_owned(),
                    Point::new(x + bar / 2.0, baseline + 11.0),
                    iced::widget::text::Alignment::Center,
                    P.fg_dim,
                ));
            }
        }
        // the hovered day: its date under the axis and its time above the plot
        if let Some(index) = hovered {
            let day = self.days[index];
            let center = plot.left + slot * (index as f32 + 0.5);
            let text = format!("{} {} · {}", weekday(day.day), crate::menu::date(day.day * 86_400), span(day.ms));
            // kept inside the plot at either end
            let half = text.chars().count() as f32 * style::SMALL * 0.3;
            let x = center.clamp(plot.left + half, (plot.left + plot.width - half).max(plot.left + half));
            frame.fill_text(label(text, Point::new(x, 9.0), iced::widget::text::Alignment::Center, P.fg));
            frame.fill_rectangle(Point::new(center.round(), baseline), Size::new(1.0, 4.0), alpha(P.fg, 0.5));
        }
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hours_are_grouped_into_local_days_with_empty_days_kept() {
        let hour = |hour, ms| ActiveHour { hour, ms };
        // 1970-01-03 12:00 UTC, two hours east: local day 2
        let now = 2 * 86_400 + 12 * 3600;
        let hours = [
            hour(21, 1000),  // day 0 21:00 UTC = 23:00 local: day 0
            hour(22, 2000),  // day 0 22:00 UTC = day 1 00:00 local
            hour(45, 4000),  // day 1 21:00 UTC: day 1
            hour(46, 8000),  // day 2 local
            hour(1, 16_000), // before the first day shown
        ];
        assert_eq!(days(&hours[..4], now, 3, |_| 7200), [Day { day: 0, ms: 1000 }, Day { day: 1, ms: 6000 }, Day { day: 2, ms: 8000 }]);
        assert_eq!(days(&hours, now, 2, |_| 7200), [Day { day: 1, ms: 6000 }, Day { day: 2, ms: 8000 }]);
    }

    /// For looking at: `target/sill-gui-snapshots/running-time-*.png`.
    #[test]
    #[ignore = "writes a PNG for review; run explicitly"]
    fn the_chart_renders_with_the_demo_month() {
        let hours = demo();
        let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (680.0, 240.0), container(view(Some(&hours))).style(style::fill(P.surface)));
        ui.point_at(Point::new(400.0, 120.0));
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/sill-gui-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let snapshot = ui.snapshot(&style::theme()).unwrap();
        let _ = snapshot.matches_image(dir.join("running-time"));
    }

    #[test]
    fn spans_and_axes_read_in_hours_and_minutes() {
        assert_eq!(span(0), "0 min");
        assert_eq!(span(25 * 60_000), "25 min");
        assert_eq!(span(2 * 3_600_000 + 20_000), "2 h");
        assert_eq!(span(3 * 3_600_000 + 25 * 60_000), "3 h 25 min");
        assert_eq!(axis(0), (0.25, 0.25));
        assert_eq!(axis(40 * 60_000), (0.75, 0.25));
        assert_eq!(axis(5 * 3_600_000 + 1), (6.0, 2.0));
        assert_eq!(axis(30 * 3_600_000), (36.0, 12.0));
        assert_eq!((tick(0.25), tick(2.0), tick(1.5)), ("15 min".into(), "2 h".into(), "1.5 h".into()));
        assert_eq!((weekday(0), weekday(4), weekday(-1)), ("Thu", "Mon", "Wed"));
    }
}
