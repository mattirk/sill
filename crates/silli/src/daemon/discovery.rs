use anyhow::Result;
use serde_json::Value;
use silli_proto::method;
use silli_proto::{discovery as api, rpc::Error, sessions::HostPath};
use std::path::PathBuf;
fn args<T: serde::de::DeserializeOwned>(p: Value) -> Result<T> {
    serde_json::from_value(p).map_err(|e| Error::invalid_params(e).into())
}
fn path(p: &HostPath) -> Result<PathBuf> {
    p.to_path().map_err(|e| Error::invalid_params(e).into())
}
fn value(v: impl serde::Serialize) -> Result<Value> {
    let v = serde_json::to_value(v)?;
    if serde_json::to_vec(&v)?.len() > 4 * 1024 * 1024 {
        return Err(silli_proto::rpc::Error::limit(silli_proto::rpc::LimitScope::Result, Some(4194304), "result exceeds 4 MiB").into());
    }
    Ok(v)
}

/// The requests answered here, each named once (`HANDLERS` in hub.rs holds every such list).
pub(super) const METHODS: &[&str] =
    &[method::FILES_FIND, method::REPOS_ROOT, method::REPOS_HEAD, method::REPOS_STATUS, method::REPOS_CHANGED_LINES, method::REPOS_SUMMARY];
