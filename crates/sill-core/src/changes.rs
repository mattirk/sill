//! Per-session change list on the host (SPEC §4.4): rebuild the session's jail plan to know its
//! layers, scan the ledger, group by repository. Nothing reaches the host without a keep: pinned
//! folders are bound live and never appear here (SPEC §4.4, decision 18).

/// Environment for a shell the person opens on the host (the daemon's host shell, a terminal
/// window): git's per-repository config in a pinned folder is agent-writable, and a git-aware
/// prompt runs `git status` there at the first prompt — `core.fsmonitor` names a program that
/// `git status` runs, so it is off for every repository (docs/security.md S8). Hooks, filters and
/// pagers stay as configured: those run only for git commands the person types on purpose.
pub const HOST_SHELL_GIT_ENV: (&str, &str) = ("GIT_CONFIG_PARAMETERS", "'core.fsmonitor=false' 'core.useBuiltinFSMonitor=false'");

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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
    filesystem: &crate::agent_storage::Plan,
) -> Result<jail::JailPlan> {
    let mut policy = policy;
    policy.paths.mask.clear();
    policy.paths.direct.clear();
    policy.paths.ro.clear();
    let spec = JailSpec {
        fragment_ro: vec![],
        fragment_masks: vec![],
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
        binds: vec![],
        agent_masks: vec![],
        policy_paths: None,
        agent_binds: vec![],
        masks: vec![], // the other agents' paths are tmpfs in the jail: nothing of theirs is in an upper tree
        memory_dir: None,
        bridge_dir: None,
        seccomp_fd: None,
        extra_args: vec![],
        command: vec!["true".into()],
    };
    let mut plan = spec.plan()?;
    plan.masked = filesystem.policy_paths.masks.iter().chain(&filesystem.agent_masks).map(|m| m.path.to_path()).collect::<Result<_>>()?;
    plan.direct = filesystem.bindings.iter().map(|b| b.target.to_path()).chain(filesystem.policy_paths.direct.iter().chain(&filesystem.policy_paths.read_only).map(|p| p.to_path())).collect::<Result<_>>()?;
    Ok(plan)
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

/// A ghost session's hour (docs/memory.md §3.2): of `changes`, those whose last write (the upper
/// entry's mtime) is at least `after` before `now` — repositories, ignored build output and
/// conflicts included, since everything of a ghost reverts. A due directory with a change below it
/// that is not due yet waits for it: reverting the directory would take the newer write too. The
/// deepest paths come first, so a directory goes after its contents.
pub fn ghost_due(changes: &[Change], now: std::time::SystemTime, after: std::time::Duration) -> Vec<Change> {
    let written = |c: &Change| std::fs::symlink_metadata(&c.upper).ok().and_then(|m| m.modified().ok());
    let is_due = |c: &Change| written(c).is_some_and(|t| now.duration_since(t).unwrap_or_default() >= after);
    let fresh: Vec<&Path> = changes.iter().filter(|c| !is_due(c)).map(|c| c.path.as_path()).collect();
    let mut due: Vec<Change> = changes
        .iter()
        .filter(|c| is_due(c))
        .filter(|c| !fresh.iter().any(|f| *f != c.path && f.starts_with(&c.path)))
        .cloned()
        .collect();
    due.sort_by_key(|c| std::cmp::Reverse(c.path.components().count()));
    due
}

static GIT_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Git on a repository the agent can write — a pin is live, and a kept overlay copy is the agent's
/// too. Its `.git/config` (hooks path, fsmonitor, filters, diff drivers, promisor remotes…) can make
/// git run anything, so git runs here with `--git-dir` at a private directory of sill's: links to
/// the real objects, refs, HEAD, index and packed-refs, and a config that keeps only the
/// repository's format (version, object format, ref storage) — nothing that names a program.
/// Hooks and fsmonitor are switched off on the command line as well, diffs never call out
/// ([`Git::DIFF_SAFE`]), and nothing is written back (`--no-optional-locks`). Every host-side git
/// goes through here (security.md S1, SPEC §4.11 #3).
pub struct Git {
    work_tree: PathBuf,
    dir: PathBuf,
}

impl Git {
    pub fn open(repo: &Path) -> Result<Git> {
        let dot = repo.join(".git");
        // a linked worktree's `.git` is a file naming its directory
        let gitdir = if dot.is_dir() {
            dot
        } else {
            let text = std::fs::read_to_string(&dot).with_context(|| format!("{}: no .git", repo.display()))?;
            let p = text.trim().strip_prefix("gitdir:").map(str::trim).with_context(|| format!("{}: malformed .git file", repo.display()))?;
            let p = Path::new(p);
            if p.is_absolute() { p.to_path_buf() } else { repo.join(p) }
        };
        // a linked worktree keeps objects and refs in the main repository
        let common = match std::fs::read_to_string(gitdir.join("commondir")) {
            Ok(c) => {
                let p = PathBuf::from(c.trim());
                if p.is_absolute() { p } else { gitdir.join(p) }
            }
            Err(_) => gitdir.clone(),
        };
        let n = GIT_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sill-git-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        let view = Git { work_tree: repo.to_path_buf(), dir };
        for (base, name) in [(&common, "objects"), (&common, "refs"), (&common, "packed-refs"), (&common, "info"), (&common, "shallow"), (&gitdir, "index")] {
            let src = base.join(name);
            if src.exists() {
                std::os::unix::fs::symlink(&src, view.dir.join(name))?;
            }
        }
        // HEAD is copied, not linked: git takes a symlinked HEAD for the ancient `HEAD -> refs/…`
        // form and refuses any other target
        std::fs::copy(gitdir.join("HEAD"), view.dir.join("HEAD")).with_context(|| format!("{}: no HEAD", repo.display()))?;
        let original = std::fs::read_to_string(common.join("config")).unwrap_or_default();
        std::fs::write(view.dir.join("config"), format_config(&original))?;
        Ok(view)
    }

    /// A `git` on this view, the subcommand and its arguments still to add.
    pub fn cmd(&self) -> std::process::Command {
        let mut c = std::process::Command::new("git");
        c.arg("--git-dir").arg(&self.dir).arg("--work-tree").arg(&self.work_tree).arg("--no-optional-locks");
        for kv in ["core.fsmonitor=false", "core.hooksPath=/dev/null", "diff.external=", "submodule.recurse=false", "core.pager=cat", "protocol.allow=never"] {
            c.arg("-c").arg(kv);
        }
        c.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .current_dir(&self.work_tree);
        c
    }

    /// `diff` arguments that keep git from calling out: no external diff, no textconv, no submodules.
    pub const DIFF_SAFE: &'static [&'static str] = &["--no-ext-diff", "--no-textconv", "--ignore-submodules=all"];
}

