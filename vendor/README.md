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
- `src/lib.rs`: `#![allow(deprecated)]`, so upstream's own deprecation warnings do not show in
  sill's builds (a path dependency's warnings are not capped as a registry crate's are).

Worth proposing upstream: make the cells readable, or the table's scrollbar configurable.
