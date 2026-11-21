//! Where the window goes and what it keeps (docs/silli-gui.md, *Window placement and resizing*):
//! one controller for opening, showing, mode changes, display changes and the person's own
//! resizing and moving (the person, 2026-11-07).
//!
//! - **Shelf** is locked to the main display's left edge, the full height of its usable area
//!   (panels excluded). Only its width is the person's: the right edge drags it, within 15–40 % of
//!   the display, and the width is kept for that display.
//! - **Drawer** and **Full** are free: their size and place are kept per display, as shares of
//!   its usable area, and they come back on the display they were left on — on the main display
//!   while that one is missing, its preferences kept for when it returns.
//!
//! Pure: the platform (`desktop.rs`, X11) reads the displays and the window and places it; this
//! decides. Device pixels throughout, so Interface scale never enters.
use std::time::{Duration, Instant};

use crate::layout::{Mode, SHELF_MAX, SHELF_MIN, WORK_SHARE};
use crate::settings::{DisplayPrefs, Settings};

/// A rectangle on the screen, device pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn center(&self) -> (i32, i32) {
        (self.x + (self.width / 2) as i32, self.y + (self.height / 2) as i32)
    }

    fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    fn contains(&self, (x, y): (i32, i32)) -> bool {
        (self.x..self.right()).contains(&x) && (self.y..self.bottom()).contains(&y)
    }

    /// How far `point` is from the rectangle, squared (0 inside).
    fn distance(&self, (x, y): (i32, i32)) -> i64 {
        let dx = (self.x - x).max(x - self.right() + 1).max(0) as i64;
        let dy = (self.y - y).max(y - self.bottom() + 1).max(0) as i64;
        dx * dx + dy * dy
    }

    /// Where the two overlap, if they do.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let (x0, y0) = (self.x.max(other.x), self.y.max(other.y));
        let (x1, y1) = (self.right().min(other.right()), self.bottom().min(other.bottom()));
        (x1 > x0 && y1 > y0).then(|| Rect { x: x0, y: y0, width: (x1 - x0) as u32, height: (y1 - y0) as u32 })
    }

    /// The same place and size but for the pixel or two a window manager rounds.
    pub fn near(&self, other: &Rect) -> bool {
        (self.x - other.x).abs() <= 2 && (self.y - other.y).abs() <= 2 && self.width.abs_diff(other.width) <= 2 && self.height.abs_diff(other.height) <= 2
    }
}

/// A display: its name (RandR's, `DP-1`, `eDP-1`: stable for its connector), whether the desktop
/// marks it as the main one, its bounds and its usable area (the bounds without panels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub name: String,
    pub primary: bool,
    pub bounds: Rect,
    pub work: Rect,
}

/// A display's usable area: of the desktop's work areas (one per monitor where the window manager
/// says so, `_GTK_WORKAREAS`; else one over all, `_NET_WORKAREA`), the part that overlaps it most;
/// its whole bounds when none does.
pub fn usable(bounds: Rect, areas: &[Rect]) -> Rect {
    areas.iter().filter_map(|area| bounds.intersect(area)).max_by_key(|r| r.width as u64 * r.height as u64).unwrap_or(bounds)
}

/// Size limits the window manager is asked to hold, device pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub min: (u32, u32),
    pub max: (u32, u32),
}

/// Where the window belongs: on which display, its place and size, and the limits it is held to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub display: String,
    pub rect: Rect,
    pub limits: Option<Limits>,
}

/// The desktop's main display: the one it marks primary, else the first. Not the one under the
/// pointer.
pub fn main_display(displays: &[Display]) -> Option<&Display> {
    displays.iter().find(|d| d.primary).or_else(|| displays.first())
}

/// The display `point` is on, else the nearest one.
pub fn display_at(displays: &[Display], point: (i32, i32)) -> Option<&Display> {
    displays.iter().find(|d| d.bounds.contains(point)).or_else(|| displays.iter().min_by_key(|d| d.bounds.distance(point)))
}

fn part(length: u32, share: f32) -> u32 {
    (length as f32 * share).round() as u32
}

/// A share as kept: finite, within `low..=high`; else `fallback`.
fn share(value: Option<f32>, fallback: f32, low: f32, high: f32) -> f32 {
    value.filter(|s| s.is_finite()).unwrap_or(fallback).clamp(low, high)
}