impl Drop for Git {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The repository's format from its config, and nothing else: the keys below, with values that are
/// one word (`1`, `true`, `sha256`, `reftable`). Everything that could name a program is left out.
fn format_config(original: &str) -> String {
    const KEEP: &[(&str, &str)] = &[
        ("core", "repositoryformatversion"),
        ("core", "filemode"),
        ("core", "ignorecase"),
        ("core", "symlinks"),
        ("core", "precomposeunicode"),
        ("core", "sparsecheckout"),
        ("core", "sparsecheckoutcone"),
        ("extensions", "objectformat"),
        ("extensions", "compatobjectformat"),
        ("extensions", "refstorage"),
    ];
    let mut section = String::new();
    let mut kept: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for raw in original.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix('[') {
            section = rest.split([']', ' ', '"']).next().unwrap_or("").to_ascii_lowercase();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if let Some((s, key)) = KEEP.iter().find(|(s, key)| *s == section && *key == k)
            && !v.is_empty()
            && v.chars().all(|c| c.is_ascii_alphanumeric())
        {
            kept.entry(s).or_default().push(format!("\t{key} = {v}\n"));
        }
    }
    let mut out = String::from("[core]\n\tbare = false\n");
    let core = kept.remove("core").unwrap_or_default();
    if !core.iter().any(|l| l.contains("repositoryformatversion")) {
        out.push_str("\trepositoryformatversion = 0\n");
    }
    out.extend(core);
    if let Some(ext) = kept.remove("extensions") {
        out.push_str("[extensions]\n");
        out.extend(ext);
    }
    out
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
        let Ok(git) = Git::open(&repo) else { return };
        let mut child = match git
            .cmd()
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
pub fn is_agent_state(path: &Path, state_paths: &[PathBuf]) -> bool {
    state_paths.iter().any(|p| path.starts_with(p))
}

/// Group changes by the nearest enclosing git repository ("" = outside any repo).
pub fn group_by_repo(changes: &[Change]) -> BTreeMap<PathBuf, Vec<Change>> {
    let mut out: BTreeMap<PathBuf, Vec<Change>> = BTreeMap::new();
    for c in changes {
        out.entry(repo_root(&c.path).unwrap_or_default()).or_default().push(c.clone());
    }
    out
}

/// The repository `path` is in: the nearest directory containing `.git`, `path` itself included
/// when it is a directory (a file, or a path that is gone, is looked up from its parent).
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let mut p = if path.is_dir() { path } else { path.parent()? };
    loop {
        if p.join(".git").exists() {
            return Some(p.to_path_buf());
        }
        p = p.parent()?;
    }
}

/// `git status --porcelain` of a repo: (relative path, one-letter status `M`/`A`/`D`/`R`/`?`).
pub fn git_status(repo: &Path) -> Vec<(PathBuf, String)> {
    let Ok(git) = Git::open(repo) else { return vec![] };
    let Ok(out) = git.cmd().args(["status", "--porcelain", "-z", "--untracked-files=all", "--ignore-submodules=all"]).output() else {
        return vec![];
    };
    let mut v = Vec::new();
    let mut it = out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty());
    while let Some(entry) = it.next() {
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry[0] as char, entry[1] as char);
        let rel = <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(entry[3..].to_vec());
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
    let out = Git::open(repo).and_then(|git| Ok(git.cmd().arg("diff").args(Git::DIFF_SAFE).args(["HEAD", "--"]).arg(rel).output()?));
    match out {
        Ok(o) if !o.stdout.is_empty() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => std::fs::read_to_string(repo.join(rel)).map(|t| format!("(untracked)\n{t}")).unwrap_or_default(),
    }
}

