# Patched upstream crates

Copies of crates from crates.io with a small change sill needs, wired in with `[patch.crates-io]`
in the workspace `Cargo.toml`. Each change is marked `sill patch` in the source. When the
upstream crate is upgraded, copy the new release here and apply the change again, or drop the copy
once upstream has an equivalent.

## `iced_widget` 0.14.2 (MIT)

- `src/markdown.rs`: `Row::cells()`, a public accessor for a Markdown table row's cells (the
  field is private upstream). sill-gui's Markdown viewer draws tables itself so their sideways
  scrollbar is the window's thin 3 px one rather than iced's fixed 10 px
  (`crates/sill-gui/src/viewer.rs`, the person's request of 2026-11-05).
- `src/pick_list.rs`: `menu_width`, `menu_padding`, `menu_text_size` and `menu_gap`, so a pick
  list's menu can be wider, roomier and set apart from a compact list (upstream gives the menu the
  list's own width, padding and text size). sill-gui's agent controls are small labels whose menus
  read as menus (the person's request of 2026-11-05). Unset, upstream's behaviour.
- `src/overlay/menu.rs` and `src/pick_list.rs`: the overlay menu closes itself — `Menu::closes`
  clears the caller's open flag on a choice or on a press outside the list, `Menu::on_dismiss`
  produces a message for such a press — and captures that press. Upstream leaves closing to the
  pick list's own `update`, which never sees a press that a widget earlier in the tree (the prompt
  editor) captures first, so the menu stayed open (the person, 2026-11-08). The pick list uses
  both; sill-gui's `popup.rs` (the shield's right-click menu) uses `on_dismiss`. The menu also
  keeps its width near the window's right edge and moves left to fit (upstream limits it to the
  room right of its control, which hid the shield's menu once the shield sat before Send).
- `src/text_editor.rs`: optional `line_numbers` gutter, supplied with visible logical line
  positions from the renderer. Gutter width tracks the number of digits; source layout, pointer
  hit testing and IME positioning use the same inset. Unwrapped editors pass sideways/Shift-wheel
  events to a parent horizontal scrollbar: an event whose larger axis is sideways (a trackpad
  reports both axes in nearly every event), the rest scrolls the editor. Default editors stay
  unchanged.
- `src/text_editor.rs`: optional `marks`, a callback giving rectangles (in the text's own
  coordinates) with their colours, filled behind the text. sill-gui's Find draws its matches in the
  reader's source with it (`find::mark_rects`): upstream draws the selection only while the editor
  has the focus, and during Find the focus is in the find bar.
- `src/scrollable.rs`: a wheel event along an axis the scrollable does not scroll reaches its
  content also just after the scrollable scrolled. Upstream withholds every wheel event from the
  content for up to 1.5 s then; around the source editor that froze vertical scrolling after any
  sideways movement (the person, 2026-11-14, on a Mac trackpad).
- `src/lib.rs`: `#![allow(deprecated)]`, so upstream's own deprecation warnings do not show in
  sill's builds (a path dependency's warnings are not capped as a registry crate's are).

Worth proposing upstream: make the cells readable, or the table's scrollbar configurable; a pick
list's menu width, padding, text size and gap; a menu that closes on a press outside it.
