//! The §12 flow against real repositories: snapshots, repeated Apply of commits only, the
//! person's own edits stopping it, two workspaces reaching one checkout, Discard and Undo, and a
//! workspace made warm from the checkout's files.
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;
use crate::workspace::{Source, create, inspect, path_for};

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

/// Apply as the records do: from the source's tip.
fn apply_now(s: &Path, c: &crate::workspace::Created) -> std::result::Result<Applied, Stop> {
    apply(s, &c.path, "w1", "main", &head(s), &c.branch, &Default::default()).unwrap()
}

/// The agent commits everything in its workspace.
fn commit(c: &crate::workspace::Created, message: &str) {
    git(&c.path, &["add", "-A"]);
    git(&c.path, &["commit", "-qm", message]);
}

/// A second workspace of the same checkout: another session's.
fn second(d: &Dir, src: &Source) -> crate::workspace::Created {
    let path = path_for(&d.0.join("ws"), src, "w2", "other");
    create(src, "w2", "other", &path, "op2").unwrap()
}

#[test]
fn a_snapshot_takes_everything_but_ignored_files_and_touches_nothing() {
    let d = tmp("snap");
    let (s, _, c) = setup(&d);
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
    assert!(unapplied(&s, &c.path, &c.branch).unwrap());
}

/// Apply brings commits and nothing else: uncommitted work stops it with nothing written, and
/// once committed it arrives as a fast-forward that leaves the checkout clean.
#[test]
fn apply_brings_commits_only_and_can_repeat() {
    let d = tmp("apply");
    let (s, _, c) = setup(&d);
    let start = head(&s);
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap(), "nothing to apply at the start");
    write(&c.path.join("f1.txt"), "committed\n");
    git(&c.path, &["commit", "-qam", "agent commit"]);
    write(&c.path.join("f2.txt"), "uncommitted\n");
    write(&c.path.join("added.txt"), "new\n");
    std::fs::remove_file(c.path.join("f3.txt")).unwrap();
    assert!(uncommitted(&c.path).unwrap());
    assert_eq!(apply_now(&s, &c).unwrap_err(), Stop::Uncommitted);
    assert_eq!(head(&s), start, "the branch has not moved");
    assert_eq!(git(&s, &["status", "--porcelain"]), "", "nothing written");
    assert_eq!(read(&s.join("f1.txt")), "line 1\n");
    // the agent commits the rest: all of it arrives, committed
    commit(&c, "agent commits the rest");
    assert!(!uncommitted(&c.path).unwrap());
    let a = apply_now(&s, &c).unwrap();
    assert_eq!((a.before.as_str(), head(&s)), (start.as_str(), a.tip.clone()), "the commits are in the person's history");
    assert_eq!(git(&s, &["log", "-2", "--format=%s"]), "agent commits the rest\nagent commit");
    assert_eq!(read(&s.join("f1.txt")), "committed\n");
    assert_eq!(read(&s.join("f2.txt")), "uncommitted\n");
    assert_eq!(read(&s.join("added.txt")), "new\n");
    assert!(!s.join("f3.txt").exists());
    assert_eq!(git(&s, &["status", "--porcelain"]), "", "the checkout is clean: nothing arrives uncommitted");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
    // nothing new: an Apply is a no-op; then the agent goes on
    assert_eq!(apply_now(&s, &c).unwrap().tip, a.tip);
    write(&c.path.join("f2.txt"), "revised\n");
    commit(&c, "a fix after testing");
    let b = apply_now(&s, &c).unwrap();
    assert_eq!((b.before, head(&s)), (a.tip, b.tip.clone()));
    assert_eq!(read(&s.join("f2.txt")), "revised\n");
    assert_eq!(git(&s, &["status", "--porcelain"]), "");
}

/// A workspace no agent will write again: sill commits what is uncommitted, as the person, and
/// the Apply that follows brings it.
#[test]
fn commit_all_commits_the_uncommitted_work_as_the_person() {
    let d = tmp("commit-all");
    let (s, _, c) = setup(&d);
    git(&s, &["config", "user.name", "Pat Person"]);
    git(&s, &["config", "user.email", "pat@example.org"]);
    assert_eq!(identity(&s).unwrap(), Some(("Pat Person".into(), "pat@example.org".into())), "the repository's own identity");
    assert!(!commit_all(&c.path, &c.branch, "nothing", "Pat Person", "pat@example.org").unwrap(), "nothing to commit");
    write(&c.path.join("f1.txt"), "edited\n");
    write(&c.path.join("new.txt"), "untracked\n");
    std::fs::remove_file(c.path.join("f3.txt")).unwrap();
    write(&c.path.join("target/big"), "build output\n");
    let before = git(&c.path, &["rev-parse", "HEAD"]);
    assert!(commit_all(&c.path, &c.branch, "Tidy the reader", "Pat Person", "pat@example.org").unwrap());
    assert_eq!(git(&c.path, &["log", "-1", "--format=%an <%ae>|%cn <%ce>|%s|%P"]), format!("Pat Person <pat@example.org>|Pat Person <pat@example.org>|Tidy the reader|{before}"));
    assert_eq!(git(&c.path, &["status", "--porcelain"]), "", "the workspace is clean against its new commit");
    assert_eq!(git(&c.path, &["ls-tree", "-r", "--name-only", "HEAD"]), ".gitignore\nf1.txt\nf2.txt\nnew.txt", "ignored build output is not committed");
    assert!(c.path.join("target/big").exists());
    assert!(!uncommitted(&c.path).unwrap());
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("new.txt")), "untracked\n");
    assert!(!s.join("f3.txt").exists());
    assert_eq!(git(&s, &["log", "-1", "--format=%an|%s"]), "Pat Person|Tidy the reader");
}