/// Programming languages by file extension: (extension, name, the short mark a badge shows, the
/// colour of the mark — the usual one for the language, lightened where it would sink into a dark
/// background).
const LANGUAGES: &[(&str, &str, &str, &str)] = &[
    ("rs", "Rust", "Rs", "#dea584"),
    ("py", "Python", "Py", "#5b9bd5"),
    ("js", "JavaScript", "JS", "#e8d44d"),
    ("jsx", "JavaScript", "JS", "#e8d44d"),
    ("mjs", "JavaScript", "JS", "#e8d44d"),
    ("ts", "TypeScript", "TS", "#4a8fd6"),
    ("tsx", "TypeScript", "TS", "#4a8fd6"),
    ("go", "Go", "Go", "#00add8"),
    ("c", "C", "C", "#9a9a9a"),
    ("h", "C", "C", "#9a9a9a"),
    ("cpp", "C++", "C++", "#f34b7d"),
    ("cc", "C++", "C++", "#f34b7d"),
    ("cxx", "C++", "C++", "#f34b7d"),
    ("hpp", "C++", "C++", "#f34b7d"),
    ("java", "Java", "Java", "#c98a3a"),
    ("kt", "Kotlin", "Kt", "#a97bff"),
    ("kts", "Kotlin", "Kt", "#a97bff"),
    ("rb", "Ruby", "Rb", "#d9534f"),
    ("sh", "Shell", "Sh", "#89e051"),
    ("bash", "Shell", "Sh", "#89e051"),
    ("zsh", "Shell", "Sh", "#89e051"),
    ("swift", "Swift", "Sw", "#f05138"),
    ("cs", "C#", "C#", "#5cb85c"),
    ("php", "PHP", "PHP", "#7a88cf"),
    ("lua", "Lua", "Lua", "#5a6fd6"),
    ("dart", "Dart", "Dart", "#00b4ab"),
    ("ex", "Elixir", "Ex", "#a074c4"),
    ("exs", "Elixir", "Ex", "#a074c4"),
    ("erl", "Erlang", "Erl", "#b83998"),
    ("hs", "Haskell", "Hs", "#8f7bc7"),
    ("scala", "Scala", "Sc", "#c22d40"),
    ("zig", "Zig", "Zig", "#ec915c"),
    ("nim", "Nim", "Nim", "#ffc200"),
    ("ml", "OCaml", "ML", "#ef7a08"),
    ("clj", "Clojure", "Clj", "#db5855"),
    ("r", "R", "R", "#198ce7"),
    ("jl", "Julia", "Jl", "#a270ba"),
    ("m", "Objective-C", "ObjC", "#438eff"),
    ("vue", "Vue", "Vue", "#41b883"),
    ("svelte", "Svelte", "Sv", "#ff3e00"),
    ("html", "HTML", "HTML", "#e34c26"),
    ("css", "CSS", "CSS", "#9b6bd6"),
    ("scss", "CSS", "CSS", "#9b6bd6"),
    ("sql", "SQL", "SQL", "#e38c00"),
    ("pl", "Perl", "Pl", "#39457e"),
    ("cu", "CUDA", "CU", "#8f8f8f"),
    ("glsl", "GLSL", "GL", "#5686a5"),
    ("wgsl", "WGSL", "WG", "#8f8f8f"),
];

