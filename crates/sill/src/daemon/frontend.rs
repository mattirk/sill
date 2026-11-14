//! Host data access for frontends. The renderer never opens the daemon's database or paths.
use std::path::Path;
use anyhow::Result;
#[cfg(test)]
use sill_core::frontend::Metadata;
use super::Hub;

pub(super) static CONFIG: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn open_session_image(hub:&Hub,session:&str,src:&Path,home:&Path)->Result<Option<std::fs::File>> {
    use std::path::Component;
    let p = if src.is_absolute() { src.to_path_buf() } else { home.join(src) };
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "image path cannot contain ..").into());
    }
    if p.starts_with("/.sill") {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "shim internals are not image sources").into());
    }
    let image = p.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp"));
    if !image { return Ok(None) }
    let socket = sill_core::shim::runtime_dir(session)?.join("shim.sock");
    match sill_core::shim::Control::connect(&socket) {
        Ok(mut shim) => return shim.open_image(&p).map(Some),
        Err(e) => {
            // A stopped jail may still have persistent overlay images. A live supervisor's
            // read failure must never fall back to a different file on the host.
            let stopped = e.downcast_ref::<std::io::Error>().is_some_and(|e| matches!(e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused));
            if !stopped { return Err(e); }
        }
    }
    let Some(path) = stored_image_path(hub,session,&p)? else { return Ok(None) };
    use std::os::unix::fs::OpenOptionsExt;
    Ok(Some(std::fs::OpenOptions::new().read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?))
}

fn stored_image_path(hub:&Hub,session:&str,p:&Path)->Result<Option<std::path::PathBuf>> {
    use std::os::unix::fs::FileTypeExt;
    // The pins its jail was launched with; a session that never launched has no jail and no
    // overlay, so a path under a pin it will mount is the host's file (its stored pins decide).
    let pins = match sill_core::session::policy_snapshot(session)? {
        Some(launch) => launch.launch_pins,
        None => hub.with_store(|s| s.jail_pins(session))?,
    };
    if pins.iter().any(|pin| pin.is_dir() && p.starts_with(pin)) { return Ok(Some(p.to_path_buf())) }
    // These mounts disappear with the jail; a same-named host file is unrelated.
    // Explicit pins take precedence, just as they do when constructing the jail.
    if ["/tmp", "/run", "/proc", "/dev", "/sys"].iter().any(|root| p.starts_with(root)) {
        return Ok(None);
    }
    let view = hub.jail_view(session)?;
    if view.masked.iter().any(|mask| p.starts_with(mask)) { return Ok(None) }
    for (host, upper) in view.layers {
        if let Ok(rel) = p.strip_prefix(host) {
            let upper = upper.join(rel);
            if let Ok(m) = std::fs::symlink_metadata(&upper) {
                if m.file_type().is_char_device() { return Ok(None) }
                if m.is_file() { return Ok(Some(upper)) }
            }
        }
    }
    Ok(Some(p.to_path_buf()))
}

#[cfg(test)]
fn metadata(path: &Path) -> Result<Metadata> {
    let m = std::fs::symlink_metadata(path)?;
    Ok(Metadata { file: m.is_file(), directory: m.is_dir(), symlink: m.is_symlink(), target: if m.is_symlink() { std::fs::read_link(path).ok() } else { None }, length: m.len(), readonly: m.permissions().readonly() })
}

pub(super) fn head_label(repo: &Path) -> Option<(String, bool)> {
    let dot = repo.join(".git");
    let gitdir = if dot.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let p = Path::new(text.strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p.to_path_buf() } else { repo.join(p) }
    } else { dot };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref:") {
        Some(r) => (r.trim().strip_prefix("refs/heads/").unwrap_or(r.trim()).to_string(), false),
        None => (head.chars().take(7).collect(), true),
    })
}

