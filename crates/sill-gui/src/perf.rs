//! What a long conversation costs (docs/sill-gui.md, consolidation measurements): per streamed
//! event, before and after the consolidation's incremental rendering, and per full view build and
//! layout, which every update that redraws pays (rows are not virtualized). Ignored by default; run
//! optimized: `cargo test --release -p sill-gui perf -- --ignored --nocapture`.
use std::time::{Duration, Instant};

use sill_client::conversation::item_text;
use sill_proto::sfp::{Content, Event, Item, SessionEvent, Tool};

use crate::{App, SessionView, demo};

const PROSE: &str = "A paragraph of agent prose with **bold**, `inline code` and a [link](https://example.org), long enough to wrap twice at Shelf mode's width so layout has real work to do.\n\n- a first point\n- a second point with `code`\n\n```rust\nfn main() {\n    println!(\"sill\");\n}\n```\n";

fn item(id: usize, kind: &str, text: &str) -> Item {
    Item {
        item_id: format!("i{id:05}"),
        created_seq: id.to_string(),
        turn_id: Some(format!("t{}", id / 4)),
        submission_id: None,
        agent_generation_id: "g".into(),
        kind: kind.into(),
        content: vec![Content::text(text)],
        tool: None,
    }
}

/// The demo app with one session of `n` items: prompts, tool rows and Markdown answers.
pub(crate) fn long(n: usize) -> App {
    let (mut app, _) = App::new(true, None);
    let mut view = SessionView::new("demo");
    let mut conversation = sill_client::Conversation::new("demo", "demo-subscription", n as u64);
    conversation.session = demo::conversation().session;
    for i in 0..n {
        let mut next = match i % 4 {
            0 => item(
                i,
                "user",
                "Please look at the next part and explain what changed.",
            ),
            1 => item(i, "tool", ""),
            _ => item(i, "agent", PROSE),
        };
        if i % 4 == 1 {
            next.tool = Some(Tool {
                title: format!("rg -n 'pattern-{i}' crates"),
                kind: "execute".into(),
                status: "completed".into(),
                detail: "crates/sill-gui/src/ui.rs:1: match\n".repeat(3),
                diff: String::new(),
                parent_item_id: None,
            });
        }
        conversation.insert_item(next);
    }
    view.conversation = conversation;
    view.rebuild();
    app.views.insert("demo".into(), view);
    app
}

/// A headless window: frames built from the app's view, the widget cache kept between them.
struct Screen {
    renderer: iced::Renderer,
    cache: Option<iced_runtime::user_interface::Cache>,
}

impl Screen {
    fn new() -> Self {
        use iced::advanced::renderer::Headless;
        let settings = crate::snapshots::settings();
        let renderer = crate::tests::block_on(<iced::Renderer as Headless>::new(
            settings.default_font,
            settings.default_text_size,
            None,
        ))
        .expect("a headless renderer");
        Self {
            renderer,
            cache: Some(iced_runtime::user_interface::Cache::default()),
        }
    }

    /// One frame, split: the view built, laid out (`UserInterface::build`), and drawn.
    fn split(&mut self, app: &App) -> (Duration, Duration, Duration) {
        let start = Instant::now();
        let element = app.view();
        let built = start.elapsed();
        let start = Instant::now();
        let mut ui = iced_runtime::UserInterface::build(
            element,
            iced::Size::new(720.0, 860.0),
            self.cache.take().unwrap_or_default(),
            &mut self.renderer,
        );
        let laid = start.elapsed();
        let start = Instant::now();
        ui.draw(
            &mut self.renderer,
            &crate::style::theme(),
            &iced::advanced::renderer::Style {
                text_color: crate::style::P.fg,
            },
            iced::mouse::Cursor::Unavailable,
        );
        let drawn = start.elapsed();
        self.cache = Some(ui.into_cache());
        (built, laid, drawn)
    }

    fn frame(&mut self, app: &App) {
        let mut ui = iced_runtime::UserInterface::build(
            app.view(),
            iced::Size::new(720.0, 860.0),
            self.cache.take().unwrap_or_default(),
            &mut self.renderer,
        );
        ui.draw(
            &mut self.renderer,
            &crate::style::theme(),
            &iced::advanced::renderer::Style {
                text_color: crate::style::P.fg,
            },
            iced::mouse::Cursor::Unavailable,
        );
        self.cache = Some(ui.into_cache());
    }
}

