use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn temp() -> Temp {
    static N: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!("sill-crosscheck-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir(&path).unwrap();
    Temp(path)
}
fn git(path: &Path, args: &[&str]) -> String {
    let mut c = Command::new("git");
    c.arg("-C").arg(path).args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "init.defaultBranch=main"]).args(args);
    checked(c, "test git").unwrap()
}
fn source(t: &Temp) -> (PathBuf, String) {
    let path = t.0.join("source");
    std::fs::create_dir(&path).unwrap();
    git(&path, &["init", "-q"]);
    std::fs::write(path.join("changed"), "before\n").unwrap();
    std::fs::write(path.join("deleted"), "gone\n").unwrap();
    std::fs::write(path.join(".gitignore"), "ignored\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-qm", "base"]);
    let base = git(&path, &["rev-parse", "HEAD"]);
    (path, base)
}

#[test]
fn captures_dirty_files_without_touching_source_index_head_or_config() {
    let t = temp();
    let (source, base) = source(&t);
    std::fs::write(source.join("changed"), "staged\n").unwrap();
    git(&source, &["add", "changed"]);
    std::fs::write(source.join("changed"), "actual work\n").unwrap();
    std::fs::remove_file(source.join("deleted")).unwrap();
    std::fs::write(source.join("untracked"), "new\n").unwrap();
    std::fs::write(source.join("ignored"), "build output\n").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", source.join("link")).unwrap();
    let index = std::fs::read(source.join(".git/index")).unwrap();
    let head = std::fs::read(source.join(".git/HEAD")).unwrap();
    let config = std::fs::read(source.join(".git/config")).unwrap();
    let status = git(&source, &["status", "--porcelain"]);
    let s = capture(&source, &base, &t.0.join("snapshot")).unwrap();
    assert_eq!(s.tip, base);
    assert_eq!(s.tree, git(&s.path, &["rev-parse", "HEAD^{tree}"]));
    assert_eq!(std::fs::read_to_string(s.path.join("changed")).unwrap(), "actual work\n");
    assert_eq!(std::fs::read_to_string(s.path.join("untracked")).unwrap(), "new\n");
    assert!(!s.path.join("deleted").exists());
    assert!(!s.path.join("ignored").exists());
    assert_eq!(std::fs::read_link(s.path.join("link")).unwrap(), Path::new("/etc/passwd"));
    assert_eq!(std::fs::read(source.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(source.join(".git/HEAD")).unwrap(), head);
    assert_eq!(std::fs::read(source.join(".git/config")).unwrap(), config);
    assert_eq!(git(&source, &["status", "--porcelain"]), status);
    assert_eq!(git(&s.path, &["show", &format!("{}:changed", s.base)]), "before");
}

#[test]
fn reviewers_have_independent_objects_files_and_metadata() {
    let t = temp();
    let (source, base) = source(&t);
    std::fs::write(source.join("changed"), "review me\n").unwrap();
    let s = capture(&source, &base, &t.0.join("snapshot")).unwrap();
    let a = reviewer_copy(&s, &t.0.join("reviewer-a")).unwrap();
    let b = reviewer_copy(&s, &t.0.join("reviewer-b")).unwrap();
    for path in [&s.path, &a, &b] {
        assert!(!path.join(".git/objects/info/alternates").exists());
        assert!(git(path, &["remote"]).is_empty());
        assert!(git(path, &["status", "--porcelain"]).is_empty());
        git(path, &["fsck", "--full"]);
    }
    std::fs::remove_dir_all(&source).unwrap();
    std::fs::write(a.join("changed"), "reviewer mutation\n").unwrap();
    git(&a, &["add", "."]);
    git(&a, &["commit", "-qm", "reproduction"]);
    assert_eq!(std::fs::read_to_string(b.join("changed")).unwrap(), "review me\n");
    assert_eq!(git(&b, &["rev-parse", "HEAD"]), s.commit);
    assert_eq!(git(&s.path, &["rev-parse", "HEAD"]), s.commit);
    git(&b, &["fsck", "--full"]);
}

#[test]
fn changes_during_capture_are_refused_and_output_is_not_created() {
    let t = temp();
    let (source, base) = source(&t);
    let output = t.0.join("snapshot");
    let err = capture_with(&source, &base, &output, || {
        std::fs::write(source.join("changed"), "raced\n").unwrap();
    })
    .unwrap_err();
    assert!(err.to_string().contains("changed during"));
    assert!(!output.exists());
}

#[test]
fn existing_output_is_preserved_and_option_like_base_is_refused() {
    let t = temp();
    let (source, base) = source(&t);
    let output = t.0.join("snapshot");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("keep"), "existing").unwrap();
    assert!(capture(&source, &base, &output).is_err());
    assert_eq!(std::fs::read_to_string(output.join("keep")).unwrap(), "existing");
    assert!(capture(&source, "--help", &t.0.join("other")).is_err());
    assert!(capture(&source, &base, &source.join("nested")).is_err());
    let alias = t.0.join("alias");
    std::os::unix::fs::symlink(&source, &alias).unwrap();
    assert!(capture(&source, &base, &alias.join("nested")).is_err());
}

#[test]
fn borrowed_workspace_is_copied_without_relying_on_its_source() {
    let t = temp();
    let (source, base) = source(&t);
    let workspace = t.0.join("workspace");
    git(&t.0, &["clone", "-q", "--shared", source.to_str().unwrap(), workspace.to_str().unwrap()]);
    std::fs::write(workspace.join("changed"), "committed\n").unwrap();
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-qm", "agent change"]);
    std::fs::write(workspace.join("untracked"), "pending\n").unwrap();
    let s = capture(&workspace, &base, &t.0.join("snapshot")).unwrap();
    assert!(!s.path.join(".git/objects/info/alternates").exists());
    std::fs::remove_dir_all(&source).unwrap();
    std::fs::remove_dir_all(&workspace).unwrap();
    git(&s.path, &["fsck", "--full"]);
    assert_eq!(git(&s.path, &["show", &format!("{base}:changed")]), "before");
    assert_eq!(std::fs::read_to_string(s.path.join("changed")).unwrap(), "committed\n");
    assert_eq!(std::fs::read_to_string(s.path.join("untracked")).unwrap(), "pending\n");
}

#[test]
fn source_hooks_and_fsmonitor_are_not_executed_or_copied() {
    let t = temp();
    let (source, base) = source(&t);
    let trap = t.0.join("executed");
    let script = t.0.join("hook");
    std::fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", trap.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(&source, &["config", "core.fsmonitor", script.to_str().unwrap()]);
    git(&source, &["config", "core.hooksPath", t.0.to_str().unwrap()]);
    std::fs::write(source.join("changed"), "pending\n").unwrap();
    let s = capture(&source, &base, &t.0.join("snapshot")).unwrap();
    assert!(!trap.exists());
    let config = std::fs::read_to_string(s.path.join(".git/config")).unwrap();
    assert!(!config.contains("fsmonitor"));
    assert!(!config.contains("hooksPath"));
    assert_eq!(std::fs::metadata(&s.path).unwrap().permissions().mode() & 0o777, 0o700);
}
