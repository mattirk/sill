//! The §12 flow against real repositories: snapshots, repeated Apply with uncommitted work, the
//! person's own edits stopping it, Discard and Undo, and a spare folder reused clean of Git
//! settings but warm with ignored build output.
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;
use crate::workspace::{create, inspect, path_for};

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(name: &str) -> Dir {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("sill-flow-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d.canonicalize().unwrap())
}

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

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

/// A source with `.gitignore` for `target/`, and its workspace.
fn setup(d: &Dir) -> (PathBuf, Source, crate::workspace::Created) {
    let s = d.0.join("src");
    std::fs::create_dir_all(&s).unwrap();
    git(&s, &["init", "-q"]);
    write(&s.join(".gitignore"), "target/\n");
    for i in 1..=3 {
        write(&s.join(format!("f{i}.txt")), &format!("line {i}\n"));
    }
    git(&s, &["add", "."]);
    git(&s, &["commit", "-qm", "start"]);
    let src = inspect(&s).unwrap().unwrap();
    let path = path_for(&d.0.join("ws"), &src, "w1", "try");
    let made = create(&src, "w1", "try", &path, "op").unwrap();
    (s, src, made)
}

fn head(s: &Path) -> String {
    git(s, &["rev-parse", "main"])
}

/// Apply as the records do: from the source's tip, against what was applied, then recording it.
fn apply_now(s: &Path, c: &crate::workspace::Created) -> std::result::Result<Applied, Stop> {
    let before = head(s);
    let was = applied(&c.path).unwrap();
    let out = apply(s, &c.path, "w1", "main", &before, &c.branch, &was).unwrap();
    if let Ok(a) = &out {
        set_applied(&c.path, &a.snapshot).unwrap();
    }
    out
}

#[test]
fn a_snapshot_takes_everything_but_ignored_files_and_touches_nothing() {
    let d = tmp("snap");
    let (_, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "edited\n");
    write(&c.path.join("new.txt"), "untracked\n");
    write(&c.path.join("target/big"), "build output\n");
    let status = git(&c.path, &["status", "--porcelain"]);
    let snap = snapshot(&c.path, "x").unwrap();
    assert_eq!(git(&c.path, &["status", "--porcelain"]), status, "index and files untouched");
    assert_eq!(git(&c.path, &["show", &format!("{snap}:f1.txt")]), "edited");
    assert_eq!(git(&c.path, &["show", &format!("{snap}:new.txt")]), "untracked");
    assert!(git(&c.path, &["ls-tree", "-r", "--name-only", &snap]).lines().all(|l| !l.starts_with("target")));
    assert_eq!(git(&c.path, &["rev-parse", &format!("{snap}^")]), git(&c.path, &["rev-parse", "HEAD"]));
    assert!(unapplied(&c.path).unwrap());
}

#[test]
fn apply_brings_commits_and_uncommitted_work_and_can_repeat() {
    let d = tmp("apply");
    let (s, _, c) = setup(&d);
    assert!(!unapplied(&c.path).unwrap(), "nothing to apply at the start");
    // a commit and an uncommitted edit, a new file and a deletion
    write(&c.path.join("f1.txt"), "committed\n");
    git(&c.path, &["commit", "-qam", "agent commit"]);
    write(&c.path.join("f2.txt"), "uncommitted\n");
    write(&c.path.join("added.txt"), "new\n");
    std::fs::remove_file(c.path.join("f3.txt")).unwrap();
    let a = apply_now(&s, &c).unwrap();
    assert_eq!(head(&s), a.tip, "the commit is in the person's history");
    assert_eq!(git(&s, &["log", "-1", "--format=%s"]), "agent commit");
    assert_eq!(read(&s.join("f1.txt")), "committed\n");
    assert_eq!(read(&s.join("f2.txt")), "uncommitted\n");
    assert_eq!(read(&s.join("added.txt")), "new\n");
    assert!(!s.join("f3.txt").exists());
    let status = git(&s, &["status", "--porcelain"]);
    assert!(status.contains("M f2.txt") && status.contains("?? added.txt") && status.contains("D f3.txt"), "uncommitted: {status}");
    assert!(!unapplied(&c.path).unwrap());
    // the agent goes on: edits the same uncommitted file again, then commits it all
    write(&c.path.join("f2.txt"), "uncommitted, revised\n");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("f2.txt")), "uncommitted, revised\n");
    git(&c.path, &["add", "-A"]);
    git(&c.path, &["commit", "-qm", "agent commits the rest"]);
    let a = apply_now(&s, &c).unwrap();
    assert_eq!(head(&s), a.tip);
    assert_eq!(git(&s, &["status", "--porcelain"]), "", "all committed now, the checkout is clean");
}

