//! Viewer syntax selection and bounded caching of immutable diff highlights.
use crate::style::{self, P};
use iced::{
    Color,
    highlighter::{Settings, Stream, Theme},
};
use std::{cell::RefCell, collections::VecDeque, ops::Range, rc::Rc};

type Lines = Vec<Vec<(Range<usize>, Color)>>;
struct Cached {
    source: String,
    path: String,
    theme: Theme,
    lines: Rc<Lines>,
}
thread_local! { static DIFFS: RefCell<VecDeque<Cached>> = const {RefCell::new(VecDeque::new())}; }

pub fn token(path: &str) -> &str {
    let path = path.trim_matches('"');
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "Dockerfile" | "Containerfile" => "Dockerfile",
        "Makefile" | "GNUmakefile" => "Makefile",
        ".bashrc" | ".bash_profile" | ".zshrc" => "sh",
        _ => name.rsplit_once('.').map_or("txt", |(_, ext)| ext),
    }
}
pub fn theme() -> Theme {
    if style::theme().palette().background.r > 0.5 {
        Theme::InspiredGitHub
    } else {
        Theme::Base16Ocean
    }
}
fn stream(path: &str, theme: Theme) -> Stream {
    Stream::new(&Settings {
        theme,
        token: token(path).into(),
    })
}
fn line(stream: &mut Stream, source: &str, offset: usize) -> Vec<(Range<usize>, Color)> {
    let spans = stream
        .highlight_line(source)
        .map(|(r, h)| {
            (
                (r.start + offset)..(r.end + offset),
                h.color().unwrap_or(P.fg),
            )
        })
        .collect();
    stream.commit();
    spans
}

/// Keep old/new parse state separate: a deleted multiline comment must not colour added code.
fn parse(source: &str, path: &str, theme: Theme) -> Lines {
    let mut old = stream(path, theme);
    let mut new = stream(path, theme);
    let untracked = source.starts_with("(untracked)");
    let mut hunk = false;
    source
        .lines()
        .enumerate()
        .map(|(index, text)| {
            if untracked {
                return if index == 0 {
                    vec![]
                } else {
                    line(&mut new, text, 0)
                };
            }
            if text.starts_with("diff ") {
                hunk = false;
            }
            if !hunk && (text.starts_with("--- ") || text.starts_with("+++ ")) {
                let path = text[4..].split('\t').next().unwrap_or("");
                let path = path
                    .strip_prefix("a/")
                    .or_else(|| path.strip_prefix("b/"))
                    .unwrap_or(path);
                if path != "/dev/null" {
                    if text.starts_with("--- ") {
                        old = stream(path, theme);
                    } else {
                        new = stream(path, theme);
                    }
                }
            }
            if text.starts_with("@@ ") {
                old.reset();
                new.reset();
                hunk = true;
                return vec![];
            }
            if hunk {
                match text.as_bytes().first() {
                    Some(b'+') => return line(&mut new, &text[1..], 1),
                    Some(b'-') => return line(&mut old, &text[1..], 1),
                    Some(b' ') => {
                        let _ = line(&mut old, &text[1..], 1);
                        return line(&mut new, &text[1..], 1);
                    }
                    _ => {}
                }
            }
            vec![]
        })
        .collect()
}

pub fn diff(source: &str, path: &str) -> Rc<Lines> {
    let theme = theme();
    // The reader already bounds diffs; don't retain or parse unusually large previews.
    if source.len() > 2 * 1024 * 1024 {
        return Rc::new(vec![]);
    }
    DIFFS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(index) = cache
            .iter()
            .position(|c| c.source == source && c.path == path && c.theme == theme)
        {
            let item = cache.remove(index).unwrap();
            let result = item.lines.clone();
            cache.push_front(item);
            return result;
        }
        let lines = Rc::new(parse(source, path, theme));
        cache.push_front(Cached {
            source: source.into(),
            path: path.into(),
            theme,
            lines: lines.clone(),
        });
        cache.truncate(4);
        lines
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diff_highlights_preserve_unicode_and_separate_old_and_new_state() {
        let source = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-/* old comment\n+let café = 42;\n";
        let lines = parse(source, "a.rs", Theme::Base16Ocean);
        assert!(lines[..4].iter().all(Vec::is_empty));
        for (text, ranges) in source.lines().zip(&lines) {
            for (range, _) in ranges {
                assert!(text.get(range.clone()).is_some());
            }
        }
        assert!(lines[5].iter().all(|(r, _)| r.start >= 1));
        assert!(lines[5].windows(2).any(|w| w[0].1 != w[1].1));
        let again = parse("(untracked)\nlet café = 42;", "a.rs", Theme::Base16Ocean);
        assert_eq!(
            lines[5].iter().map(|(_, c)| c).collect::<Vec<_>>(),
            again[1].iter().map(|(_, c)| c).collect::<Vec<_>>()
        );
    }
    #[test]
    fn filenames_and_cached_revisions() {
        assert_eq!(token("/project/Dockerfile"), "Dockerfile");
        assert_eq!(token("main.rs"), "rs");
        assert_eq!(token("README"), "txt");
        let a = diff("(untracked)\nlet n = 1;", "a.rs");
        assert!(Rc::ptr_eq(&a, &diff("(untracked)\nlet n = 1;", "a.rs")));
        assert!(!Rc::ptr_eq(&a, &diff("(untracked)\nlet n = 2;", "a.rs")));
    }
}