/// A repository's main language and its share of the tracked code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Language {
    pub name: &'static str,
    pub mark: &'static str,
    /// `#rrggbb`
    pub color: &'static str,
    pub percent: u8,
}

/// The language behind a badge mark (`Rs` → `Rust`).
pub fn language_name_of_mark(mark: &str) -> Option<&'static str> {
    LANGUAGES.iter().find(|(_, _, m, _)| *m == mark).map(|(_, name, _, _)| *name)
}

/// The language a file is written in, by its extension: (name, mark, colour).
pub fn file_language(path: &Path) -> Option<(&'static str, &'static str, &'static str)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGES.iter().find(|(e, _, _, _)| *e == ext).map(|(_, name, mark, color)| (*name, *mark, *color))
}

/// The language most of a repository is written in, by the size of its tracked files per extension
/// (`git ls-files` with the index's object sizes: nothing is read, ignored and untracked files
/// don't count). `None` without any code, or with a git too old for `--format`.
pub fn repo_language(repo: &Path) -> Option<Language> {
    let out = Git::open(repo).ok()?.cmd().args(["ls-files", "-z", "--format=%(objectsize)%x09%(path)"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let mut by_name: BTreeMap<&'static str, u64> = BTreeMap::new();
    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((size, path)) = entry.split_once('\t') else { continue };
        let Ok(size) = size.trim().parse::<u64>() else { continue };
        let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) else { continue };
        let ext = ext.to_ascii_lowercase();
        if let Some((_, name, _, _)) = LANGUAGES.iter().find(|(e, _, _, _)| *e == ext) {
            *by_name.entry(name).or_default() += size;
        }
    }
    let total: u64 = by_name.values().sum();
    let (name, size) = by_name.into_iter().max_by_key(|(_, s)| *s)?;
    if total == 0 {
        return None;
    }
    let (_, _, mark, color) = LANGUAGES.iter().find(|(_, n, _, _)| *n == name)?;
    Some(Language { name, mark, color, percent: (size * 100 / total) as u8 })
}