#[test]
fn the_persons_own_edits_stop_apply_and_nothing_is_written() {
    let d = tmp("person");
    let (s, _, c) = setup(&d);
    let start = head(&s);
    // the person edits a file the agent's commit changes
    write(&c.path.join("f1.txt"), "agent f1\n");
    write(&c.path.join("f2.txt"), "agent\n");
    commit(&c, "agent");
    write(&s.join("f2.txt"), "the person's\n");
    assert_eq!(apply_now(&s, &c).unwrap_err(), Stop::Overlap(vec!["f2.txt".into()]));
    assert_eq!(read(&s.join("f2.txt")), "the person's\n");
    assert_eq!(read(&s.join("f1.txt")), "line 1\n", "nothing written");
    assert_eq!(head(&s), start, "the branch has not moved");
    // an untracked file of the person's where the agent adds one
    git(&s, &["checkout", "-q", "--", "f2.txt"]);
    write(&s.join("mine.txt"), "mine\n");
    write(&c.path.join("mine.txt"), "agent's\n");
    commit(&c, "agent adds mine.txt");
    assert_eq!(apply_now(&s, &c).unwrap_err(), Stop::Untracked(vec!["mine.txt".into()]));
    assert_eq!(read(&s.join("mine.txt")), "mine\n");
    assert_eq!(head(&s), start);
    // an edit of the person's elsewhere is kept through an Apply
    git(&c.path, &["rm", "-q", "mine.txt"]);
    commit(&c, "agent drops it");
    std::fs::remove_file(s.join("mine.txt")).unwrap();
    write(&s.join("f3.txt"), "the person's f3\n");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("f3.txt")), "the person's f3\n");
    assert_eq!(read(&s.join("f1.txt")), "agent f1\n");
    assert_eq!(git(&s, &["status", "--porcelain"]), "M f3.txt", "only their own edit is uncommitted");
}

/// Files of the person's that Git ignores are theirs all the same: a commit that adds a file at
/// such a path, or below one, stops, where `read-tree` alone would write over them.
#[test]
fn the_persons_ignored_files_in_the_way_stop_apply() {
    let d = tmp("ignored-way");
    let (s, _, c) = setup(&d);
    write(&s.join(".git/info/exclude"), "local.env\nnotes\n");
    write(&s.join("local.env"), "PERSON=only copy\n");
    write(&s.join("notes"), "a file of theirs\n");
    write(&c.path.join("local.env"), "AGENT=1\n");
    write(&c.path.join("notes/a.txt"), "the agent's folder\n");
    commit(&c, "agent");
    assert_eq!(apply_now(&s, &c).unwrap_err(), Stop::Untracked(vec!["local.env".into(), "notes".into()]));
    assert_eq!((read(&s.join("local.env")), read(&s.join("notes"))), ("PERSON=only copy\n".into(), "a file of theirs\n".into()));
    assert_eq!(git(&s, &["log", "-1", "--format=%s"]), "start", "the branch has not moved");
    // out of the way, or already holding what would be written: applied
    std::fs::remove_file(s.join("notes")).unwrap();
    write(&s.join("local.env"), "AGENT=1\n");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("notes/a.txt")), "the agent's folder\n");
}

#[test]
fn a_moved_checkout_asks_for_a_rebase_and_symlinks_stay_links() {
    let d = tmp("moved");
    let (s, _, c) = setup(&d);
    write(&s.join("f3.txt"), "host\n");
    git(&s, &["commit", "-qam", "host"]);
    git(&c.path, &["commit", "--allow-empty", "-qm", "agent"]);
    assert_eq!(apply(&s, &c.path, "w1", "main", &head(&s), &c.branch, &Default::default()).unwrap(), Err(Stop::NeedsPrepare));
    // a link the agent commits arrives as a link, its target not read
    let d2 = tmp("link");
    let (s2, _, c2) = setup(&d2);
    std::os::unix::fs::symlink("/etc/hostname", c2.path.join("link")).unwrap();
    commit(&c2, "a link");
    apply_now(&s2, &c2).unwrap();
    assert_eq!(std::fs::read_link(s2.join("link")).unwrap(), Path::new("/etc/hostname"));
    // a directory of the person's that is a link out of the checkout is never written through
    let outside = d2.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, s2.join("sub")).unwrap();
    git(&s2, &["add", "sub"]);
    git(&s2, &["commit", "-qm", "sub is a link"]);
    git(&c2.path, &["fetch", "-q", s2.to_str().unwrap(), "main"]);
    git(&c2.path, &["reset", "-q", "--hard", "FETCH_HEAD"]);
    std::fs::remove_file(c2.path.join("sub")).unwrap();
    write(&c2.path.join("sub/escape.txt"), "x\n");
    commit(&c2, "sub is a directory");
    let _ = apply(&s2, &c2.path, "w1", "main", &head(&s2), &c2.branch, &Default::default());
    assert!(!outside.join("escape.txt").exists(), "nothing written through the link");
}