pub(super) fn find_files(bases: &[std::path::PathBuf], q: &str, cap: usize, visits: usize) -> (Vec<std::path::PathBuf>, bool) {
    let mut found = Vec::new();
    let mut matched = std::collections::HashSet::new();
    let mut seen = 0;
    let mut cut = false;
    'bases: for base in bases {
        let Ok(metadata) = std::fs::symlink_metadata(base) else { continue };
        if metadata.is_file() {
            seen += 1;
            if seen > visits || found.len() >= cap { cut = true; break }
            let hay = if q.contains('/') {
                base.to_string_lossy().to_lowercase()
            } else {
                base.file_name().unwrap_or_default().to_string_lossy().to_lowercase()
            };
            if hay.contains(q) && matched.insert(base.clone()) { found.push(base.clone()); }
            continue;
        }
        if !metadata.is_dir() { continue }
        let mut queue = std::collections::VecDeque::from([base.clone()]);
        while let Some(dir) = queue.pop_front() {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                seen += 1;
                if seen > visits || found.len() >= cap { cut = true; break 'bases }
                let name = e.file_name().to_string_lossy().into_owned();
                let path = e.path();
                let hay = if q.contains('/') { path.strip_prefix(base).map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default() } else { name.to_lowercase() };
                if hay.contains(q) && matched.insert(path.clone()) { found.push(path.clone()); }
                if e.file_type().is_ok_and(|t| t.is_dir()) && !name.starts_with('.') && !matches!(name.as_str(), "target" | "node_modules" | "__pycache__" | "build" | "dist") { queue.push_back(path); }
            }
        }
    }
    found.sort();
    (found, cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A search finds by name, by path with a slash, skips noise folders, and says when cut.
    #[test]
    fn find_files_by_name_and_path() {
        let d = std::env::temp_dir().join(format!("sill-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for f in ["src/main.rs", "src/ui/tree.rs", "docs/Tree.md", "target/debug/tree.d", "node_modules/x/tree.js"] {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
        }
        let (found, cut) = find_files(&[d.clone()], "tree", 100, 10_000);
        let rel: Vec<String> = found.iter().map(|p| p.strip_prefix(&d).unwrap().display().to_string()).collect();
        assert_eq!(rel, vec!["docs/Tree.md", "src/ui/tree.rs"], "noise folders skipped, case folded");
        assert!(!cut);
        let (found, _) = find_files(&[d.clone()], "ui/tr", 100, 10_000);
        assert_eq!(found, vec![d.join("src/ui/tree.rs")]);
        let (found, cut) = find_files(&[d.clone()], "", 2, 10_000);
        assert_eq!(found.len(), 2);
        assert!(cut);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn find_files_includes_explicit_file_bases() {
        let d = std::env::temp_dir().join(format!("sill-find-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("notes")).unwrap();
        let file = d.join("notes/Plan.md");
        std::fs::write(&file, "").unwrap();
        for q in ["plan", "notes/plan"] {
            assert_eq!(find_files(&[file.clone()], q, 100, 10_000), (vec![file.clone()], false));
        }
        assert_eq!(find_files(&[file.clone()], "absent", 100, 10_000), (vec![], false));
        assert_eq!(find_files(&[d.clone(), file.clone()], "plan", 100, 10_000), (vec![file], false));
        let link = d.join("Plan-link.md");
        std::os::unix::fs::symlink("notes/Plan.md", &link).unwrap();
        assert_eq!(find_files(&[link], "plan", 100, 10_000), (vec![], false));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn find_files_explicit_files_respect_limits() {
        let d = std::env::temp_dir().join(format!("sill-find-file-limits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("Plan.md");
        std::fs::write(&file, "").unwrap();
        assert_eq!(find_files(&[file.clone()], "plan", 0, 100), (vec![], true));
        assert_eq!(find_files(&[file.clone()], "plan", 100, 0), (vec![], true));
        assert_eq!(find_files(&[file.clone()], "plan", 1, 1), (vec![file], false));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// The root row's checkout label: a branch by name, a detached HEAD by its short hash, a
    /// worktree through its gitdir file.
    #[test]
    fn head_label_reads_the_checkout() {
        let d = std::env::temp_dir().join(format!("sill-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("repo/.git")).unwrap();
        std::fs::write(d.join("repo/.git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(head_label(&d.join("repo")), Some(("feature/x".to_string(), false)));
        std::fs::write(d.join("repo/.git/HEAD"), "a8cf233deadbeef0123456789abcdef012345678\n").unwrap();
        assert_eq!(head_label(&d.join("repo")), Some(("a8cf233".to_string(), true)));
        std::fs::create_dir_all(d.join("wt")).unwrap();
        std::fs::write(d.join("wt/.git"), format!("gitdir: {}\n", d.join("repo/.git").display())).unwrap();
        assert_eq!(head_label(&d.join("wt")), Some(("a8cf233".to_string(), true)));
        assert_eq!(head_label(&d.join("none")), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn metadata_never_follows_a_link() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("sill-front-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file"), "abc").unwrap();
        symlink("file", dir.join("link")).unwrap();
        let m = metadata(&dir.join("link")).unwrap();
        assert!(m.symlink);
        assert!(!m.file);
        assert_eq!(m.target, Some("file".into()));
        assert!(metadata(&dir.join("file")).unwrap().file);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// What differs from HEAD in a repository, or in one file of it.
pub(super) struct GitSummary {
    pub added: u64,
    pub removed: u64,
    pub untracked: u64,
    /// When asked for: each changed file's lines added and removed, in the order `repos.status`
    /// lists them — none for a binary file or a new one that is too large to count.
    pub files: Option<Vec<(std::path::PathBuf, Option<(u64, u64)>)>>,
    /// More files changed than `files` holds.
    pub truncated: bool,
}

/// The lines of this many changed files are given: what a list shows without being asked twice.
pub(super) const COUNTED_FILES: usize = 150;
/// A new file larger than this gets no line count.
const NEW_FILE_MAX: u64 = 1_000_000;

/// The totals and the per-file lines come from one `diff --numstat`, so a list of files and the
/// sum above it cannot disagree. New files are read for their lines only when `files` is asked.
pub(super) fn git_summary(path: &std::path::Path, files: bool) -> Option<GitSummary> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let (repo, spec): (std::path::PathBuf, Option<std::path::PathBuf>) = if path.is_dir() {
        (path.to_path_buf(), None)
    } else {
        let root = path.ancestors().skip(1).find(|a| a.join(".git").exists())?;
        (root.to_path_buf(), Some(path.strip_prefix(root).ok()?.to_path_buf()))
    };
    if !repo.join(".git").exists() {
        return None;
    }
    let git = sill_core::changes::Git::open(&repo).ok()?;
    let mut cmd = git.cmd();
    cmd.args(["diff", "--numstat", "-z"]).args(sill_core::changes::Git::DIFF_SAFE).args(["HEAD", "--"]);
    if let Some(s) = &spec {
        cmd.arg(s);
    }
    let tracked = numstat(&cmd.output().ok()?.stdout);
    let (mut added, mut removed) = (0u64, 0u64);
    for (a, r) in tracked.iter().filter_map(|(_, lines)| *lines) {
        added += a;
        removed += r;
    }
    let mut cmd = git.cmd();
    cmd.args(["ls-files", "--others", "--exclude-standard", "-z", "--"]);
    if let Some(s) = &spec {
        cmd.arg(s);
    }
    let out = cmd.output().ok()?;
    let new: Vec<&[u8]> = out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).collect();
    let untracked = new.len() as u64;
    let mut truncated = false;
    let files = files.then(|| {
        // tracked rows carry their counts; a new file is counted only once it is within the cap
        let mut rows: Vec<(std::path::PathBuf, Option<Option<(u64, u64)>>)> = tracked
            .into_iter()
            .map(|(p, lines)| (repo.join(p), Some(lines)))
            .chain(new.iter().map(|p| (repo.join(std::ffi::OsStr::from_bytes(p)), None)))
            .collect();
        rows.sort_by(|a, b| a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));
        truncated = rows.len() > COUNTED_FILES;
        rows.truncate(COUNTED_FILES);
        rows.into_iter()
            .map(|(p, lines)| {
                let lines = lines.unwrap_or_else(|| new_file_lines(&p));
                (p, lines)
            })
            .collect()
    });
    return Some(GitSummary { added, removed, untracked, files, truncated });

    /// `diff --numstat -z`: `added⇥removed⇥path␀`, or for a rename `added⇥removed⇥␀old␀new␀`
    /// (the row is the new path's); `-` for the counts of a binary file.
    fn numstat(out: &[u8]) -> Vec<(std::path::PathBuf, Option<(u64, u64)>)> {
        let mut rows = Vec::new();
        let mut records = out.split(|b| *b == 0);
        while let Some(record) = records.next() {
            let mut parts = record.splitn(3, |b| *b == b'\t');
            let (Some(a), Some(r), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let path = if path.is_empty() {
                records.next();
                let Some(new) = records.next() else { break };
                new
            } else {
                path
            };
            let n = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u64>().ok();
            rows.push((std::ffi::OsString::from_vec(path.to_vec()).into(), n(a).zip(n(r))));
        }
        rows
    }
}

/// A new file's lines, all of them added; none when it is not a regular file, is larger than
/// [`NEW_FILE_MAX`], or holds a NUL in its first bytes (binary, as git judges it).
fn new_file_lines(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > NEW_FILE_MAX {
        return None;
    }
    let mut bytes = Vec::new();
    // bounded even when the file grew since it was measured
    std::fs::File::open(path).ok()?.take(NEW_FILE_MAX + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > NEW_FILE_MAX || bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    let lines = bytes.iter().filter(|b| **b == b'\n').count() as u64
        + u64::from(bytes.last().is_some_and(|b| *b != b'\n'));
    Some((lines, 0))
}