#[test]
fn the_persons_own_edits_stop_apply_and_nothing_is_written() {
    let d = tmp("person");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f2.txt"), "agent\n");
    apply_now(&s, &c).unwrap();
    // the person edits what the agent applied, and the agent edits it again
    write(&s.join("f2.txt"), "the person's\n");
    write(&c.path.join("f2.txt"), "agent again\n");
    write(&c.path.join("f1.txt"), "agent f1\n");
    assert_eq!(apply_now(&s, &c).unwrap_err(), Stop::Overlap(vec!["f2.txt".into()]));
    assert_eq!(read(&s.join("f2.txt")), "the person's\n");
    assert_eq!(read(&s.join("f1.txt")), "line 1\n", "nothing written");
    // an untracked file of the person's where the agent adds one
    git(&s, &["checkout", "-q", "--", "f2.txt"]);
    write(&c.path.join("f2.txt"), "line 2\n");
    write(&s.join("mine.txt"), "mine\n");
    write(&c.path.join("mine.txt"), "agent's\n");
    assert_eq!(apply_now(&s, &c).unwrap_err(), Stop::Untracked(vec!["mine.txt".into()]));
    assert_eq!(read(&s.join("mine.txt")), "mine\n");
    // an edit of the person's elsewhere is kept through an Apply
    std::fs::remove_file(c.path.join("mine.txt")).unwrap();
    write(&s.join("f3.txt"), "the person's f3\n");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("f3.txt")), "the person's f3\n");
    assert_eq!(read(&s.join("f1.txt")), "agent f1\n");
}

#[test]
fn a_moved_checkout_asks_for_a_rebase_and_symlinks_stay_links() {
    let d = tmp("moved");
    let (s, _, c) = setup(&d);
    write(&s.join("f3.txt"), "host\n");
    git(&s, &["commit", "-qam", "host"]);
    git(&c.path, &["commit", "--allow-empty", "-qm", "agent"]);
    let was = applied(&c.path).unwrap();
    assert_eq!(apply(&s, &c.path, "w1", "main", &head(&s), &c.branch, &was).unwrap(), Err(Stop::NeedsPrepare));
    // a link the agent makes arrives as a link, its target not read
    let d2 = tmp("link");
    let (s2, _, c2) = setup(&d2);
    std::os::unix::fs::symlink("/etc/hostname", c2.path.join("link")).unwrap();
    apply_now(&s2, &c2).unwrap();
    assert_eq!(std::fs::read_link(s2.join("link")).unwrap(), Path::new("/etc/hostname"));
    // a directory of the person's that is a link out of the checkout is never written through
    let outside = d2.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, s2.join("sub")).unwrap();
    git(&s2, &["add", "sub"]);
    git(&s2, &["commit", "-qm", "sub is a link"]);
    let d3 = tmp("link2");
    let _ = d3;
    std::fs::remove_file(c2.path.join("link")).unwrap();
    git(&c2.path, &["fetch", "-q", s2.to_str().unwrap(), "main"]);
    git(&c2.path, &["reset", "-q", "--hard", "FETCH_HEAD"]);
    set_applied(&c2.path, &snapshot(&c2.path, "x").unwrap()).unwrap();
    std::fs::remove_file(c2.path.join("sub")).unwrap();
    write(&c2.path.join("sub/escape.txt"), "x\n");
    let before = head(&s2);
    let was = applied(&c2.path).unwrap();
    assert!(apply(&s2, &c2.path, "w1", "main", &before, &c2.branch, &was).is_err() || !outside.join("escape.txt").exists());
    assert!(!outside.join("escape.txt").exists(), "nothing written through the link");
}