fn time(mut work: impl FnMut()) -> Duration {
    let start = Instant::now();
    work();
    start.elapsed()
}

/// Image discovery runs after lane updates, including chunks without images. Measure its
/// transcript scan separately from rendering; no daemon or headless renderer is needed.
#[test]
#[ignore = "timing; run with --release"]
fn image_discovery_without_images() {
    for n in [250, 1000, 2000, 8000] {
        let mut app = long(n);
        app.actions = Some(sill_client::actions::Actions::new(std::sync::Arc::new(
            || anyhow::bail!("no image fetch expected"),
        )));
        let start = Instant::now();
        for _ in 0..1000 {
            let task = app.load_media("demo");
            assert!(task.units() == 0, "no-image transcript does not fetch");
            let _ = std::hint::black_box(task);
        }
        println!(
            "{n:>5} items: no-image discovery scan {:?} per lane update",
            start.elapsed() / 1000
        );
    }
}

#[test]
#[ignore = "timing; run with --release"]
fn cost_of_a_long_conversation() {
    for n in [250, 1000, 2000] {
        let mut app = long(n);
        let last = format!("i{:05}", n - 1);
        // per streamed event: now the changed item only
        let events = 200;
        let now = time(|| {
            for k in 0..events {
                let view = app.views.get_mut("demo").unwrap();
                let event = SessionEvent {
                    session_id: "demo".into(),
                    subscription_id: "demo-subscription".into(),
                    seq: (view.conversation.seq + 1).to_string(),
                    event: Event::ItemTextAppended {
                        item_id: last.clone(),
                        block_id: "text".into(),
                        text: format!(" more {k}"),
                    },
                };
                assert!(view.accept(event));
            }
        }) / events;
        // what every event cost before: each item's text rebuilt and compared with its cache
        let view = &app.views["demo"];
        let before = time(|| {
            for _ in 0..20 {
                for item in view.conversation.items().values() {
                    let source = item_text(item);
                    std::hint::black_box(
                        view.rendered
                            .get(&item.item_id)
                            .is_some_and(|r| r.source == source),
                    );
                }
            }
        }) / 20;
        // frames as the runtime runs them: the view built, laid out and drawn, with the widget
        // cache kept from the frame before — cold (the first after opening the session), then
        // warm, after each streamed event
        let mut screen = Screen::new();
        let cold = time(|| screen.frame(&app));
        // as the first scroll message tells it: the view's height, following the tail
        app.views.get_mut("demo").unwrap().viewport = 700.0;
        let mut warm = Duration::ZERO;
        let frames = 20;
        for k in 0..frames {
            let view = app.views.get_mut("demo").unwrap();
            let event = SessionEvent {
                session_id: "demo".into(),
                subscription_id: "demo-subscription".into(),
                seq: (view.conversation.seq + 1).to_string(),
                event: Event::ItemTextAppended {
                    item_id: last.clone(),
                    block_id: "text".into(),
                    text: format!(" frame {k}"),
                },
            };
            view.accept(event);
            warm += time(|| screen.frame(&app));
        }
        let warm = warm / frames;
        // scrolled away from the tail: the middle of the conversation
        {
            let view = app.views.get_mut("demo").unwrap();
            view.follow_tail = false;
            view.scroll_offset = view
                .heights
                .borrow()
                .values()
                .map(|(_, h)| h + 12.0)
                .sum::<f32>()
                / 2.0;
        }
        let middle = time(|| screen.frame(&app));
        println!("{n:>5} items: a frame scrolled to the middle {middle:>9.2?}");
        let (mut built, mut laid, mut drawn) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        for _ in 0..frames {
            let (b, l, d) = screen.split(&app);
            built += b;
            laid += l;
            drawn += d;
        }
        println!(
            "{n:>5} items: a warm frame is view {:>9.2?}, layout {:>9.2?}, draw {:>9.2?}",
            built / frames,
            laid / frames,
            drawn / frames
        );
        let snapshot = time(|| app.views.get_mut("demo").unwrap().rebuild());
        println!(
            "{n:>5} items: per event {now:>9.2?} (before the change {before:>9.2?}); frame cold \
             {cold:>9.2?}, warm {warm:>9.2?}; Markdown rebuild after a snapshot {snapshot:>9.2?}"
        );
    }
}