#[test]
fn discard_returns_to_what_was_applied_and_undo_brings_it_back() {
    let d = tmp("discard");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "applied work\n");
    commit(&c, "applied");
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
    assert_eq!(git(&c.path, &["rev-parse", "HEAD"]), head(&s), "on the person's commit");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
    assert_eq!(git(&s, &["rev-parse", "refs/sill/workspaces/w1/discarded/1"]), kept, "kept in the person's own store");
    // Undo brings all of it back
    assert!(undo(&s, &c.path, &c.branch, &kept).unwrap());
    assert_eq!(read(&c.path.join("f2.txt")), "later\n");
    assert_eq!(read(&c.path.join("f3.txt")), "later uncommitted\n");
    assert_eq!(read(&c.path.join("scratch.txt")), "new\n");
    assert_eq!(git(&c.path, &["log", "-1", "--format=%s"]), "later commit");
    // discarded again, then new work: Undo would overwrite it, so it refuses
    let kept = discard(&s, &c.path, "w1", "2", &c.branch).unwrap();
    write(&c.path.join("f2.txt"), "newer\n");
    assert!(!undo(&s, &c.path, &c.branch, &kept).unwrap());
    assert_eq!(read(&c.path.join("f2.txt")), "newer\n");
}

/// What Apply would bring, as the paths of the mirror's difference to the workspace.
fn pending(s: &Path, c: &crate::workspace::Created) -> Vec<String> {
    let m = mirror(s, &c.path, &c.branch).unwrap();
    git(&c.path, &["diff", "--name-only", &m.tree, &m.now]).lines().map(str::to_string).collect()
}

/// The person resets their checkout to before an Apply while the workspace keeps the commit: it
/// is not theirs any more, so it is offered again and Apply brings it back. Their own edit of an
/// applied file is theirs: it stops an Apply that touches the file.
#[test]
fn a_reset_checkout_gets_the_applied_commit_offered_again() {
    let d = tmp("mirror-host");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "edited\n");
    write(&c.path.join("new.txt"), "added\n");
    commit(&c, "agent");
    apply_now(&s, &c).unwrap();
    assert!(pending(&s, &c).is_empty());
    git(&s, &["reset", "-q", "--hard", "HEAD~1"]);
    // the agent goes on in another file
    write(&c.path.join("f2.txt"), "more\n");
    assert_eq!(pending(&s, &c), ["f1.txt", "f2.txt", "new.txt"], "all of it is pending again");
    commit(&c, "more");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("f1.txt")), "edited\n");
    assert_eq!(read(&s.join("new.txt")), "added\n");
    assert_eq!(read(&s.join("f2.txt")), "more\n");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
    // the person's own edit of an applied file is not offered as the agent's work…
    write(&s.join("f1.txt"), "the person's own\n");
    assert!(pending(&s, &c).is_empty(), "nothing of the agent's is new");
    assert_eq!(mirror(&s, &c.path, &c.branch).unwrap().checkout_only, ["f1.txt"], "it is theirs, and the workspace lacks it");
    // …and stops the next Apply that touches the file
    write(&c.path.join("f1.txt"), "edited again\n");
    commit(&c, "again");
    assert_eq!(apply_now(&s, &c).unwrap_err(), Stop::Overlap(vec!["f1.txt".into()]));
    assert_eq!(read(&s.join("f1.txt")), "the person's own\n");
}

/// The agent takes an applied commit back with a commit of its own: the revert is what is
/// offered, and Apply brings it.
#[test]
fn a_revert_in_the_workspace_is_a_commit_like_any_other() {
    let d = tmp("mirror-one");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "edited\n");
    commit(&c, "edit");
    apply_now(&s, &c).unwrap();
    git(&c.path, &["revert", "--no-edit", "HEAD"]);
    assert_eq!(pending(&s, &c), ["f1.txt"], "the checkout has the edit, the workspace does not");
    apply_now(&s, &c).unwrap();
    assert_eq!(read(&s.join("f1.txt")), "line 1\n", "the revert is brought over");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
    // an edit made and taken back without a commit is nothing at all
    write(&c.path.join("f1.txt"), "tried\n");
    write(&c.path.join("f1.txt"), "line 1\n");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
    assert!(!uncommitted(&c.path).unwrap());
}