#[test]
fn discard_returns_to_what_was_applied_and_undo_brings_it_back() {
    let d = tmp("discard");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "applied work\n");
    apply_now(&s, &c).unwrap();
    // more work, some committed, some not, then discarded
    write(&c.path.join("f2.txt"), "later\n");
    git(&c.path, &["commit", "-qam", "later commit"]);
    write(&c.path.join("f3.txt"), "later uncommitted\n");
    write(&c.path.join("scratch.txt"), "new\n");
    write(&c.path.join("target/out"), "build\n");
    let kept = discard(&s, &c.path, "w1", "1", &c.branch).unwrap();
    assert_eq!(read(&c.path.join("f1.txt")), "applied work\n", "what was applied stays");
    assert_eq!(read(&c.path.join("f2.txt")), "line 2\n");
    assert_eq!(read(&c.path.join("f3.txt")), "line 3\n");
    assert!(!c.path.join("scratch.txt").exists());
    assert!(c.path.join("target/out").exists(), "ignored build output kept");
    assert!(!unapplied(&c.path).unwrap());
    assert_eq!(git(&s, &["rev-parse", "refs/sill/workspaces/w1/discarded/1"]), kept, "kept in the person's own store");
    // Undo brings all of it back
    assert!(undo(&c.path, &c.branch, &kept).unwrap());
    assert_eq!(read(&c.path.join("f2.txt")), "later\n");
    assert_eq!(read(&c.path.join("f3.txt")), "later uncommitted\n");
    assert_eq!(read(&c.path.join("scratch.txt")), "new\n");
    assert_eq!(git(&c.path, &["log", "-1", "--format=%s"]), "later commit");
    // discarded again, then new work: Undo would overwrite it, so it refuses
    let kept = discard(&s, &c.path, "w1", "2", &c.branch).unwrap();
    write(&c.path.join("f2.txt"), "newer\n");
    assert!(!undo(&c.path, &c.branch, &kept).unwrap());
    assert_eq!(read(&c.path.join("f2.txt")), "newer\n");
}

/// The person reverts an applied file in their checkout and the agent reverts it in the
/// workspace: both sides equal, nothing to bring, however the record of the last Apply reads.
#[test]
fn a_revert_on_both_sides_leaves_nothing_to_apply() {
    let d = tmp("reconcile");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "edited\n");
    write(&c.path.join("new.txt"), "untracked\n");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("f1.txt")), "edited\n");
    git(&s, &["reset", "-q", "--hard"]);
    std::fs::remove_file(s.join("new.txt")).unwrap();
    git(&c.path, &["reset", "-q", "--hard"]);
    std::fs::remove_file(c.path.join("new.txt")).unwrap();
    assert!(unapplied(&c.path).unwrap(), "the record alone still says the revert is pending");
    assert!(reconcile(&s, &c.path).unwrap());
    assert!(!unapplied(&c.path).unwrap(), "the person already has what the workspace has");
    assert!(!reconcile(&s, &c.path).unwrap(), "settled once");
    assert_eq!(git(&c.path, &["status", "--porcelain"]), "", "the workspace is untouched");
    assert_eq!(git(&s, &["status", "--porcelain"]), "", "the checkout is untouched");
    // a later Discard has nothing to bring back
    discard(&s, &c.path, "w1", "1", &c.branch).unwrap();
    assert_eq!(read(&c.path.join("f1.txt")), "line 1\n");
    assert!(!c.path.join("new.txt").exists());
}

/// The person resets their checkout after an Apply while the workspace keeps the change: the
/// change is not theirs any more, so it is offered again and Apply brings it back.
#[test]
fn a_reset_checkout_gets_the_applied_change_offered_again() {
    let d = tmp("reconcile-host");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "edited\n");
    write(&c.path.join("new.txt"), "untracked\n");
    apply_now(&s, &c).unwrap();
    git(&s, &["reset", "-q", "--hard"]);
    std::fs::remove_file(s.join("new.txt")).unwrap();
    // the agent goes on in another file
    write(&c.path.join("f2.txt"), "more\n");
    assert!(reconcile(&s, &c.path).unwrap());
    let changed = git(&c.path, &["diff", "--name-only", APPLIED, &snapshot(&c.path, "x").unwrap()]);
    assert_eq!(changed.lines().collect::<Vec<_>>(), ["f1.txt", "f2.txt", "new.txt"], "all of it is pending again");
    assert!(!reconcile(&s, &c.path).unwrap(), "settled once");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("f1.txt")), "edited\n");
    assert_eq!(read(&s.join("new.txt")), "untracked\n");
    assert_eq!(read(&s.join("f2.txt")), "more\n");
    assert!(!unapplied(&c.path).unwrap());
    // the person's own different edit of an applied file is theirs: not taken into the record
    write(&s.join("f1.txt"), "the person's own\n");
    assert!(!reconcile(&s, &c.path).unwrap());
}