/// The lines of a tracked file that differ from HEAD, as 1-based `[start, end)` ranges in the
/// working-tree file (`git diff -U0`; a pure deletion has no lines to mark and is left out).
/// `None` for an untracked file (everything is new) or when git can't say.
pub fn git_changed_lines(repo: &Path, rel: &Path) -> Option<Vec<(usize, usize)>> {
    let git = Git::open(repo).ok()?;
    let tracked = git
        .cmd()
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
    let out = git.cmd().args(["diff", "-U0"]).args(Git::DIFF_SAFE).args(["HEAD", "--"]).arg(rel).output().ok()?;
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

/// Revert a file in a live repo to HEAD (or delete it when untracked). Git checks nothing out
/// (a checkout runs smudge filters and hooks): the blob is read with `cat-file` and written below
/// the repository without following links.
pub fn git_revert(repo: &Path, rel: &Path) -> Result<&'static str> {
    let git = Git::open(repo)?;
    let out = git.cmd().args(["ls-tree", "-z", "HEAD", "--"]).arg(rel).output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let entry = text.trim_end_matches('\0');
    if entry.is_empty() {
        crate::safe_fs::remove_file_beneath(repo, rel)?;
        return Ok("untracked file removed");
    }
    // `<mode> blob <sha>\t<path>`
    let mut fields = entry.split('\t').next().unwrap_or("").split_whitespace();
    let (mode, kind, sha) = (fields.next().unwrap_or(""), fields.next().unwrap_or(""), fields.next().unwrap_or(""));
    if kind != "blob" || mode == "120000" {
        bail!("{}: not a regular file in HEAD", rel.display());
    }
    let blob = git.cmd().args(["cat-file", "blob", sha]).output()?;
    if !blob.status.success() {
        bail!("git cat-file {sha} failed");
    }
    crate::safe_fs::write_file_beneath(repo, rel, &blob.stdout, if mode == "100755" { 0o755 } else { 0o644 })?;
    Ok("restored from HEAD")
}

/// Larger files are not diffed: reading and diffing them would hold the daemon (SPEC §4.11 #13).
pub const DIFF_MAX: u64 = 4 << 20;

/// One side of a diff: the bytes of a regular file, or a line saying why there are none.
enum Side {
    Text(Vec<u8>),
    Note(String),
}

/// Read `p` for a diff by what it is, never through a link: the upper tree is the agent's, and a
/// symlink there planted at a changed path would otherwise read any host file into the diff pane
/// (SPEC §4.11 #10). Absent = empty.
fn diff_side(p: &Path) -> Side {
    use std::os::unix::fs::OpenOptionsExt;
    let md = match p.symlink_metadata() {
        Ok(md) => md,
        Err(_) => return Side::Text(Vec::new()),
    };
    let ft = md.file_type();
    if ft.is_symlink() {
        return Side::Note(format!("symlink → {}", std::fs::read_link(p).map(|t| t.display().to_string()).unwrap_or_default()));
    }
    if ft.is_dir() {
        return Side::Note("directory".into());
    }
    if !ft.is_file() {
        return Side::Note("special file".into());
    }
    if md.len() > DIFF_MAX {
        return Side::Note(format!("{} MB: too large to diff", md.len() / 1_000_000));
    }
    let mut f = match std::fs::OpenOptions::new().read(true).custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32).open(p) {
        Ok(f) => f,
        Err(e) => return Side::Note(format!("unreadable: {e}")),
    };
    let mut bytes = Vec::with_capacity(md.len() as usize);
    match std::io::Read::read_to_end(&mut f, &mut bytes) {
        Ok(_) => Side::Text(bytes),
        Err(e) => Side::Note(format!("unreadable: {e}")),
    }
}

fn diff_sides(a: Side, b: Side, a_label: &str, b_label: &str) -> String {
    match (a, b) {
        (Side::Text(a), Side::Text(b)) => text_diff(&a, &b, a_label, b_label),
        (a, b) => {
            let say = |s: Side| match s {
                Side::Text(t) if t.is_empty() => "absent".to_string(),
                Side::Text(t) => format!("{} bytes", t.len()),
                Side::Note(n) => n,
            };
            format!("{a_label}: {}\n{b_label}: {}\n", say(a), say(b))
        }
    }
}

