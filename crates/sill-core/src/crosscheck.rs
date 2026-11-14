//! Immutable inputs for Crosscheck and disposable, independent reviewer repositories.
//!
//! Capture uses the workspace's hardened Git view and private-index snapshot machinery. It
//! writes objects, but never changes the producing repository's HEAD, index or working files.
//! Review repositories contain their own packed objects: no alternates, hardlinks, remotes or
//! source configuration, so a reviewer cannot write back through shared Git metadata.
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::changes::Git;
use crate::workspace::flow;

/// The immutable repository retained with a Crosscheck report. `commit` is a synthetic commit
/// of the actual proposed files; `tip` is the producing workspace's original HEAD.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub path: PathBuf,
    pub base: String,
    pub tip: String,
    pub tree: String,
    pub commit: String,
}

/// Capture all proposed work, including nonignored untracked files, against `base`: where the
/// work leaves the person's checkout (`workspace::flow::Mirror::upstream`), so that a review
/// covers what an Apply would bring and nothing else. (2026-11-13: against the checkout's tip, a
/// Reviewer was shown commits the checkout had gained from other sessions as removed by this
/// work, and applied work as no change.) `output` must be a new absolute directory outside the producing repository.
/// A changing HEAD or working tree refuses capture; callers can ask the person to retry.
pub fn capture(workspace: &Path, base: &str, output: &Path) -> Result<Snapshot> {
    capture_with(workspace, base, output, || {})
}

fn capture_with(workspace: &Path, base: &str, output: &Path, between: impl FnOnce()) -> Result<Snapshot> {
    let workspace = workspace.canonicalize().context("resolve Crosscheck workspace")?;
    outside(&workspace, output)?;
    ensure!(valid_oid(base), "Crosscheck base must be a full Git object id");
    let git = Git::open(&workspace)?;
    let base = rev(&git, &format!("{base}^{{commit}}"))?;
    let tip = rev(&git, "HEAD^{commit}")?;
    let tree = flow::state_tree(&workspace)?;
    let commit = flow::snapshot(&workspace, "Crosscheck input snapshot")?;
    between();
    let captured_tree = rev(&git, &format!("{commit}^{{tree}}"))?;
    ensure!(
        rev(&git, "HEAD^{commit}")? == tip && flow::state_tree(&workspace)? == tree && captured_tree == tree,
        "The workspace changed during Crosscheck capture; retry when its work has stopped"
    );
    independent_copy(&workspace, &base, &commit, output)?;
    Ok(Snapshot { path: output.to_path_buf(), base, tip, tree, commit })
}

/// Make a disposable writable repository from a captured input. Every reviewer and every
/// verification context receives a separate copy. The retained snapshot itself is never a pin.
pub fn reviewer_copy(snapshot: &Snapshot, output: &Path) -> Result<PathBuf> {
    outside(&snapshot.path, output)?;
    independent_copy(&snapshot.path, &snapshot.base, &snapshot.commit, output)?;
    Ok(output.to_path_buf())
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

fn outside(source: &Path, output: &Path) -> Result<()> {
    ensure!(output.is_absolute(), "Crosscheck output must be an absolute path");
    ensure!(!output.components().any(|c| c == std::path::Component::ParentDir), "Crosscheck output must not contain parent components");
    let ancestor = output.ancestors().find(|p| p.symlink_metadata().is_ok()).context("Crosscheck output has no existing ancestor")?;
    let resolved = ancestor.canonicalize()?.join(output.strip_prefix(ancestor)?);
    ensure!(!resolved.starts_with(source.canonicalize()?), "Crosscheck output must be outside the workspace or retained snapshot");
    Ok(())
}

fn rev(git: &Git, revision: &str) -> Result<String> {
    let mut c = git.cmd();
    c.args(["rev-parse", "--verify", revision]);
    checked(c, "read Crosscheck revision")
}

fn checked(mut c: Command, what: &str) -> Result<String> {
    c.env("LC_ALL", "C");
    let out = c.output().with_context(|| what.to_string())?;
    if !out.status.success() {
        bail!("{what}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn independent_copy(source: &Path, base: &str, commit: &str, output: &Path) -> Result<()> {
    ensure!(valid_oid(base) && valid_oid(commit), "Invalid Crosscheck revision");
    let source_git = Git::open(source)?;
    std::fs::create_dir_all(output.parent().context("Crosscheck output has no parent")?)?;
    // create_new semantics: never remove or overwrite a preexisting result, even on failure.
    std::fs::DirBuilder::new().mode(0o700).create(output).context("create Crosscheck repository")?;
    let copied = (|| -> Result<()> {
        let format = if commit.len() == 64 { "sha256" } else { "sha1" };
        let mut init = Command::new("git");
        init.args(["init", "-q", "--template=", "--initial-branch=crosscheck"])
            .arg(format!("--object-format={format}"))
            .arg(output)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE");
        checked(init, "initialize Crosscheck repository")?;
        // Packing named revisions reads even unreachable snapshot objects. No temporary ref is
        // needed in the source, and no inode or source path is shared with the new repository.
        let mut pack = source_git.cmd();
        pack.args(["-c", "pack.threads=1", "pack-objects", "--revs"])
            .arg(output.join(".git/objects/pack/pack"))
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = pack.spawn().context("pack Crosscheck objects")?;
        {
            let mut stdin = child.stdin.take().context("Crosscheck pack stdin")?;
            writeln!(stdin, "{base}\n{commit}")?;
        }
        let packed = child.wait_with_output()?;
        ensure!(packed.status.success(), "Pack Crosscheck objects: {}", String::from_utf8_lossy(&packed.stderr).trim());
        if let Ok(shallow) = std::fs::read(source_git.dirs().1.join("shallow")) {
            std::fs::write(output.join(".git/shallow"), shallow)?;
        }
        let target = Git::open(output)?;
        let mut c = target.cmd_write();
        c.args(["update-ref", "refs/heads/crosscheck", commit]);
        checked(c, "retain Crosscheck result")?;
        let mut c = target.cmd_write();
        c.args(["update-ref", "refs/crosscheck/base", base]);
        checked(c, "retain Crosscheck base")?;
        let mut c = target.cmd_write();
        c.args(["read-tree", "--reset", "-u", commit]);
        checked(c, "check out Crosscheck files")?;
        Ok(())
    })();
    if copied.is_err() {
        let _ = std::fs::remove_dir_all(output);
    }
    copied
}

#[cfg(test)]
mod tests;