/// Reverting in the workspace alone is a real difference: the checkout still has the edit, and
/// bringing the revert there is offered as before. The person's own other edit stays theirs.
#[test]
fn a_revert_in_the_workspace_alone_still_offers_the_reverse() {
    let d = tmp("reconcile-one");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "edited\n");
    apply_now(&s, &c).unwrap();
    git(&c.path, &["reset", "-q", "--hard"]);
    assert!(!reconcile(&s, &c.path).unwrap());
    assert!(unapplied(&c.path).unwrap(), "the checkout has the edit, the workspace does not");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("f1.txt")), "line 1\n", "the revert is brought over");
    assert!(!unapplied(&c.path).unwrap());
    // the person edits the applied file to something else: theirs, and it stops Apply
    write(&c.path.join("f1.txt"), "edited again\n");
    apply_now(&s, &c).unwrap();
    write(&s.join("f1.txt"), "the person's own\n");
    git(&c.path, &["reset", "-q", "--hard"]);
    assert!(!reconcile(&s, &c.path).unwrap());
    assert!(unapplied(&c.path).unwrap());
    assert_eq!(apply_now(&s, &c).unwrap_err(), Stop::Overlap(vec!["f1.txt".into()]));
    assert_eq!(read(&s.join("f1.txt")), "the person's own\n");
}

#[test]
fn a_reused_folder_keeps_build_output_but_nothing_a_session_planted() {
    let d = tmp("reuse");
    let (s, src, c) = setup(&d);
    write(&c.path.join("target/debug/app"), "warm build\n");
    write(&c.path.join("leftover.txt"), "untracked\n");
    write(&c.path.join("f1.txt"), "unfinished\n");
    git(&c.path, &["checkout", "-qb", "agent-branch"]);
    git(&c.path, &["config", "core.fsmonitor", "/bin/false"]);
    git(&c.path, &["config", "alias.x", "!touch /tmp/sill-planted"]);
    write(&c.path.join(".git/hooks/post-checkout"), "#!/bin/sh\ntouch /tmp/sill-planted\n");
    write(&c.path.join(".git/info/exclude"), "f2.txt\n");
    // the source moved on meanwhile
    write(&s.join("f3.txt"), "host\n");
    git(&s, &["commit", "-qam", "host"]);
    let src = Source { head: head(&s), ..src };
    let spare = spare_path(&d.0.join("ws"), &s);
    std::fs::create_dir_all(spare.parent().unwrap()).unwrap();
    std::fs::rename(&c.path, &spare).unwrap();
    let to = path_for(&d.0.join("ws"), &src, "w2", "again");
    let made = reuse(&spare, &src, "w2", "again", &to, "op2").unwrap();
    assert!(!spare.exists() && to.exists());
    assert_eq!(read(&to.join("target/debug/app")), "warm build\n", "ignored build output kept");
    assert!(!to.join("leftover.txt").exists());
    assert_eq!(read(&to.join("f1.txt")), "line 1\n");
    assert_eq!(read(&to.join("f3.txt")), "host\n", "at the source's HEAD");
    assert_eq!(git(&to, &["rev-parse", "HEAD"]), src.head);
    assert_eq!(git(&to, &["branch", "--format=%(refname:short)"]), made.branch, "only the new branch");
    let config = read(&to.join(".git/config"));
    assert!(!config.contains("fsmonitor") && !config.contains("alias"), "{config}");
    assert!(!to.join(".git/hooks/post-checkout").exists());
    assert!(!std::fs::read_to_string(to.join(".git/info/exclude")).unwrap_or_default().contains("f2.txt"));
    assert_eq!(git(&to, &["status", "--porcelain"]), "");
    assert!(!unapplied(&to).unwrap());
    git(&to, &["fsck"]);
}
