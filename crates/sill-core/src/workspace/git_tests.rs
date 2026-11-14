//! Workspaces against real repositories in a temporary directory: refusals, the clone, anchors
//! surviving a hostile source gc, the fast-forward Merge and its stops, retention, and no program
//! from either repository's config running on the host.
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(name: &str) -> Dir {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("sill-ws-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d.canonicalize().unwrap())
}

/// Plain git for setting things up, as a person or an agent would run it.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main", "-c", "gc.auto=0"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "gc.auto=0"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
        .status
        .success()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A source with three commits on `main`.
fn source(d: &Dir) -> PathBuf {
    let s = d.0.join("projects/src");
    std::fs::create_dir_all(&s).unwrap();
    git(&s, &["init", "-q"]);
    for i in 1..=3 {
        write(&s.join(format!("f{i}.txt")), &format!("line {i}\n"));
        git(&s, &["add", "."]);
        git(&s, &["commit", "-qm", &format!("c{i}")]);
    }
    s
}

fn make(d: &Dir, s: &Path, id: &str) -> (Source, Created) {
    let src = inspect(s).unwrap().unwrap();
    let path = path_for(&d.0.join("ws"), &src, id, "Fix the Thing!");
    let created = create(&src, id, "Fix the Thing!", &path, "op-1").unwrap();
    (src, created)
}

/// Agent work in the clone: one commit changing f2 and adding new.txt.
fn agent_commit(ws: &Path) {
    write(&ws.join("f2.txt"), "agent work\n");
    write(&ws.join("new.txt"), "new\n");
    git(ws, &["add", "."]);
    git(ws, &["commit", "-qm", "agent change"]);
}

#[test]
fn refusals_name_their_reason() {
    let d = tmp("refuse");
    let s = source(&d);
    assert!(inspect(&s).unwrap().is_ok());
    assert_eq!(inspect(&d.0.join("nowhere")).unwrap(), Err(Refusal::NotARepository));
    assert_eq!(inspect(&d.0).unwrap(), Err(Refusal::NotARepository));

    let empty = d.0.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    assert_eq!(inspect(&empty).unwrap(), Err(Refusal::NoCommits));

    let linked = d.0.join("linked");
    git(&s, &["worktree", "add", "-q", linked.to_str().unwrap(), "-b", "side"]);
    assert_eq!(inspect(&linked).unwrap(), Err(Refusal::LinkedWorktree));

    git(&s, &["checkout", "-q", "--detach"]);
    assert_eq!(inspect(&s).unwrap(), Err(Refusal::DetachedHead));
    git(&s, &["checkout", "-q", "main"]);

    git(&s, &["config", "extensions.partialclone", "origin"]);
    assert_eq!(inspect(&s).unwrap(), Err(Refusal::PartialClone));
    git(&s, &["config", "--unset", "extensions.partialclone"]);

    write(&s.join(".git/objects/info/alternates"), "/elsewhere/objects\n");
    assert_eq!(inspect(&s).unwrap(), Err(Refusal::BorrowedObjects));
    std::fs::remove_file(s.join(".git/objects/info/alternates")).unwrap();

    write(&s.join(".gitattributes"), "*.bin filter=lfs diff=lfs merge=lfs -text\n");
    git(&s, &["add", ".gitattributes"]);
    git(&s, &["commit", "-qm", "lfs"]);
    assert_eq!(inspect(&s).unwrap(), Err(Refusal::Lfs));

    write(&s.join(".gitattributes"), "*.secret filter=crypt\n");
    git(&s, &["commit", "-qam", "crypt"]);
    // a filter named but defined nowhere is harmless
    assert!(inspect(&s).unwrap().is_ok());
    git(&s, &["config", "filter.crypt.smudge", "cat"]);
    assert_eq!(inspect(&s).unwrap(), Err(Refusal::FilterDrivers));
    git(&s, &["config", "--remove-section", "filter.crypt"]);

    write(&s.join(".gitmodules"), "[submodule \"x\"]\n\tpath = x\n\turl = ./x\n");
    git(&s, &["add", ".gitmodules"]);
    git(&s, &["commit", "-qm", "sub"]);
    assert_eq!(inspect(&s).unwrap(), Err(Refusal::Submodules));
}

#[test]
fn reftable_is_refused_before_git_reads_refs() {
    let d = tmp("reftable");
    let s = d.0.join("r");
    std::fs::create_dir_all(&s).unwrap();
    if !git_ok(&s, &["init", "-q", "--ref-format=reftable"]) {
        return; // this git has no reftable
    }
    assert_eq!(inspect(&s).unwrap(), Err(Refusal::RefFormat));
}

#[test]
fn shallow_and_dirty_sources_are_reported() {
    let d = tmp("shallow");
    let s = source(&d);
    write(&s.join("f1.txt"), "edited\n");
    let src = inspect(&s).unwrap().unwrap();
    assert!(src.dirty && !src.shallow);
    let sh = d.0.join("sh");
    git(&d.0, &["clone", "-q", "--depth=1", &format!("file://{}", s.display()), sh.to_str().unwrap()]);
    let src = inspect(&sh).unwrap().unwrap();
    assert!(src.shallow && !src.dirty);
}

#[test]
fn slugs_and_paths() {
    assert_eq!(slug("Fix the Thing!"), "fix-the-thing");
    assert_eq!(slug("  ÄÖ  "), "work");
    assert_eq!(slug("a/b..c"), "a-b-c");
    assert!(slug(&"x".repeat(100)).len() <= 40);
}

#[test]
fn create_borrows_the_real_store_and_leaves_only_an_anchor() {
    let d = tmp("create");
    let s = source(&d);
    write(&s.join("f1.txt"), "my uncommitted edit\n");
    let before_status = git(&s, &["status", "--porcelain"]);
    let (src, c) = make(&d, &s, "ab12");
    assert!(c.path.ends_with("ws/src/fix-the-thing-ab12"));
    assert_eq!(c.branch, "sill/fix-the-thing-ab12");
    assert_eq!(c.base, src.head);
    // its own git directory, borrowing the source's real object store; no remote back to it
    let alternates = std::fs::read_to_string(c.path.join(".git/objects/info/alternates")).unwrap();
    assert_eq!(alternates.trim(), s.join(".git/objects").display().to_string());
    assert_eq!(git(&c.path, &["remote"]), "");
    assert_eq!(marker(&c.path).as_deref(), Some("op-1"));
    assert_eq!(git(&c.path, &["rev-parse", "HEAD"]), src.head);
    assert_eq!(git(&c.path, &["rev-parse", "refs/sill/base"]), src.head);
    assert_eq!(std::fs::read_to_string(c.path.join("f1.txt")).unwrap(), "line 1\n", "from HEAD only");
    // nothing copied from the source, and no object of its own
    assert!(git(&c.path, &["count-objects"]).starts_with("0 objects"));
    // the source: its edit and status as they were, one anchor more
    assert_eq!(git(&s, &["status", "--porcelain"]), before_status);
    assert_eq!(git(&s, &["for-each-ref", "--format=%(refname)", "refs/sill/"]), "refs/sill/workspaces/ab12/base");
    // a second create at the same place refuses; a stranger's directory is never removed
    assert!(create(&src, "ab12", "Fix the Thing!", &c.path, "op-2").is_err());
    assert!(remove_partial(&c.path, "op-2").is_err());
    assert!(c.path.exists());
    assert!(remove_partial(&c.path, "op-1").unwrap());
    assert!(!c.path.exists());
}

#[test]
fn anchors_keep_a_workspace_through_a_hostile_source_gc() {
    let d = tmp("gc");
    let s = source(&d);
    let (src, c) = make(&d, &s, "ab12");
    agent_commit(&c.path);
    // the person rewrites everything and prunes
    git(&s, &["checkout", "-q", "--orphan", "tmp"]);
    git(&s, &["rm", "-qrf", "."]);
    write(&s.join("fresh.txt"), "fresh\n");
    git(&s, &["add", "."]);
    git(&s, &["commit", "-qm", "fresh"]);
    git(&s, &["branch", "-qD", "main"]);
    git(&s, &["branch", "-qm", "main"]);
    git(&s, &["reflog", "expire", "--expire=now", "--all"]);
    git(&s, &["gc", "-q", "--prune=now"]);
    assert!(git_ok(&c.path, &["fsck", "--full"]));
    assert_eq!(git(&c.path, &["show", &format!("{}:f1.txt", src.head)]), "line 1");
    // without the anchor the same gc breaks it
    drop_anchors(&s, "ab12").unwrap();
    git(&s, &["reflog", "expire", "--expire=now", "--all"]);
    git(&s, &["gc", "-q", "--prune=now"]);
    assert!(!git_ok(&c.path, &["fsck", "--full"]));
}

#[test]
fn summary_and_revision_follow_the_work() {
    let d = tmp("summary");
    let s = source(&d);
    let (src, c) = make(&d, &s, "ab12");
    let clean = revision(&c.path, &c.branch).unwrap();
    assert_eq!(clean, Revision { tip: src.head.clone(), worktree: String::new() });
    agent_commit(&c.path);
    write(&c.path.join("f3.txt"), "uncommitted\n");
    write(&c.path.join("loose.txt"), "untracked\n");
    write(&c.path.join(".gitignore"), "*.log\n");
    write(&c.path.join("build.log"), "ignored\n");
    let sum = summary(&c.path, &c.branch, &c.base).unwrap();
    assert_eq!(sum.commits, 1);
    assert!(sum.files >= 2 && sum.added >= 2);
    assert!(sum.uncommitted.contains(&"f3.txt".to_string()) && sum.uncommitted.contains(&"loose.txt".to_string()));
    assert_eq!(sum.ignored, vec!["build.log".to_string()]);
    let r1 = revision(&c.path, &c.branch).unwrap();
    assert!(!r1.worktree.is_empty() && r1.tip != src.head);
    write(&c.path.join("loose.txt"), "changed untracked\n");
    let r2 = revision(&c.path, &c.branch).unwrap();
    assert_eq!(r1.tip, r2.tip);
    assert_ne!(r1.worktree, r2.worktree, "an untracked file's content is part of the revision");
}

#[test]
fn apply_works_with_packed_refs() {
    let d = tmp("packed");
    let s = source(&d);
    let (src, c) = make(&d, &s, "ab12");
    agent_commit(&c.path);
    git(&s, &["pack-refs", "--all"]);
    assert!(!s.join(".git/refs/heads/main").exists());
    let new = super::flow::apply(&s, &c.path, "ab12", "main", &src.head, &c.branch, &Default::default()).unwrap().unwrap().tip;
    assert_eq!(git(&s, &["rev-parse", "main"]), new);
    assert!(git_ok(&s, &["fsck"]));
    // the view's link to packed-refs is intact: the real file is still a file
    assert!(s.join(".git/packed-refs").is_file() && !s.join(".git/packed-refs").is_symlink());
}

#[test]
fn rebase_onto_a_moved_source_needs_no_fetch() {
    let d = tmp("rebase");
    let s = source(&d);
    let (_, c) = make(&d, &s, "ab12");
    agent_commit(&c.path);
    write(&s.join("f3.txt"), "host work\n");
    git(&s, &["commit", "-qam", "host"]);
    let dest = set_destination(&s, &c.path, "ab12", "main", &c.branch).unwrap();
    assert_eq!(git(&s, &["rev-parse", "refs/sill/workspaces/ab12/dest"]), dest);
    assert_eq!(git(&c.path, &["rev-parse", "refs/sill/dest"]), dest);
    let before = git(&c.path, &["rev-parse", "HEAD"]);
    assert_eq!(git(&c.path, &["rev-parse", "refs/sill/pre-rebase"]), before);
    git(&c.path, &["rebase", "-q", "refs/sill/dest"]); // the helper jail's job; plain here
    let new = super::flow::apply(&s, &c.path, "ab12", "main", &dest, &c.branch, &Default::default()).unwrap().unwrap().tip;
    assert_eq!(std::fs::read_to_string(s.join("f3.txt")).unwrap(), "host work\n");
    assert_eq!(std::fs::read_to_string(s.join("f2.txt")).unwrap(), "agent work\n");
    assert_eq!(git(&s, &["rev-parse", "main"]), new);
}

/// Programs either repository's config names: none may run while sill works on the host.
#[test]
fn no_program_from_either_config_runs_on_the_host() {
    let d = tmp("canary");
    let s = source(&d);
    let (_, c) = make(&d, &s, "ab12");
    let log = d.0.join("ran.log");
    let canary = |name: &str| {
        let p = d.0.join(format!("c-{name}"));
        write(&p, &format!("#!/bin/sh\necho {name} >> {}\ncat\n", log.display()));
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p.display().to_string()
    };
    write(&c.path.join(".gitattributes"), "* filter=evil diff=evil\n");
    agent_commit(&c.path);
    let hooks = d.0.join("hooks");
    for h in ["post-checkout", "reference-transaction", "post-merge", "pre-auto-gc", "post-index-change"] {
        write(&hooks.join(h), &format!("#!/bin/sh\necho hook-{h} >> {}\n", log.display()));
        std::fs::set_permissions(hooks.join(h), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    for repo in [&s, &c.path] {
        for (k, v) in [
            ("core.fsmonitor", canary("fsmonitor")),
            ("core.hooksPath", hooks.display().to_string()),
            ("filter.evil.clean", canary("clean")),
            ("filter.evil.smudge", canary("smudge")),
            ("filter.evil.process", canary("process")),
            ("diff.evil.textconv", canary("textconv")),
            ("diff.external", canary("external")),
            ("uploadpack.packObjectsHook", canary("packobjects")),
            ("core.pager", canary("pager")),
            ("core.sshCommand", canary("ssh")),
        ] {
            git(repo, &["config", k, &v]);
        }
    }
    write(&s.join("f1.txt"), "dirty\n");
    let _ = summary(&c.path, &c.branch, &c.base).unwrap();
    let _ = revision(&c.path, &c.branch).unwrap();
    set_destination(&s, &c.path, "ab12", "main", &c.branch).unwrap();
    // the §12 flow too: sill's own commit of uncommitted work as the person, an Apply, a Discard,
    // an Undo, a workspace made warm
    write(&c.path.join("f3.txt"), "agent uncommitted\n");
    write(&c.path.join("extra.txt"), "agent new\n");
    git(&s, &["config", "user.name", "Pat Person"]);
    git(&s, &["config", "user.email", "pat@example.org"]);
    let (name, email) = super::flow::identity(&s).unwrap().unwrap();
    assert!(super::flow::commit_all(&c.path, &c.branch, "the session's title", &name, &email).unwrap());
    let tip = git(&s, &["rev-parse", "main"]);
    super::flow::apply(&s, &c.path, "ab12", "main", &tip, &c.branch, &Default::default()).unwrap().unwrap();
    write(&c.path.join("f3.txt"), "more\n");
    let kept = super::flow::discard(&s, &c.path, "ab12", "1", &c.branch).unwrap();
    assert!(super::flow::undo(&s, &c.path, &c.branch, &kept).unwrap());
    // (inspect would now refuse it: the applied .gitattributes names a filter its config defines)
    let again = Source { root: s.clone(), branch: "main".into(), head: git(&s, &["rev-parse", "main"]), object_format: "sha1".into(), shallow: false, dirty: true };
    let to = path_for(&d.0.join("ws"), &again, "cd34", "again");
    create_as(&again, "cd34", "again", &to, "op-2", seed::How::Bytes).unwrap();
    drop_anchors(&s, "ab12").unwrap();
    let ran = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(ran.is_empty(), "programs ran on the host: {ran}");
}