/// Where the window goes in `mode` with these displays; `None` without any.
pub fn target(mode: Mode, settings: &Settings, displays: &[Display]) -> Option<Target> {
    let main = main_display(displays)?;
    if mode == Mode::Shelf {
        let work = main.work;
        return Some(Target {
            display: main.name.clone(),
            rect: Rect { width: part(work.width, settings.shelf_share_on(&main.name)), ..work },
            // the height is the usable area's, held there: the right corners change the width
            limits: Some(Limits { min: (part(work.width, SHELF_MIN), work.height), max: (part(work.width, SHELF_MAX), work.height) }),
        });
    }
    let display = settings.display.as_deref().and_then(|name| displays.iter().find(|d| d.name == name)).unwrap_or(main);
    let prefs = settings.displays.get(&display.name).cloned().unwrap_or_default();
    let work = display.work;
    let width = part(work.width, share(prefs.work_width, WORK_SHARE, 0.05, 1.0));
    let height = part(work.height, share(prefs.work_height, 1.0, 0.05, 1.0));
    // inside the usable area, whatever was kept: a window never comes back off the screen
    let x = work.x + part(work.width, share(prefs.work_x, 0.0, 0.0, 1.0)) as i32;
    let y = work.y + part(work.height, share(prefs.work_y, 0.0, 0.0, 1.0)) as i32;
    Some(Target {
        display: display.name.clone(),
        rect: Rect { x: x.min(work.right() - width as i32).max(work.x), y: y.min(work.bottom() - height as i32).max(work.y), width, height },
        limits: None,
    })
}

/// The person's own geometry `rect` in `mode`, kept in `settings`: in Shelf mode its width on the
/// main display; in Drawer and Full its size and place on the display it is on, which becomes the
/// one it comes back on. Whether anything changed.
pub fn remember(mode: Mode, settings: &mut Settings, displays: &[Display], rect: Rect) -> bool {
    if mode == Mode::Shelf {
        let Some(main) = main_display(displays).filter(|d| d.work.width > 0) else {
            return false;
        };
        let width = (rect.width as f32 / main.work.width as f32).clamp(SHELF_MIN, SHELF_MAX);
        let prefs = settings.displays.entry(main.name.clone()).or_default();
        let changed = prefs.shelf_width != Some(width);
        prefs.shelf_width = Some(width);
        return changed;
    }
    let Some(display) = display_at(displays, rect.center()) else {
        return false;
    };
    let work = display.work;
    if work.width == 0 || work.height == 0 {
        return false;
    }
    let (w, h) = (work.width as f32, work.height as f32);
    let prefs = settings.displays.entry(display.name.clone()).or_default();
    let old = prefs.clone();
    *prefs = DisplayPrefs {
        work_width: Some((rect.width as f32 / w).clamp(0.05, 1.0)),
        work_height: Some((rect.height as f32 / h).clamp(0.05, 1.0)),
        work_x: Some(((rect.x - work.x) as f32 / w).clamp(0.0, 1.0)),
        work_y: Some(((rect.y - work.y) as f32 / h).clamp(0.0, 1.0)),
        ..old.clone()
    };
    let changed = *prefs != old || settings.display.as_deref() != Some(&display.name);
    settings.display = Some(display.name.clone());
    changed
}

/// What the window's geometry, read now, asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Put the window where it belongs again.
    Place,
    /// The person's own change, held still: keep it (`remember`).
    Keep(Rect),
}

/// How long after Silli places the window, or the displays change, the window's changes are the
/// window manager's settling it rather than the person's.
const QUIET: Duration = Duration::from_millis(800);
/// How long the person's change holds still before it is kept: the end of a drag.
const SETTLE: Duration = Duration::from_millis(300);
/// After the window says it was resized or moved: read it often for this long.
const STIRRED: Duration = Duration::from_millis(1500);