pub(super) fn handle(method: &str, params: Value) -> Option<Result<Value>> {
    if !METHODS.contains(&method) {
        return None;
    }
    Some((|| match method {
        method::FILES_FIND => {
            let a: api::Find = args(params)?;
            let n = a.limit.unwrap_or(128) as usize;
            let visits = a.visits.unwrap_or(10000) as usize;
            if a.bases.len() > 128 || a.query.len() > 1024 || n == 0 || n > 1000 || visits == 0 || visits > 100000 {
                return Err(Error::invalid_params("at most 128 bases, 1024 query bytes, 1–1000 results and 1–100000 visits").into());
            }
            let bases = a.bases.iter().map(path).collect::<Result<Vec<_>>>()?;
            let (paths, truncated) = super::frontend::find_files(&bases, &a.query.to_lowercase(), n, visits);
            value(api::Found { paths: paths.iter().map(|p| HostPath::from_path(p)).collect(), truncated })
        }
        method::REPOS_ROOT => {
            let a: api::PathKey = args(params)?;
            value(api::Root { path: silli_core::changes::repo_root(&path(&a.path)?).map(|p| HostPath::from_path(&p)) })
        }
        method::REPOS_HEAD => {
            let a: api::PathKey = args(params)?;
            value(api::HeadResult { head: super::frontend::head_label(&path(&a.path)?).map(|(label, detached)| api::Head { label, detached }) })
        }
        method::REPOS_SUMMARY => {
            let a: api::SummaryKey = args(params)?;
            let n = |n: u64| n.to_string();
            value(api::SummaryResult {
                summary: super::frontend::git_summary(&path(&a.path)?, a.files.unwrap_or(false)).map(|s| api::Summary {
                    insertions: n(s.added),
                    deletions: n(s.removed),
                    untracked: n(s.untracked),
                    truncated: s.files.as_ref().map(|_| s.truncated),
                    files: s.files.map(|f| {
                        f.into_iter()
                            .map(|(p, lines)| api::FileLines {
                                path: HostPath::from_path(&p),
                                lines: lines.map(|(a, r)| api::LineCount { added: n(a), removed: n(r) }),
                            })
                            .collect()
                    }),
                }),
            })
        }
        method::REPOS_CHANGED_LINES => {
            let a: api::ChangedLines = args(params)?;
            let repo = path(&a.repo)?;
            let p = path(&a.path)?;
            let rel = p.strip_prefix(&repo).map_err(Error::invalid_params)?;
            if rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                return Err(Error::invalid_params("path must be below repository").into());
            }
            value(api::Lines {
                ranges: silli_core::changes::git_changed_lines(&repo, rel)
                    .map(|r| r.into_iter().map(|(a, b)| api::LineRange { start: a.to_string(), end: b.to_string() }).collect()),
            })
        }
        method::REPOS_STATUS => {
            use std::os::unix::ffi::OsStrExt;
            let a: api::Status = args(params)?;
            let n = a.limit.unwrap_or(64) as usize;
            if !(1..=128).contains(&n) {
                return Err(Error::invalid_params("limit must be 1–128").into());
            }
            let repo = path(&a.repo)?;
            let after = a.after.as_ref().map(path).transpose()?;
            if after.as_ref().is_some_and(|p| !p.starts_with(&repo)) {
                return Err(Error::domain("invalid_cursor", "cursor is outside repository").into());
            }
            let mut entries = silli_core::changes::git_status(&repo).into_iter().map(|(p, status)| (repo.join(p), status)).collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));
            entries.retain(|(p, _)| after.as_ref().is_none_or(|a| p.as_os_str().as_bytes() > a.as_os_str().as_bytes()));
            let more = entries.len() > n;
            entries.truncate(n);
            let next = if more { entries.last().map(|(p, _)| HostPath::from_path(p)) } else { None };
            value(api::StatusPage {
                entries: entries.into_iter().map(|(p, status)| api::StatusEntry { path: HostPath::from_path(&p), status }).collect(),
                next,
            })
        }
        _ => unreachable!(),
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_preserves_exact_paths_and_bounds_status_pages() {
        use std::os::unix::ffi::OsStringExt;
        let dir = std::env::temp_dir().join(format!("silli-discovery-{}", crate::daemon::sfp::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status().unwrap().success());
        let a = dir.join(std::ffi::OsString::from_vec(b"file\xff".to_vec()));
        let b = dir.join("file\u{fffd}");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let invoke = |method: &str, params: Value| handle(method, params).unwrap().unwrap();
        let found: api::Found =
            serde_json::from_value(invoke(method::FILES_FIND, serde_json::json!({"bases":[HostPath::from_path(&dir)],"query":"FILE","limit":10,"visits":100})))
                .unwrap();
        let paths = found.paths.iter().map(|p| p.to_path().unwrap()).collect::<Vec<_>>();
        assert!(paths.contains(&a) && paths.contains(&b));
        let first: api::StatusPage =
            serde_json::from_value(invoke(method::REPOS_STATUS, serde_json::json!({"repo":HostPath::from_path(&dir),"limit":1}))).unwrap();
        assert_eq!(first.entries.len(), 1);
        let second: api::StatusPage =
            serde_json::from_value(invoke(method::REPOS_STATUS, serde_json::json!({"repo":HostPath::from_path(&dir),"limit":1,"after":first.next}))).unwrap();
        assert_eq!(second.entries.len(), 1);
        assert!(second.next.is_none());
        assert_ne!(first.entries[0].path.to_path().unwrap(), second.entries[0].path.to_path().unwrap());
        assert!(
            handle(method::REPOS_CHANGED_LINES, serde_json::json!({"repo":HostPath::from_path(&dir),"path":HostPath::from_path(&dir.join("../outside"))}))
                .unwrap()
                .is_err()
        );
        assert!(handle(method::FILES_FIND, serde_json::json!({"bases":[],"query":"x","limit":1001})).unwrap().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// The Diff view's rows (docs/silli-gui.md "Diff"): each file's lines from the same numstat as
    /// the totals, counted again at every read — a file that was already changed and is changed
    /// further, a commit that leaves the same files modified.
    #[test]
    fn a_summary_gives_each_changed_file_its_lines_as_they_are_now() {
        use std::os::unix::ffi::OsStringExt;
        let dir = std::env::temp_dir().join(format!("silli-summary-{}", crate::daemon::sfp::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            )
        };
        let odd = dir.join(std::ffi::OsString::from_vec(b"odd \xff\tname".to_vec()));
        let body = (1..=40).map(|n| format!("line {n}\n")).collect::<String>();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("old.txt"), &body).unwrap();
        std::fs::write(&odd, "x\n").unwrap();
        std::fs::write(dir.join("bin"), b"\0\x01").unwrap();
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first"]);
        let read = |files: bool| {
            let r: api::SummaryResult =
                serde_json::from_value(handle(method::REPOS_SUMMARY, serde_json::json!({"path":HostPath::from_path(&dir),"files":files})).unwrap().unwrap())
                    .unwrap();
            let s = r.summary.unwrap();
            let rows = s.files.clone().map(|f| {
                f.into_iter()
                    .map(|f| (f.path.to_path().unwrap().strip_prefix(&dir).unwrap().to_path_buf(), f.lines.map(|l| (l.added, l.removed))))
                    .collect::<Vec<_>>()
            });
            (s, rows)
        };
        let lines = |a: &str, r: &str| Some((a.to_string(), r.to_string()));
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let (s, rows) = read(true);
        assert_eq!(rows.unwrap(), vec![("a.txt".into(), lines("1", "0"))]);
        assert_eq!((s.insertions.as_str(), s.deletions.as_str(), s.truncated), ("1", "0", Some(false)));
        // changed further: the same file, the same letter, other lines
        std::fs::write(dir.join("a.txt"), "one\nthree\nfour\nfive\n").unwrap();
        assert_eq!(read(true).1.unwrap(), vec![("a.txt".into(), lines("3", "1"))]);
        // committed and changed again: against the new HEAD
        git(&["commit", "-qam", "second"]);
        std::fs::write(dir.join("a.txt"), "one\nthree\nfour\n").unwrap();
        assert_eq!(read(true).1.unwrap(), vec![("a.txt".into(), lines("0", "1"))]);
        // a rename is the new path's row, a path of any bytes its own, a binary file without counts,
        // new files counted by their lines: text, without a final newline, binary
        git(&["mv", "old.txt", "new.txt"]);
        std::fs::write(dir.join("new.txt"), body.replace("line 7\n", "line seven\n")).unwrap();
        std::fs::write(&odd, "x\ny\n").unwrap();
        std::fs::write(dir.join("bin"), b"\0\x02").unwrap();
        std::fs::write(dir.join("fresh.txt"), "1\n2\n3").unwrap();
        std::fs::write(dir.join("fresh.bin"), b"a\0b\n").unwrap();
        let (s, rows) = read(true);
        assert_eq!(
            rows.unwrap(),
            vec![
                ("a.txt".into(), lines("0", "1")),
                ("bin".into(), None),
                ("fresh.bin".into(), None),
                ("fresh.txt".into(), lines("3", "0")),
                ("new.txt".into(), lines("1", "1")),
                (odd.strip_prefix(&dir).unwrap().to_path_buf(), lines("1", "0")),
            ]
        );
        assert_eq!((s.insertions.as_str(), s.deletions.as_str(), s.untracked.as_str()), ("2", "2", "2"));
        // the rows are the ones repos.status lists, in its order
        let status: api::StatusPage =
            serde_json::from_value(handle(method::REPOS_STATUS, serde_json::json!({"repo":HostPath::from_path(&dir),"limit":128})).unwrap().unwrap()).unwrap();
        assert_eq!(
            status.entries.iter().map(|e| e.path.to_path().unwrap()).collect::<Vec<_>>(),
            s.files.unwrap().iter().map(|f| f.path.to_path().unwrap()).collect::<Vec<_>>()
        );
        // not asked for: the totals alone, no file read
        let (s, rows) = read(false);
        assert!(rows.is_none() && s.truncated.is_none());
        assert_eq!((s.insertions.as_str(), s.deletions.as_str(), s.untracked.as_str()), ("2", "2", "2"));
        // past the cap: the first files in that order, and said so
        for n in 0..super::super::frontend::COUNTED_FILES {
            std::fs::write(dir.join(format!("z{n:03}")), "z\n").unwrap();
        }
        let (s, rows) = read(true);
        let rows = rows.unwrap();
        assert_eq!((rows.len(), s.truncated), (super::super::frontend::COUNTED_FILES, Some(true)));
        assert_eq!(rows[0].0, std::path::PathBuf::from("a.txt"));
        assert_eq!(rows.last().unwrap(), &("z143".into(), lines("1", "0")));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