/// Two sessions on one checkout, never told of each other (2026-11-10: the reason Apply brings
/// commits only). The first Apply is a commit of the person's, so the second workspace is behind
/// it and takes it by rebase: its agent then has the other's work to build and test against, and
/// the second Apply brings both. With uncommitted Applies the second never saw the first's work.
#[test]
fn two_workspaces_reach_the_checkout_one_after_the_other() {
    let d = tmp("two");
    let (s, src, a) = setup(&d);
    let b = second(&d, &src);
    write(&a.path.join("f1.txt"), "session A\n");
    commit(&a, "A's work");
    write(&b.path.join("f2.txt"), "session B\n");
    commit(&b, "B's work");
    apply_now(&s, &a).unwrap();
    assert_eq!(apply(&s, &b.path, "w2", "main", &head(&s), &b.branch, &Default::default()).unwrap(), Err(Stop::NeedsPrepare), "B is behind A's commit");
    let m = mirror(&s, &b.path, &b.branch).unwrap();
    assert_eq!(git(&b.path, &["diff", "--name-only", &m.tree, &m.now]), "f2.txt", "B still offers its own work only");
    // the rebase is the helper jail's job; plain here
    git(&b.path, &["fetch", "-q", s.to_str().unwrap(), "main"]);
    git(&b.path, &["rebase", "-q", "FETCH_HEAD"]);
    assert_eq!(read(&b.path.join("f1.txt")), "session A\n", "B's workspace now has A's work");
    apply(&s, &b.path, "w2", "main", &head(&s), &b.branch, &Default::default()).unwrap().unwrap();
    assert_eq!((read(&s.join("f1.txt")), read(&s.join("f2.txt"))), ("session A\n".into(), "session B\n".into()));
    assert_eq!(git(&s, &["log", "-2", "--format=%s"]), "B's work\nA's work");
    assert_eq!(git(&s, &["status", "--porcelain"]), "");
}

/// The same line changed by both: the conflict is in the second workspace's rebase, where its
/// agent can resolve it, and the person's checkout — holding the first's commit — is untouched.
#[test]
fn two_workspaces_changing_the_same_line_conflict_in_the_second_workspace() {
    let d = tmp("two-conflict");
    let (s, src, a) = setup(&d);
    let b = second(&d, &src);
    write(&a.path.join("f1.txt"), "session A\n");
    commit(&a, "A's work");
    write(&b.path.join("f1.txt"), "session B\n");
    commit(&b, "B's work");
    apply_now(&s, &a).unwrap();
    let applied = head(&s);
    assert_eq!(apply(&s, &b.path, "w2", "main", &applied, &b.branch, &Default::default()).unwrap(), Err(Stop::NeedsPrepare));
    git(&b.path, &["fetch", "-q", s.to_str().unwrap(), "main"]);
    let out = Command::new("git").arg("-C").arg(&b.path).args(["-c", "user.name=t", "-c", "user.email=t@t", "rebase", "FETCH_HEAD"]).output().unwrap();
    assert!(!out.status.success(), "the rebase stops");
    assert!(read(&b.path.join("f1.txt")).contains("<<<<<<<"), "the conflict is in B's workspace");
    assert_eq!((head(&s), read(&s.join("f1.txt"))), (applied, "session A\n".into()), "the checkout is as A's Apply left it");
    assert_eq!(git(&s, &["status", "--porcelain"]), "");
}

/// After a rebase onto the person's new commit, Discard keeps the workspace on that commit: the
/// person's side is their tip, not where the workspace started (before 2026-11-09 it went back to
/// the recorded Apply's base, undoing the rebase).
#[test]
fn discard_after_a_rebase_stays_on_the_persons_commit() {
    let d = tmp("mirror-rebase");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "agent\n");
    write(&s.join("f3.txt"), "the person's commit\n");
    git(&s, &["commit", "-qam", "theirs"]);
    git(&c.path, &["fetch", "-q", s.to_str().unwrap(), "main"]);
    git(&c.path, &["rebase", "-q", "--autostash", "FETCH_HEAD"]);
    assert_eq!(read(&c.path.join("f1.txt")), "agent\n", "the edit survives the rebase");
    let m = mirror(&s, &c.path, &c.branch).unwrap();
    assert_eq!(m.upstream, head(&s));
    assert_eq!(pending(&s, &c), ["f1.txt"]);
    discard(&s, &c.path, "w1", "1", &c.branch).unwrap();
    assert_eq!(git(&c.path, &["rev-parse", &format!("refs/heads/{}", c.branch)]), head(&s), "still on the person's commit");
    assert_eq!(read(&c.path.join("f1.txt")), "line 1\n");
    assert_eq!(read(&c.path.join("f3.txt")), "the person's commit\n");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
}

/// The person commits on past where the workspace started, touching a file the agent also
/// edited, and the agent's work is discarded: the workspace comes to the person's commit, clean
/// against it. Before 2026-11-10 its branch stayed on the old commit with the person's newer file
/// uncommitted on top: nothing to apply on the Changes page, yet a changed file in the session.
#[test]
fn discard_after_the_person_moved_on_comes_to_their_commit_clean() {
    let d = tmp("mirror-ahead");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "agent\n");
    write(&c.path.join("scratch.txt"), "agent's new file\n");
    write(&s.join("f1.txt"), "the person's commit\n");
    write(&s.join("f2.txt"), "also theirs\n");
    git(&s, &["commit", "-qam", "theirs"]);
    // the person's uncommitted edits are never in a workspace (§5)
    write(&s.join("f3.txt"), "dirty, the person's\n");
    discard(&s, &c.path, "w1", "1", &c.branch).unwrap();
    assert_eq!(git(&c.path, &["rev-parse", &format!("refs/heads/{}", c.branch)]), head(&s), "on the person's commit");
    assert_eq!(read(&c.path.join("f1.txt")), "the person's commit\n");
    assert_eq!(read(&c.path.join("f2.txt")), "also theirs\n");
    assert_eq!(read(&c.path.join("f3.txt")), "line 3\n");
    assert!(!c.path.join("scratch.txt").exists());
    assert_eq!(git(&c.path, &["status", "--porcelain"]), "", "nothing changed in the workspace");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
}