/// The window's geometry over time: what Silli asked for, what the window manager did with it,
/// and what the person did. The person's changes are kept when a drag ends (or the window hides,
/// changes mode or quits); Silli's own resizes are never taken for the person's.
#[derive(Debug)]
pub struct Controller {
    /// The displays as last read; empty until read (and without X11).
    pub displays: Vec<Display>,
    /// The geometry Silli last asked for, or kept as the person's: what the window should be.
    expected: Option<Rect>,
    /// The window as last read.
    seen: Option<Rect>,
    quiet_until: Option<Instant>,
    /// When the quiet ends, put the window back once should the window manager have moved it.
    recheck: bool,
    /// That once is now: the placement it asks for is not checked again.
    rechecking: bool,
    /// The person's change waiting to hold still, and since when it has.
    pending: Option<(Rect, Instant)>,
    stirred_until: Option<Instant>,
    quiet: Duration,
    settle: Duration,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            displays: Vec::new(),
            expected: None,
            seen: None,
            quiet_until: None,
            recheck: false,
            rechecking: false,
            pending: None,
            stirred_until: None,
            quiet: QUIET,
            settle: SETTLE,
        }
    }
}

impl Controller {
    /// Without waiting: for tests, which do not sleep.
    #[cfg(test)]
    pub fn immediate() -> Self {
        Self { quiet: Duration::ZERO, settle: Duration::ZERO, ..Self::default() }
    }

    fn quiet(&self, now: Instant) -> bool {
        self.quiet_until.is_some_and(|until| now < until)
    }

    /// Read often: settling after a placement, a change waiting to hold still, or the window
    /// just resized or moved.
    pub fn busy(&self, now: Instant) -> bool {
        self.quiet(now) || self.recheck || self.pending.is_some() || self.stirred_until.is_some_and(|until| now < until)
    }

    /// The window says it was resized or moved.
    pub fn stir(&mut self, now: Instant) {
        self.stirred_until = Some(now + STIRRED);
    }

    /// Silli placed the window at `rect` with these displays (`None`: nowhere to put it).
    pub fn placed(&mut self, displays: Vec<Display>, rect: Option<Rect>, now: Instant) {
        self.displays = displays;
        if rect.is_some() {
            self.expected = rect;
        }
        self.pending = None;
        self.quiet_until = Some(now + self.quiet);
        self.recheck = !std::mem::take(&mut self.rechecking);
    }

    /// The displays and the shown window as read now, and where the window belongs with these
    /// displays: what to do about it.
    pub fn observe(&mut self, displays: Vec<Display>, window: Option<Rect>, target: Option<&Target>, now: Instant) -> Step {
        if window.is_some() {
            self.seen = window;
        }
        if displays != self.displays {
            self.displays = displays;
            // the window manager moves windows about on a display change too: not the person
            self.pending = None;
            self.quiet_until = Some(now + self.quiet);
            self.recheck = true;
            return match (target, self.expected) {
                (Some(t), Some(expected)) if t.rect.near(&expected) => Step::Nothing,
                (Some(_), _) => Step::Place,
                (None, _) => Step::Nothing,
            };
        }
        let Some(window) = window else {
            return Step::Nothing;
        };
        if self.quiet(now) {
            return Step::Nothing;
        }
        self.quiet_until = None;
        let Some(expected) = self.expected else {
            // never placed (no displays to place it on): what it is is what it should be
            self.expected = Some(window);
            return Step::Nothing;
        };
        if std::mem::take(&mut self.recheck) && !window.near(&expected) {
            // once: a window manager that keeps it elsewhere (a tiling one) is not fought
            self.rechecking = true;
            return Step::Place;
        }
        if window.near(&expected) {
            self.pending = None;
            return Step::Nothing;
        }
        match self.pending {
            Some((rect, since)) if rect == window => {
                if now.duration_since(since) < self.settle {
                    return Step::Nothing;
                }
                self.pending = None;
                self.expected = Some(window);
                Step::Keep(window)
            }
            _ => {
                self.pending = Some((window, now));
                Step::Nothing
            }
        }
    }

