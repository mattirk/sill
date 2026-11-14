//! Drawer modes and the root rows belonging to each view, independent of GTK and filesystem I/O.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A caption row starts with this marker, rather than a filesystem path.
pub(super) const SECTION: &str = "\u{1}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DrawerView {
    Files,
    Tree,
    Diff,
}

impl DrawerView {
    pub(super) fn next(self) -> Self {
        match self {
            Self::Tree => Self::Diff,
            Self::Diff => Self::Files,
            Self::Files => Self::Tree,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Tree => "Pinned",
            Self::Diff => "Diff",
        }
    }

    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::Files => "sill-files",
            Self::Tree => "sill-list-tree",
            Self::Diff => "sill-git-compare",
        }
    }
}

pub(super) fn root_rows(
    view: DrawerView,
    scope: Option<&Path>,
    pinned: &[PathBuf],
    global: &[PathBuf],
    touched: &[PathBuf],
    home: &Path,
) -> Vec<String> {
    let text = |path: &Path| path.to_string_lossy().into_owned();
    if let Some(scope) = scope {
        return vec![text(scope)];
    }
    if view != DrawerView::Files {
        return pinned.iter().map(|path| text(path)).collect();
    }

    let mut wanted = Vec::new();
    let mut seen = HashSet::new();
    for (caption, paths) in [
        ("Pinned here", pinned),
        ("Global pins", global),
        ("Repositories with changes", touched),
    ] {
        let rows: Vec<_> = paths.iter()
            .filter(|path| seen.insert(path.as_path()))
            .map(|path| text(path))
            .collect();
        if !rows.is_empty() {
            wanted.push(format!("{SECTION}{caption}"));
            wanted.extend(rows);
        }
    }
    if seen.insert(home) {
        wanted.push(text(home));
    }
    wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_tree_diff_files() {
        assert_eq!(DrawerView::Tree.next(), DrawerView::Diff);
        assert_eq!(DrawerView::Diff.next(), DrawerView::Files);
        assert_eq!(DrawerView::Files.next(), DrawerView::Tree);
    }


    #[test]
    fn pinned_views_keep_files_and_exclude_unrelated_roots() {
        let pinned = [PathBuf::from("/project"), PathBuf::from("/project/README.md")];
        let global = [PathBuf::from("/global")];
        let touched = [PathBuf::from("/other-session")];
        for view in [DrawerView::Tree, DrawerView::Diff] {
            assert_eq!(
                root_rows(view, None, &pinned, &global, &touched, Path::new("/home/person")),
                ["/project", "/project/README.md"],
            );
        }
    }

    #[test]
    fn files_groups_unique_roots_and_adds_home_only_when_absent() {
        let home = Path::new("/home/person");
        let pinned = [PathBuf::from("/project"), home.to_path_buf()];
        let global = [PathBuf::from("/project"), PathBuf::from("/global")];
        let touched = [PathBuf::from("/global"), PathBuf::from("/changed")];
        assert_eq!(
            root_rows(DrawerView::Files, None, &pinned, &global, &touched, home),
            [
                "\u{1}Pinned here", "/project", "/home/person",
                "\u{1}Global pins", "/global",
                "\u{1}Repositories with changes", "/changed",
            ],
        );
        assert_eq!(
            root_rows(DrawerView::Files, None, &[], &global[1..], &[], home),
            ["\u{1}Global pins", "/global", "/home/person"],
        );
        assert_eq!(
            root_rows(DrawerView::Files, None, &[], &[], &[], home),
            ["/home/person"],
        );
    }

    #[test]
    fn scoped_views_show_only_the_requested_root() {
        let pinned = [PathBuf::from("/project")];
        let global = [PathBuf::from("/global")];
        let touched = [PathBuf::from("/changed")];
        for view in [DrawerView::Files, DrawerView::Tree, DrawerView::Diff] {
            assert_eq!(
                root_rows(view, Some(Path::new("/chosen/subfolder")), &pinned, &global, &touched, Path::new("/home/person")),
                ["/chosen/subfolder"],
            );
        }
    }
}
