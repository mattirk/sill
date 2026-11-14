//! Per-session change list on the host (SPEC §4.4): rebuild the session's jail plan to know its
//! layers, scan the ledger, group by repository. Nothing reaches the host without a keep: pinned
//! folders are bound live and never appear here (SPEC §4.4, decision 18).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::jail::{self, JailSpec};
use crate::ledger::{self, Change, ScanOptions};
use crate::policy::Policy;

/// The jail plan of a session as the launcher built it (same inputs: policy, state dir, host mounts).
pub fn session_plan(
    policy: Policy,
    state_dir: PathBuf,
    home: &Path,
    uid: u32,
    user: &str,
    binds: Vec<(PathBuf, PathBuf)>,
) -> Result<jail::JailPlan> {
    let spec = JailSpec {
        policy,
        home: home.to_path_buf(),
        user: user.to_string(),
        uid,
        state_dir,
        unsafe_state: true, // reading only
        cwd: home.to_path_buf(),
        env: Default::default(),
        share_net: false,
        mounts: crate::mounts::host_mounts()?,
        root_entries: jail::read_root_entries()?,
        proxy_socket: None,
        proxy_port: jail::PROXY_PORT,
        binds,
        extra_args: vec![],
        command: vec!["true".into()],
    };
    spec.plan()
}

/// Scan a session's upper trees. Inside git repositories, git-ignored paths count as ignored (build
/// output) exactly as the repository defines them; the policy's `ledger_ignore` patterns apply
/// everywhere else.
pub fn scan(policy: &Policy, plan: &jail::JailPlan, session_start: Option<std::time::SystemTime>) -> Result<Vec<Change>> {
    let mut skip = plan.masked.clone();
    skip.extend(plan.direct.iter().cloned());
    let opts = ScanOptions { session_start, ignore: policy.paths.ledger_ignore.clone(), skip };
    let mut changes = ledger::scan(&plan.layers, &opts)?;
    apply_gitignore(&mut changes);
    Ok(changes)
}

/// Mark changes that their repository's `.gitignore` rules ignore (`git check-ignore`, one call per
/// repo). Paths already ignored by pattern stay ignored.
pub fn apply_gitignore(changes: &mut [Change]) {
    use std::io::Write;
    let mut by_repo: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (i, c) in changes.iter().enumerate() {
        if c.ignored {
            continue; // the pattern list already decided; build output needn't go through git
        }
        if let Some(r) = repo_root(&c.path) {
            by_repo.entry(r).or_default().push(i);
        }
    }
    for (repo, idxs) in by_repo {
        let mut child = match std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["check-ignore", "--stdin", "-z", "--no-index"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        // feed stdin from its own thread: past 64 KiB of matches git blocks on stdout, and a writer
        // that hasn't started reading yet would then block on stdin forever
        let mut input = Vec::new();
        for &i in &idxs {
            if let Ok(rel) = changes[i].path.strip_prefix(&repo) {
                input.extend_from_slice(rel.as_os_str().as_encoded_bytes());
                input.push(0);
            }
        }
        let mut stdin = child.stdin.take().unwrap();
        let feeder = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let Ok(out) = child.wait_with_output() else { continue };
        let _ = feeder.join();
        let ignored: std::collections::HashSet<Vec<u8>> =
            out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| s.to_vec()).collect();
        for &i in &idxs {
            if let Ok(rel) = changes[i].path.strip_prefix(&repo)
                && ignored.contains(rel.as_os_str().as_encoded_bytes())
            {
                changes[i].ignored = true;
            }
        }
    }
}

/// Is this path inside one of the agent's own state locations (`~/.claude`, `~/.claude.json`, …)?
pub fn is_agent_state(path: &Path, state_dirs: &[String], home: &Path) -> bool {
    state_dirs.iter().any(|d| {
        let p = crate::paths::expand_home(d, home);
        path == p || path.starts_with(&p)
    })
}

/// Group changes by the nearest enclosing git repository ("" = outside any repo).
pub fn group_by_repo(changes: &[Change]) -> BTreeMap<PathBuf, Vec<Change>> {
    let mut out: BTreeMap<PathBuf, Vec<Change>> = BTreeMap::new();
    for c in changes {
        out.entry(repo_root(&c.path).unwrap_or_default()).or_default().push(c.clone());
    }
    out
}

/// Nearest ancestor containing `.git` (on the host).
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let mut p = path.parent()?;
    loop {
        if p.join(".git").exists() {
            return Some(p.to_path_buf());
        }
        p = p.parent()?;
    }
}

/// `git status --porcelain` of a repo: (relative path, one-letter status `M`/`A`/`D`/`R`/`?`).
pub fn git_status(repo: &Path) -> Vec<(PathBuf, String)> {
    let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain", "-z", "--untracked-files=all"])
        .output()
    else {
        return vec![];
    };
    let mut v = Vec::new();
    let mut it = out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty());
    while let Some(entry) = it.next() {
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry[0] as char, entry[1] as char);
        let rel = String::from_utf8_lossy(&entry[3..]).into_owned();
        if x == 'R' || x == 'C' {
            let _ = it.next();
        }
        let st = if x == '?' { "?".to_string() } else if y != ' ' { y.to_string() } else { x.to_string() };
        v.push((PathBuf::from(rel), st));
    }
    v
}