/// Unified diff text for a change (host vs upper).
pub fn unified_diff(c: &Change) -> Result<String> {
    let host = diff_side(&c.path);
    let upper = if c.kind == ledger::Kind::Deleted { Side::Text(Vec::new()) } else { diff_side(&c.upper) };
    let name = c.path.display().to_string();
    Ok(diff_sides(host, upper, &format!("host:{name}"), &format!("jail:{name}")))
}

/// What undoing a keep would change: the host's current content against the saved previous version
/// (`keep::prior_path`).
pub fn undo_diff(host: &Path, prior: &Path) -> String {
    let name = host.display().to_string();
    diff_sides(diff_side(host), diff_side(prior), &format!("host:{name}"), &format!("undo:{name}"))
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

    /// A folder that is itself a repository answers for itself; a file, a missing path and a
    /// nested folder answer with the enclosing one; outside any repository, nothing.
    #[test]
    fn repo_root_of_the_repository_folder_is_itself() {
        let dir = std::env::temp_dir().join(format!("sill-repo-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "").unwrap();
        assert_eq!(repo_root(&repo).as_deref(), Some(repo.as_path()));
        assert_eq!(repo_root(&repo.join("src")).as_deref(), Some(repo.as_path()));
        assert_eq!(repo_root(&repo.join("src/main.rs")).as_deref(), Some(repo.as_path()));
        assert_eq!(repo_root(&repo.join("src/gone.rs")).as_deref(), Some(repo.as_path()));
        assert_eq!(repo_root(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ghost hour: due by the upper entry's last write, a directory waits for its newer
    /// contents, deepest first.
    #[test]
    fn ghost_due_by_last_write() {
        use crate::ledger::{FileType, Kind};
        let dir = std::env::temp_dir().join(format!("sill-ghost-due-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("old/sub")).unwrap();
        std::fs::create_dir_all(dir.join("mixed")).unwrap();
        for f in ["old/sub/a", "mixed/stale", "mixed/fresh", "lone"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let now = std::time::SystemTime::now();
        let two_h = now - std::time::Duration::from_secs(7200);
        let set = |p: &str, t: std::time::SystemTime| std::fs::File::open(dir.join(p)).unwrap().set_modified(t).unwrap();
        for p in ["old/sub/a", "old/sub", "old", "mixed/stale", "mixed", "lone"] {
            set(p, two_h);
        }
        let ch = |p: &str, ft: FileType| Change {
            path: PathBuf::from("/home/me").join(p),
            upper: dir.join(p),
            kind: Kind::Added,
            file_type: ft,
            size: 1,
            conflict: p == "lone",
            ignored: false,
        };
        let all = vec![
            ch("old", FileType::Dir),
            ch("old/sub", FileType::Dir),
            ch("old/sub/a", FileType::File),
            ch("mixed", FileType::Dir),
            ch("mixed/stale", FileType::File),
            ch("mixed/fresh", FileType::File),
            ch("lone", FileType::File),
        ];
        let due: Vec<String> = ghost_due(&all, now, std::time::Duration::from_secs(3600))
            .iter()
            .map(|c| c.path.strip_prefix("/home/me").unwrap().display().to_string())
            .collect();
        assert_eq!(due, ["old/sub/a", "old/sub", "mixed/stale", "old", "lone"], "mixed waits for mixed/fresh; a conflict reverts too");
        assert!(ghost_due(&all, now, std::time::Duration::from_secs(3 * 3600)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
    use crate::ledger::{FileType, Kind};

    #[test]
    fn diff_never_reads_through_a_link() {
        let dir = std::env::temp_dir().join(format!("sill-diff-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("secret"), "the key\n").unwrap();
        std::fs::write(dir.join("host.txt"), "old\n").unwrap();
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("upper.txt")).unwrap();
        let c = Change { path: dir.join("host.txt"), upper: dir.join("upper.txt"), kind: Kind::Modified, file_type: FileType::Symlink, size: 0, conflict: false, ignored: false };
        let d = unified_diff(&c).unwrap();
        assert!(d.contains("symlink →") && !d.contains("the key"), "{d}");
        // plain files diff as before
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let c = Change { upper: dir.join("new.txt"), ..c };
        assert!(unified_diff(&c).unwrap().contains("+new"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_status_retains_non_utf8_and_newline_filenames() {
        use std::os::unix::ffi::OsStringExt;
        let dir = std::env::temp_dir().join(format!("sill-git-byte-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status().unwrap().success());
        let rel = PathBuf::from(std::ffi::OsString::from_vec(b"file\xff\nname".to_vec()));
        std::fs::write(dir.join(&rel), "content").unwrap();
        assert_eq!(git_status(&dir), vec![(rel, "?".into())]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The language is the one with most tracked bytes; docs and config don't count.
    #[test]
    fn repo_language_by_tracked_size() {
        let dir = std::env::temp_dir().join(format!("sill-lang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&dir).args(args).status().map(|s| s.success()).unwrap_or(false);
        if !git(&["init", "-q"]) {
            eprintln!("no git: skipped");
            return;
        }
        std::fs::write(dir.join("a.rs"), "x".repeat(900)).unwrap();
        std::fs::write(dir.join("b.py"), "y".repeat(100)).unwrap();
        std::fs::write(dir.join("README.md"), "z".repeat(5000)).unwrap();
        std::fs::write(dir.join("untracked.go"), "g".repeat(9000)).unwrap();
        assert!(git(&["add", "a.rs", "b.py", "README.md"]));
        let l = repo_language(&dir).expect("a language");
        assert_eq!((l.name, l.mark, l.percent), ("Rust", "Rs", 90));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pin's `.git/config` is the agent's: fsmonitor, hooks and filters it names must not run.
    #[test]
    fn git_view_runs_nothing_from_the_repository_config() {
        let dir = std::env::temp_dir().join(format!("sill-gitview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().map(|o| o.status.success()).unwrap_or(false);
        if !git(&["init", "-q"]) {
            eprintln!("no git: skipped");
            return;
        }
        let marker = dir.join("RAN");
        let script = dir.join("evil.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch {}\ncat\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join(".gitattributes"), "*.txt filter=evil diff=evil\n").unwrap();
        assert!(git(&["-c", "user.email=t@t", "-c", "user.name=t", "add", "."]));
        assert!(git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "one"]));
        let s = script.display();
        for kv in [format!("core.fsmonitor={s}"), format!("filter.evil.clean={s}"), format!("filter.evil.smudge={s}"), format!("diff.evil.command={s}"), format!("diff.evil.textconv={s}"), format!("core.hooksPath={}", dir.display())] {
            let (k, v) = kv.split_once('=').unwrap();
            assert!(git(&["config", k, v]));
        }
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let st = git_status(&dir);
        assert!(st.iter().any(|(p, s)| p == Path::new("a.txt") && s == "M"), "{st:?}");
        assert!(git_diff(&dir, Path::new("a.txt")).contains("+two"));
        assert!(git_changed_lines(&dir, Path::new("a.txt")).is_some());
        let _ = repo_language(&dir); // through the view like the rest
        assert_eq!(git_revert(&dir, Path::new("a.txt")).unwrap(), "restored from HEAD");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(!marker.exists(), "the repository's config ran a program");
        // a tracked file turned into a link out of the repository is not followed on revert
        let outside = std::env::temp_dir().join(format!("sill-gitview-out-{}", std::process::id()));
        std::fs::write(&outside, "host").unwrap();
        std::fs::remove_file(dir.join("a.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("a.txt")).unwrap();
        assert!(git_revert(&dir, Path::new("a.txt")).is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "host");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
