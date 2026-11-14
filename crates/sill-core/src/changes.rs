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
pub fn session_plan(policy: Policy, state_dir: PathBuf, home: &Path, uid: u32, user: &str) -> Result<jail::JailPlan> {
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
        extra_args: vec![],
        command: vec!["true".into()],
    };
    spec.plan()
}

/// Scan a session's upper trees.
pub fn scan(policy: &Policy, plan: &jail::JailPlan, session_start: Option<std::time::SystemTime>) -> Result<Vec<Change>> {
    let mut skip = plan.masked.clone();
    skip.extend(plan.direct.iter().cloned());
    let opts = ScanOptions { session_start, ignore: policy.paths.ledger_ignore.clone(), skip };
    ledger::scan(&plan.layers, &opts)
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