/// The same when the agent's path is dirty in the checkout: the person's uncommitted edit stays
/// theirs alone. The workspace is their commit, clean (2026-11-10, commits only; before, Discard
/// copied the checkout's content onto the agent's paths, uncommitted).
#[test]
fn discard_never_takes_the_persons_uncommitted_edit() {
    let d = tmp("mirror-ahead-dirty");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "agent\n");
    write(&s.join("f2.txt"), "theirs\n");
    git(&s, &["commit", "-qam", "theirs"]);
    write(&s.join("f1.txt"), "the person's uncommitted\n");
    discard(&s, &c.path, "w1", "1", &c.branch).unwrap();
    assert_eq!(git(&c.path, &["rev-parse", &format!("refs/heads/{}", c.branch)]), head(&s));
    assert_eq!(read(&c.path.join("f1.txt")), "line 1\n");
    assert_eq!(read(&c.path.join("f2.txt")), "theirs\n");
    assert_eq!(git(&c.path, &["status", "--porcelain"]), "");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
    assert_eq!(read(&s.join("f1.txt")), "the person's uncommitted\n", "their checkout is untouched");
}

/// When the workspace has commits of its own the person lacks and they have moved on too, its
/// branch stays at their common ancestor: their tip would lose nothing, but taking it is a rebase.
#[test]
fn discard_with_diverged_history_stays_at_the_common_ancestor() {
    let d = tmp("mirror-diverged");
    let (s, _, c) = setup(&d);
    let start = head(&s);
    write(&c.path.join("f1.txt"), "agent\n");
    git(&c.path, &["commit", "-qam", "agent's"]);
    write(&s.join("f2.txt"), "theirs\n");
    git(&s, &["commit", "-qam", "theirs"]);
    discard(&s, &c.path, "w1", "1", &c.branch).unwrap();
    assert_eq!(git(&c.path, &["rev-parse", &format!("refs/heads/{}", c.branch)]), start);
    assert_eq!(read(&c.path.join("f1.txt")), "line 1\n");
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
}