/// Unified diff of a file in a live repo (working tree vs HEAD; whole file for untracked).
pub fn git_diff(repo: &Path, rel: &Path) -> String {
    let out = std::process::Command::new("git").arg("-C").arg(repo).args(["diff", "HEAD", "--"]).arg(rel).output();
    match out {
        Ok(o) if !o.stdout.is_empty() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => std::fs::read_to_string(repo.join(rel)).map(|t| format!("(untracked)\n{t}")).unwrap_or_default(),
    }
}

/// The lines of a tracked file that differ from HEAD, as 1-based `[start, end)` ranges in the
/// working-tree file (`git diff -U0`; a pure deletion has no lines to mark and is left out).
/// `None` for an untracked file (everything is new) or when git can't say.
pub fn git_changed_lines(repo: &Path, rel: &Path) -> Option<Vec<(usize, usize)>> {
    let tracked = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(rel)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !tracked {
        return None;
    }
    let out = std::process::Command::new("git").arg("-C").arg(repo).args(["diff", "-U0", "HEAD", "--"]).arg(rel).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_hunk_ranges(&String::from_utf8_lossy(&out.stdout)))
}

/// `@@ -a[,b] +c[,d] @@` → `(c, c + d)` for every hunk with lines on the new side.
fn parse_hunk_ranges(diff: &str) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    for line in diff.lines().filter(|l| l.starts_with("@@")) {
        let Some(plus) = line.split_whitespace().find(|w| w.starts_with('+')) else { continue };
        let mut it = plus[1..].split(',');
        let start: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let count: usize = it.next().map(|s| s.parse().unwrap_or(0)).unwrap_or(1);
        if start > 0 && count > 0 {
            v.push((start, start + count));
        }
    }
    v
}

/// Revert a file in a live repo to HEAD (or delete it when untracked).
pub fn git_revert(repo: &Path, rel: &Path) -> Result<&'static str> {
    let tracked = std::process::Command::new("git").arg("-C").arg(repo).args(["ls-files", "--error-unmatch", "--"]).arg(rel)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()?.success();
    if tracked {
        let st = std::process::Command::new("git").arg("-C").arg(repo).args(["checkout", "HEAD", "--"]).arg(rel).status()?;
        if !st.success() {
            anyhow::bail!("git checkout failed");
        }
        Ok("restored from HEAD")
    } else {
        std::fs::remove_file(repo.join(rel))?;
        Ok("untracked file removed")
    }
}

/// Unified diff text for a change (host vs upper).
pub fn unified_diff(c: &Change) -> Result<String> {
    let host = if c.path.is_file() { std::fs::read(&c.path).unwrap_or_default() } else { Vec::new() };
    let upper = if c.kind != ledger::Kind::Deleted && c.upper.is_file() { std::fs::read(&c.upper).unwrap_or_default() } else { Vec::new() };
    let name = c.path.display().to_string();
    Ok(text_diff(&host, &upper, &format!("host:{name}"), &format!("jail:{name}")))
}

/// What undoing a keep would change: the host's current content against the saved previous version
/// (`keep::prior_path`).
pub fn undo_diff(host: &Path, prior: &Path) -> String {
    let now = if host.is_file() { std::fs::read(host).unwrap_or_default() } else { Vec::new() };
    let before = if prior.is_file() { std::fs::read(prior).unwrap_or_default() } else { Vec::new() };
    let name = host.display().to_string();
    text_diff(&now, &before, &format!("host:{name}"), &format!("undo:{name}"))
}

/// A unified diff of two texts (3 lines of context), or a note for binary content.
pub fn text_diff(from: &[u8], to: &[u8], from_label: &str, to_label: &str) -> String {
    use similar::TextDiff;
    if from.contains(&0) || to.contains(&0) {
        return format!("binary file ({} → {} bytes)", from.len(), to.len());
    }
    let a = String::from_utf8_lossy(from);
    let b = String::from_utf8_lossy(to);
    TextDiff::from_lines(a.as_ref(), b.as_ref()).unified_diff().context_radius(3).header(from_label, to_label).to_string()
}

pub fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME not set")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{FileType, Kind};

    #[test]
    fn hunk_ranges_from_a_diff() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -3 +3 @@\n-old\n+new\n@@ -5,0 +6,2 @@\n+a\n+b\n@@ -9,2 +11,0 @@\n-gone\n-gone\n";
        assert_eq!(parse_hunk_ranges(diff), vec![(3, 4), (6, 8)]); // the pure deletion has no lines to mark
        assert!(parse_hunk_ranges("").is_empty());
    }

    /// Thousands of matches overflow the pipe to git; the call must still return.
    #[test]
    fn check_ignore_survives_many_matches() {
        let dir = std::env::temp_dir().join(format!("sill-ci-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ok = std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            return; // no git here
        }
        std::fs::write(dir.join(".gitignore"), "big/\n").unwrap();
        let mut changes: Vec<Change> = (0..5000)
            .map(|i| Change {
                path: dir.join(format!("big/some/deeper/directory/with/a/long/name/file-{i:05}.o")),
                upper: PathBuf::new(),
                kind: Kind::Added,
                file_type: FileType::File,
                size: 0,
                conflict: false,
                ignored: false,
            })
            .collect();
        apply_gitignore(&mut changes);
        assert!(changes.iter().all(|c| c.ignored));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