    /// The window hides, changes mode or quits: the person's change to keep now, if there is one —
    /// in `window`, read now, else as last seen. Never while the window manager is settling it.
    pub fn flush(&mut self, window: Option<Rect>, now: Instant) -> Option<Rect> {
        let window = window.or(self.seen)?;
        let pending = self.pending.take();
        if self.quiet(now) || self.recheck {
            return None;
        }
        let expected = self.expected?;
        if window.near(&expected) && pending.is_none() {
            return None;
        }
        self.expected = Some(window);
        Some(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }

    /// A laptop at the left and a primary external display at its right with a 40 px panel at
    /// the bottom.
    fn two() -> Vec<Display> {
        vec![
            Display { name: "eDP-1".into(), primary: false, bounds: rect(0, 0, 1920, 1200), work: rect(0, 0, 1920, 1200) },
            Display { name: "DP-1".into(), primary: true, bounds: rect(1920, 0, 2560, 1440), work: rect(1920, 0, 2560, 1400) },
        ]
    }

    #[test]
    fn the_usable_area_is_the_work_area_over_the_display() {
        let bounds = rect(1920, 0, 2560, 1440);
        assert_eq!(usable(bounds, &[]), bounds, "no work area: the whole display");
        // one work area over every display (_NET_WORKAREA)
        assert_eq!(usable(bounds, &[rect(0, 0, 4480, 1400)]), rect(1920, 0, 2560, 1400));
        // one per display (_GTK_WORKAREAS): the one on this display
        assert_eq!(usable(bounds, &[rect(0, 32, 1920, 1168), rect(1920, 0, 2560, 1400)]), rect(1920, 0, 2560, 1400));
        assert_eq!(usable(bounds, &[rect(0, 0, 1920, 1200)]), bounds, "none over it");
    }

    #[test]
    fn the_shelf_is_on_the_main_display_s_left_edge_at_full_height() {
        let displays = two();
        let mut settings = Settings::default();
        let t = target(Mode::Shelf, &settings, &displays).unwrap();
        assert_eq!(t.display, "DP-1", "the primary, not the first");
        assert_eq!(t.rect, rect(1920, 0, 896, 1400), "35 % of 2560, panel excluded");
        assert_eq!(t.limits, Some(Limits { min: (384, 1400), max: (1024, 1400) }), "15–40 % wide, the height held");
        // dragged wider on this display: kept for it, the other keeps the default
        assert!(remember(Mode::Shelf, &mut settings, &displays, rect(1920, 0, 768, 1400)));
        assert_eq!(target(Mode::Shelf, &settings, &displays).unwrap().rect.width, 768);
        let mut laptop = displays.clone();
        laptop[0].primary = true;
        laptop[1].primary = false;
        assert_eq!(target(Mode::Shelf, &settings, &laptop).unwrap().rect, rect(0, 0, 672, 1200));
        // past the bounds: held to them
        remember(Mode::Shelf, &mut settings, &displays, rect(1920, 0, 2000, 1400));
        assert_eq!(settings.displays["DP-1"].shelf_width, Some(SHELF_MAX));
        // without a display there is nowhere to go
        assert_eq!(target(Mode::Shelf, &settings, &[]), None);
    }

    #[test]
    fn a_free_window_comes_back_where_it_was_left_on_its_display() {
        let displays = two();
        let mut settings = Settings::default();
        // nothing kept yet: half the main display's width, its full height, at its top-left
        let first = target(Mode::Drawer, &settings, &displays).unwrap();
        assert_eq!(first.rect, rect(1920, 0, 1280, 1400));
        assert_eq!(first.limits, None);
        // moved to the laptop and sized there
        assert!(remember(Mode::Drawer, &mut settings, &displays, rect(480, 300, 960, 600)));
        assert_eq!(settings.display.as_deref(), Some("eDP-1"));
        let t = target(Mode::Full, &settings, &displays).unwrap();
        assert_eq!((t.display.as_str(), t.rect), ("eDP-1", rect(480, 300, 960, 600)));
        // the laptop's display gone: on the main display, its own size, the laptop's kept
        let external = vec![displays[1].clone()];
        let t = target(Mode::Drawer, &settings, &external).unwrap();
        assert_eq!((t.display.as_str(), t.rect), ("DP-1", rect(1920, 0, 1280, 1400)));
        assert_eq!(settings.display.as_deref(), Some("eDP-1"));
        assert!(settings.displays.contains_key("eDP-1"));
        // back: where it was
        assert_eq!(target(Mode::Drawer, &settings, &displays).unwrap().rect, rect(480, 300, 960, 600));
    }

    #[test]
    fn a_kept_place_never_puts_the_window_off_its_display() {
        let displays = two();
        let mut settings = Settings::default();
        // half off the laptop's left edge, its centre still on it
        remember(Mode::Drawer, &mut settings, &displays, rect(-400, 800, 1000, 600));
        let t = target(Mode::Drawer, &settings, &displays).unwrap();
        assert_eq!(t.rect, rect(0, 600, 1000, 600), "inside the usable area");
        // a place kept far right: the window still ends at the display's edge
        settings.displays.get_mut("eDP-1").unwrap().work_x = Some(0.9);
        let t = target(Mode::Drawer, &settings, &displays).unwrap();
        assert_eq!(t.rect.x + t.rect.width as i32, 1920);
        // a hand-edited share that is no number
        settings.displays.get_mut("eDP-1").unwrap().work_width = Some(f32::NAN);
        assert_eq!(target(Mode::Drawer, &settings, &displays).unwrap().rect.width, 960);
    }

    #[test]
    fn silli_s_own_placement_is_not_taken_for_the_person_s() {
        let displays = two();
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut c = Controller::default();
        let placed = rect(1920, 0, 896, 1400);
        c.placed(displays.clone(), Some(placed), t0);
        let target = Target { display: "DP-1".into(), rect: placed, limits: None };
        // the window manager moves it while settling: not the person
        let moved = rect(1950, 30, 896, 1370);
        assert_eq!(c.observe(displays.clone(), Some(moved), Some(&target), at(100)), Step::Nothing);
        assert!(c.busy(at(100)));
        // the quiet over and still elsewhere: put back, once
        assert_eq!(c.observe(displays.clone(), Some(moved), Some(&target), at(900)), Step::Place);
        c.placed(displays.clone(), Some(placed), at(900));
        // kept elsewhere again (a tiling window manager): taken as it is, not fought
        assert_eq!(c.observe(displays.clone(), Some(moved), Some(&target), at(1800)), Step::Nothing);
        assert_eq!(c.observe(displays.clone(), Some(moved), Some(&target), at(2200)), Step::Keep(moved));
        assert!(!c.busy(at(2200)));
        // a 1 px rounding is the same place
        let mut c = Controller::default();
        c.placed(displays.clone(), Some(placed), t0);
        let rounded = rect(1921, 0, 895, 1400);
        assert_eq!(c.observe(displays.clone(), Some(rounded), Some(&target), at(900)), Step::Nothing);
        assert_eq!(c.observe(displays, Some(rounded), Some(&target), at(1500)), Step::Nothing);
    }

    #[test]
    fn the_person_s_resize_is_kept_when_it_holds_still() {
        let displays = two();
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut c = Controller::default();
        let placed = rect(1920, 0, 896, 1400);
        c.placed(displays.clone(), Some(placed), t0);
        let target = Target { display: "DP-1".into(), rect: placed, limits: None };
        let _ = c.observe(displays.clone(), Some(placed), Some(&target), at(900));
        // dragging: each new width waits
        for (ms, width) in [(1000, 920), (1200, 980), (1400, 1000)] {
            let r = rect(1920, 0, width, 1400);
            assert_eq!(c.observe(displays.clone(), Some(r), Some(&target), at(ms)), Step::Nothing);
        }
        assert!(c.busy(at(1500)));
        let dragged = rect(1920, 0, 1000, 1400);
        assert_eq!(c.observe(displays.clone(), Some(dragged), Some(&target), at(1800)), Step::Keep(dragged));
        // kept: no longer a change
        assert_eq!(c.observe(displays.clone(), Some(dragged), Some(&target), at(2000)), Step::Nothing);
        // hidden mid-drag: kept at once
        let wider = rect(1920, 0, 1010, 1400);
        let _ = c.observe(displays.clone(), Some(wider), Some(&target), at(2100));
        assert_eq!(c.flush(None, at(2150)), Some(wider));
        assert_eq!(c.flush(None, at(2200)), None, "and only once");
        // a display change: the window manager's moves are not the person's, and the window goes
        // where it belongs
        let _ = c.observe(displays.clone(), Some(rect(1920, 0, 1100, 1400)), Some(&target), at(2300));
        let laptop = vec![displays[0].clone()];
        let elsewhere = Target { display: "eDP-1".into(), rect: rect(0, 0, 672, 1200), limits: None };
        assert_eq!(c.observe(laptop.clone(), Some(rect(0, 0, 1100, 1200)), Some(&elsewhere), at(2400)), Step::Place);
        assert_eq!(c.flush(Some(rect(0, 0, 1100, 1200)), at(2500)), None, "settling");
    }
}