/// The checkout as the person may have it when a session starts: build output, an edit, a
/// deletion, files of their own, links, a repository nested in it, Git settings.
fn lived_in(s: &Path) {
    use std::os::unix::fs::PermissionsExt;
    write(&s.join("target/debug/app"), "warm build\n");
    std::fs::set_permissions(s.join("target/debug/app"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("app", s.join("target/debug/link")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/elsewhere", s.join("target/dangling")).unwrap();
    write(&s.join("leftover.txt"), "the person's untracked file\n");
    write(&s.join("nested/n.txt"), "another repository\n");
    git(&s.join("nested"), &["init", "-q"]);
    write(&s.join("f1.txt"), "the person's edit\n");
    std::fs::remove_file(s.join("f2.txt")).unwrap();
    git(s, &["config", "core.fsmonitor", "/bin/false"]);
    write(&s.join(".git/hooks/post-checkout"), "#!/bin/sh\ntouch /tmp/sill-planted\n");
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    for f in ["f3.txt", "target/debug/app"] {
        std::fs::File::options().write(true).open(s.join(f)).unwrap().set_modified(old).unwrap();
    }
}

fn modified(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p).unwrap().modified().unwrap()
}

#[test]
fn a_workspace_made_from_the_checkout_is_warm_and_at_the_base() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("warm");
    let (s, src, _) = setup(&d);
    lived_in(&s);
    let to = path_for(&d.0.join("ws"), &src, "w2", "warm");
    let made = crate::workspace::create_as(&src, "w2", "warm", &to, "op2", crate::workspace::seed::How::Bytes).unwrap();
    // the build output, as it was
    assert_eq!(read(&to.join("target/debug/app")), "warm build\n");
    assert_eq!(std::fs::metadata(to.join("target/debug/app")).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(modified(&to.join("target/debug/app")), modified(&s.join("target/debug/app")));
    assert_eq!(std::fs::read_link(to.join("target/debug/link")).unwrap(), Path::new("app"));
    assert_eq!(std::fs::read_link(to.join("target/dangling")).unwrap(), Path::new("/nonexistent/elsewhere"));
    // the base, not the person's uncommitted state
    assert_eq!(read(&to.join("f1.txt")), "line 1\n");
    assert_eq!(read(&to.join("f2.txt")), "line 2\n");
    assert!(!to.join("leftover.txt").exists() && !to.join("nested").exists());
    assert_eq!(git(&to, &["rev-parse", "HEAD"]), src.head);
    assert_eq!(git(&to, &["branch", "--format=%(refname:short)"]), made.branch, "only the new branch");
    assert_eq!(git(&to, &["status", "--porcelain"]), "");
    // an unchanged source keeps its time: a build sees nothing to do
    assert_eq!(modified(&to.join("f3.txt")), modified(&s.join("f3.txt")));
    // nothing of the checkout's Git settings
    assert!(!read(&to.join(".git/config")).contains("fsmonitor"));
    assert!(!to.join(".git/hooks").exists());
    assert_eq!(crate::workspace::marker(&to).as_deref(), Some("op2"));
    assert!(!unapplied(&s, &to, &made.branch).unwrap(), "nothing to bring: the person's edits are theirs");
    git(&to, &["fsck"]);
    // the checkout is as it was
    assert_eq!(read(&s.join("f1.txt")), "the person's edit\n");
    assert!(!s.join("f2.txt").exists() && s.join("leftover.txt").exists() && s.join("nested/.git").exists());
    // and the two are apart: a build in the workspace is not one in the checkout
    write(&to.join("target/debug/app"), "rebuilt\n");
    assert_eq!(read(&s.join("target/debug/app")), "warm build\n");
}

/// An unreadable test-jail leftover must not discard the usable build cache. A tracked
/// file that cannot be seeded is restored by Git, and a failed artifact leaves no stub.
#[test]
fn unreadable_entries_do_not_make_the_workspace_cold() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("partly-warm");
    let (s, src, _) = setup(&d);
    lived_in(&s);
    let blocked = [s.join("target/jail/work"), s.join("target/debug/unreadable"), s.join("f3.txt")];
    write(&blocked[0].join("hidden"), "inaccessible");
    write(&blocked[1], "inaccessible artifact");
    for path in &blocked {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0)).unwrap();
    }
    assert!(std::fs::read_dir(&blocked[0]).is_err(), "test must run without permission bypass");
    let to = path_for(&d.0.join("ws"), &src, "w2", "partial");
    let result = crate::workspace::create_as(&src, "w2", "partial", &to, "op2", crate::workspace::seed::How::Bytes);
    // Restore source permissions even if creation failed, so the fixture can be removed.
    for (path, mode) in blocked.iter().zip([0o755, 0o644, 0o644]) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let made = result.unwrap();
    assert_eq!(read(&to.join("target/debug/app")), "warm build\n");
    assert_eq!(modified(&to.join("target/debug/app")), modified(&s.join("target/debug/app")));
    assert!(!to.join("target/debug/unreadable").exists());
    assert!(!to.join("target/jail/work/hidden").exists());
    assert_eq!(read(&to.join("f3.txt")), "line 3\n");
    assert_eq!(read(&to.join("f1.txt")), "line 1\n");
    assert_eq!(git(&to, &["status", "--porcelain"]), "");
    assert!(!unapplied(&s, &to, &made.branch).unwrap());
}

/// A checkout that holds sill's workspaces (a home folder kept in Git): the copy leaves itself out.
#[test]
fn a_workspace_inside_the_checkout_does_not_copy_itself() {
    let d = tmp("inside");
    let (s, src, _) = setup(&d);
    write(&s.join("target/debug/app"), "warm build\n");
    let to = s.join("target/workspaces/w2");
    crate::workspace::create_as(&src, "w2", "in", &to, "op2", crate::workspace::seed::How::Bytes).unwrap();
    assert_eq!(read(&to.join("target/debug/app")), "warm build\n");
    assert!(to.join("target/workspaces").exists() && !to.join("target/workspaces/w2").exists());
    assert_eq!(git(&to, &["status", "--porcelain"]), "");
}

/// As the daemon makes one: warm where this filesystem shares file data, cold where it does
/// not, a whole workspace either way.
#[test]
fn a_workspace_is_whole_whether_or_not_the_filesystem_shares() {
    let d = tmp("share");
    let (s, src, _) = setup(&d);
    lived_in(&s);
    let probe = d.0.join("probe");
    std::fs::create_dir_all(&probe).unwrap();
    let shares = crate::workspace::seed::copy(&s, &probe, crate::workspace::seed::How::Share).is_ok();
    let to = path_for(&d.0.join("ws"), &src, "w2", "any");
    let made = create(&src, "w2", "any", &to, "op2").unwrap();
    assert_eq!(to.join("target/debug/app").exists(), shares);
    assert_eq!(read(&to.join("f1.txt")), "line 1\n");
    assert_eq!(read(&to.join("f2.txt")), "line 2\n");
    assert!(!to.join("leftover.txt").exists());
    assert_eq!(git(&to, &["status", "--porcelain"]), "");
    assert_eq!(crate::workspace::marker(&to).as_deref(), Some("op2"));
    assert!(!unapplied(&s, &to, &made.branch).unwrap());
    git(&to, &["fsck"]);
}

