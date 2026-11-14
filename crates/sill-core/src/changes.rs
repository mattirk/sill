//! Per-session change list on the host (SPEC §4.4): rebuild the session's jail plan to know its
//! layers, scan the ledger, group by repository, and auto-keep what the policy allows at turn end.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::jail::{self, JailSpec};
use crate::keep::{KeepOptions, keep};
use crate::ledger::{self, Change, ScanOptions};
use crate::paths::expand_pattern;
use crate::policy::Policy;

/// Paths inside `.git` that are never auto-kept: a planted hook or filter would run on the host
/// (SPEC §4.4). They stay in the ledger for manual keep.
pub fn never_auto_keep(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("/.git/hooks/") || s.ends_with("/.git/config") || s.contains("/.git/info/")
}

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
        {
            let mut stdin = child.stdin.take().unwrap();
            for &i in &idxs {
                if let Ok(rel) = changes[i].path.strip_prefix(&repo) {
                    let _ = stdin.write_all(rel.as_os_str().as_encoded_bytes());
                    let _ = stdin.write_all(b"\0");
                }
            }
        }
        let Ok(out) = child.wait_with_output() else { continue };
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

/// Keep every change under one of the policy's `auto_keep` roots that is safe to keep without review.
/// Returns (kept, skipped-with-reason).
pub fn auto_keep(policy: &Policy, home: &Path, changes: &[Change]) -> (Vec<PathBuf>, Vec<(PathBuf, String)>) {
    let roots: Vec<PathBuf> = policy.paths.auto_keep.iter().flat_map(|p| expand_pattern(p, home)).collect();
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for c in changes {
        if !roots.iter().any(|r| c.path.starts_with(r)) {
            continue;
        }
        if c.ignored {
            continue;
        }
        if c.conflict {
            skipped.push((c.path.clone(), "conflict: changed on the host too".into()));
            continue;
        }
        if never_auto_keep(&c.path) {
            skipped.push((c.path.clone(), "git config/hooks: manual keep only".into()));
            continue;
        }
        match keep(c, &KeepOptions::default()) {
            Ok(_) => kept.push(c.path.clone()),
            Err(e) => skipped.push((c.path.clone(), format!("{e:#}"))),
        }
    }
    (kept, skipped)
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
    use similar::TextDiff;
    let host = if c.path.is_file() { std::fs::read(&c.path).unwrap_or_default() } else { Vec::new() };
    let upper = if c.kind != ledger::Kind::Deleted && c.upper.is_file() { std::fs::read(&c.upper).unwrap_or_default() } else { Vec::new() };
    if host.iter().any(|b| *b == 0) || upper.iter().any(|b| *b == 0) {
        return Ok(format!("binary file ({} → {} bytes)", host.len(), upper.len()));
    }
    let a = String::from_utf8_lossy(&host);
    let b = String::from_utf8_lossy(&upper);
    let diff = TextDiff::from_lines(a.as_ref(), b.as_ref());
    let name = c.path.display().to_string();
    Ok(diff.unified_diff().context_radius(3).header(&format!("host:{name}"), &format!("jail:{name}")).to_string())
}

pub fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME not set")?))
}