/// A file the person deleted in their checkout, uncommitted, that the agent never touched: not
/// the agent's, so nothing is offered (both sides lacking it is not the agent's content there).
#[test]
fn a_file_the_person_deleted_that_the_agent_never_touched_is_not_offered() {
    let d = tmp("mirror-deleted");
    let (s, _, c) = setup(&d);
    std::fs::remove_file(s.join("f3.txt")).unwrap();
    assert_eq!(pending(&s, &c), Vec::<String>::new());
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
    // the same with a staged edit of another file alongside
    write(&s.join("f2.txt"), "theirs\n");
    git(&s, &["add", "f2.txt"]);
    assert_eq!(pending(&s, &c), Vec::<String>::new());
}

/// The snapshot's tree is what `git add -A` into a private index would stage, on a messy tree:
/// staged and unstaged edits, a staged edit reverted in the file, deletions staged and not, a
/// rename, an untracked nested directory, a link, an exec bit, an ignored file beside a modified
/// tracked one under the ignored pattern, odd names, and an in-repository clean conversion
/// (`ident`), which the files pass through as for a commit.
#[test]
fn a_snapshot_is_what_add_dash_a_would_stage() {
    let d = tmp("snap-parity");
    let (_, _, c) = setup(&d);
    let w = &c.path;
    write(&w.join(".gitattributes"), "*.id ident\n");
    write(&w.join("keys.id"), "$Id$\n");
    write(&w.join("target/tracked.txt"), "tracked under an ignored pattern\n");
    git(w, &["add", "-f", ".gitattributes", "keys.id", "target/tracked.txt"]);
    git(w, &["commit", "-qm", "more"]);
    // the mess
    write(&w.join("f1.txt"), "staged\n");
    git(w, &["add", "f1.txt"]);
    write(&w.join("f1.txt"), "staged then edited again\n");
    write(&w.join("f2.txt"), "staged\n");
    git(w, &["add", "f2.txt"]);
    write(&w.join("f2.txt"), "line 2\n"); // back to HEAD's content in the file
    git(w, &["rm", "-q", "f3.txt"]);
    std::fs::remove_file(w.join("target/tracked.txt")).unwrap();
    git(w, &["mv", "keys.id", "moved.id"]);
    write(&w.join("moved.id"), "$Id: 0123456789abcdef $\n");
    write(&w.join("deep/er/new.txt"), "nested untracked\n");
    write(&w.join("with space.txt"), "odd name\n");
    write(&w.join("ünï.txt"), "odd name\n");
    write(&w.join("target/out.o"), "ignored\n");
    std::os::unix::fs::symlink("f1.txt", w.join("link")).unwrap();
    write(&w.join("run.sh"), "#!/bin/sh\n");
    std::fs::set_permissions(w.join("run.sh"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    write(&w.join("staged-new.txt"), "staged new\n");
    git(w, &["add", "staged-new.txt"]);
    let before = git(w, &["status", "--porcelain"]);
    // the oracle: git's own staging of everything, in a private index
    let index = d.0.join("oracle-index");
    let oracle = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(w).args(args).env("GIT_INDEX_FILE", &index).env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
        assert!(out.status.success(), "oracle git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    oracle(&["read-tree", "HEAD"]);
    oracle(&["add", "-A", "--", ":/"]);
    let expected = oracle(&["write-tree"]);
    let snap = snapshot(w, "x").unwrap();
    assert_eq!(git(w, &["rev-parse", &format!("{snap}^{{tree}}")]), expected, "\n{}\n", git(w, &["diff", "--stat", &expected, &snap]));
    assert_eq!(git(w, &["status", "--porcelain"]), before, "index and files untouched");
    assert_eq!(git(w, &["show", &format!("{snap}:moved.id")]), "$Id$", "the clean conversion ran");
    assert_eq!(git(w, &["ls-tree", &snap, "run.sh"]).split(' ').next().unwrap(), "100755");
    assert_eq!(git(w, &["ls-tree", &snap, "link"]).split(' ').next().unwrap(), "120000");
    assert_eq!(git(w, &["ls-tree", "--name-only", &snap, "target/"]), "", "ignored and deleted alike: nothing under target");
}

/// A look leaves no commit behind, only trees and blobs; prune drops what nothing reaches and is
/// older than an hour, and nothing else: not the fresh look, not the reachable, not the store
/// the clone borrows.
#[test]
fn looks_leave_no_commits_and_prune_drops_their_leavings_after_an_hour() {
    let d = tmp("prune");
    let (s, _, c) = setup(&d);
    let commits = |p: &Path| git(p, &["rev-list", "--all", "--count"]);
    let before = commits(&c.path);
    write(&c.path.join("f1.txt"), "work\n");
    for _ in 0..3 {
        assert!(unapplied(&s, &c.path, &c.branch).unwrap());
    }
    assert_eq!(commits(&c.path), before, "looks make no commits");
    let m = mirror(&s, &c.path, &c.branch).unwrap();
    let loose = |p: &Path| -> Vec<PathBuf> {
        let mut v = Vec::new();
        for dir in std::fs::read_dir(p.join(".git/objects")).unwrap().flatten().filter(|e| e.file_name().len() == 2) {
            v.extend(std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.path()));
        }
        v
    };
    assert!(!loose(&c.path).is_empty(), "the look's trees are in the clone's own store");
    let source_objects = loose(&s).len();
    // everything an hour old: the looks' leavings go, what a ref reaches stays
    let of = |id: &str| c.path.join(".git/objects").join(&id[..2]).join(&id[2..]);
    let age = |p: &Path| std::fs::File::open(p).unwrap().set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600)).unwrap();
    for p in loose(&c.path) {
        age(&p);
    }
    prune(&c.path).unwrap();
    assert!(!of(&m.now).exists(), "an hour-old look's tree goes");
    assert_eq!(loose(&s).len(), source_objects, "the borrowed store is untouched");
    // a fresh look's trees stay
    let m = mirror(&s, &c.path, &c.branch).unwrap();
    prune(&c.path).unwrap();
    assert!(of(&m.now).exists(), "a fresh look's tree stays");
    assert_eq!(git(&c.path, &["cat-file", "-t", &m.tree]), "tree", "the person's side here is HEAD's own tree, borrowed");
    // what the branch reaches stays however old
    write(&c.path.join("f2.txt"), "committed\n");
    git(&c.path, &["commit", "-qam", "agent"]);
    let kept = git(&c.path, &["rev-parse", "HEAD^{tree}"]);
    age(&of(&kept));
    prune(&c.path).unwrap();
    assert!(of(&kept).exists(), "reachable, so kept");
}

#[test]
fn rebased_checkout_does_not_offer_the_same_patch_again() {
    let d = tmp("mirror-rebased");
    let (s, _, c) = setup(&d);
    let base = head(&s);
    write(&c.path.join("f1.txt"), "agent work\n");
    commit(&c, "agent");
    apply_now(&s, &c).unwrap();
    let applied = head(&s);
    git(&s, &["checkout", "-qb", "upstream", &base]);
    write(&s.join("upstream.txt"), "upstream work\n");
    git(&s, &["add", "."]);
    git(&s, &["commit", "-qm", "upstream"]);
    git(&s, &["checkout", "-q", "main"]);
    git(&s, &["rebase", "upstream"]);
    assert_ne!(head(&s), applied);
    assert!(!unapplied(&s, &c.path, &c.branch).unwrap());
    assert!(pending(&s, &c).is_empty());
    // Both uncommitted and committed follow-up work remain visible.
    write(&c.path.join("f2.txt"), "follow-up\n");
    assert_eq!(pending(&s, &c), ["f2.txt"]);
    commit(&c, "follow-up");
    assert_eq!(pending(&s, &c), ["f2.txt"]);
    let m = mirror(&s, &c.path, &c.branch).unwrap();
    assert_eq!(git(&c.path, &["rev-list", "--count", &format!("{}..HEAD", m.upstream)]), "1");
}

#[test]
fn equivalent_later_patch_does_not_hide_an_unmatched_prefix() {
    let d = tmp("mirror-partial-equivalence");
    let (s, _, c) = setup(&d);
    write(&c.path.join("f1.txt"), "not applied\n");
    commit(&c, "first");
    write(&c.path.join("f2.txt"), "already present\n");
    commit(&c, "second");
    let second = git(&c.path, &["rev-parse", "HEAD"]);
    git(&s, &["fetch", c.path.to_str().unwrap(), &c.branch]);
    git(&s, &["cherry-pick", &second]);
    assert!(unapplied(&s, &c.path, &c.branch).unwrap());
    assert_eq!(pending(&s, &c), ["f1.txt", "f2.txt"]);
}

#[test]
fn a_reset_never_writes_head_through_a_link_or_takes_a_name_that_is_not_one() {
    let d = tmp("headlink");
    let (_s, _src, c) = setup(&d);
    let at = git(&c.path, &["rev-parse", "HEAD"]);
    let victim = d.0.join("victim");
    write(&victim, "the person's file\n");
    let head = c.path.join(".git/HEAD");
    let real = read(&head);
    std::fs::remove_file(&head).unwrap();
    std::os::unix::fs::symlink(&victim, &head).unwrap();
    assert!(reset_to(&c.path, &c.branch, &at, &at).is_err(), "a link at HEAD is refused");
    assert_eq!(read(&victim), "the person's file\n");
    std::fs::remove_file(&head).unwrap();
    write(&head, &real);
    assert!(reset_to(&c.path, "x\nref: refs/heads/other", &at, &at).is_err());
    assert_eq!(read(&head), real);
    reset_to(&c.path, &c.branch, &at, &at).unwrap();
    assert_eq!(read(&head), format!("ref: refs/heads/{}\n", c.branch));
}

#[test]
fn a_branch_name_is_the_title_in_snake_case() {
    assert_eq!(super::branch_name("Fix the VM's clock after sleep"), "fix_the_vm_s_clock_after_sleep");
    assert_eq!(super::branch_name("  Ääkköset -- ja ? merkit!! "), "kk_set_ja_merkit");
    assert_eq!(super::branch_name("???"), "sill_work");
    let long = super::branch_name("one two three four five six seven eight nine ten eleven twelve");
    assert_eq!(long, "one_two_three_four_five_six_seven_eight_nine_ten", "cut at a word, 48 at most");
    assert_eq!(super::branch_name(&"x".repeat(60)).len(), 48);
}
