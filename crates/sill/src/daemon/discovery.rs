use anyhow::Result;
use serde_json::Value;
use sill_proto::{discovery as api, sessions::HostPath, rpc::Error};
use std::path::PathBuf;
fn args<T: serde::de::DeserializeOwned>(p: Value) -> Result<T> {serde_json::from_value(p).map_err(|e|Error::invalid_params(e).into())}
fn path(p: &HostPath) -> Result<PathBuf> {p.to_path().map_err(|e|Error::invalid_params(e).into())}
fn value(v: impl serde::Serialize) -> Result<Value> {let v=serde_json::to_value(v)?;if serde_json::to_vec(&v)?.len()>4*1024*1024{return Err(Error::domain("limit_exceeded","result exceeds 4 MiB").into());}Ok(v)}
pub(super) fn handle(method: &str, params: Value) -> Option<Result<Value>> {
    if !matches!(method,"_sill/files.find"|"_sill/repos.root"|"_sill/repos.head"|"_sill/repos.status"|"_sill/repos.changed_lines"|"_sill/repos.summary") {return None;}
    Some((||match method {
        "_sill/files.find"=>{
            let a:api::Find=args(params)?;let n=a.limit.unwrap_or(128) as usize;let visits=a.visits.unwrap_or(10000) as usize;
            if a.bases.len()>128||a.query.len()>1024||n==0||n>1000||visits==0||visits>100000{return Err(Error::invalid_params("at most 128 bases, 1024 query bytes, 1–1000 results and 1–100000 visits").into());}
            let bases=a.bases.iter().map(path).collect::<Result<Vec<_>>>()?;
            let (paths,truncated)=super::frontend::find_files(&bases,&a.query.to_lowercase(),n,visits);
            value(api::Found{paths:paths.iter().map(|p|HostPath::from_path(p)).collect(),truncated})
        }
        "_sill/repos.root"=>{let a:api::PathKey=args(params)?;value(api::Root{path:sill_core::changes::repo_root(&path(&a.path)?).map(|p|HostPath::from_path(&p))})}
        "_sill/repos.head"=>{let a:api::PathKey=args(params)?;value(api::HeadResult{head:super::frontend::head_label(&path(&a.path)?).map(|(label,detached)|api::Head{label,detached})})}
        "_sill/repos.summary"=>{let a:api::PathKey=args(params)?;value(api::SummaryResult{summary:super::frontend::git_summary(&path(&a.path)?).map(|(a,b,c)|api::Summary{insertions:a.to_string(),deletions:b.to_string(),untracked:c.to_string()})})}
        "_sill/repos.changed_lines"=>{
            let a:api::ChangedLines=args(params)?;let repo=path(&a.repo)?;let p=path(&a.path)?;
            let rel=p.strip_prefix(&repo).map_err(Error::invalid_params)?;
            if rel.components().any(|c|!matches!(c,std::path::Component::Normal(_))){return Err(Error::invalid_params("path must be below repository").into());}
            value(api::Lines{ranges:sill_core::changes::git_changed_lines(&repo,rel).map(|r|r.into_iter().map(|(a,b)|api::LineRange{start:a.to_string(),end:b.to_string()}).collect())})
        }
        "_sill/repos.status"=>{
            use std::os::unix::ffi::OsStrExt;
            let a:api::Status=args(params)?;let n=a.limit.unwrap_or(64) as usize;
            if !(1..=128).contains(&n){return Err(Error::invalid_params("limit must be 1–128").into());}
            let repo=path(&a.repo)?;let after=a.after.as_ref().map(path).transpose()?;
            if after.as_ref().is_some_and(|p|!p.starts_with(&repo)){return Err(Error::domain("invalid_cursor","cursor is outside repository").into());}
            let mut entries=sill_core::changes::git_status(&repo).into_iter().map(|(p,status)|(repo.join(p),status)).collect::<Vec<_>>();
            entries.sort_by(|a,b|a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));
            entries.retain(|(p,_)|after.as_ref().is_none_or(|a|p.as_os_str().as_bytes()>a.as_os_str().as_bytes()));
            let more=entries.len()>n;entries.truncate(n);
            let next=if more {entries.last().map(|(p,_)|HostPath::from_path(p))}else{None};
            value(api::StatusPage{entries:entries.into_iter().map(|(p,status)|api::StatusEntry{path:HostPath::from_path(&p),status}).collect(),next})
        }
        _=>unreachable!(),
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_preserves_exact_paths_and_bounds_status_pages() {
        use std::os::unix::ffi::OsStringExt;
        let dir=std::env::temp_dir().join(format!("sill-discovery-{}",crate::daemon::sfp::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(["init","-q"]).status().unwrap().success());
        let a=dir.join(std::ffi::OsString::from_vec(b"file\xff".to_vec()));
        let b=dir.join("file\u{fffd}");
        std::fs::write(&a,b"a").unwrap();std::fs::write(&b,b"b").unwrap();
        let invoke=|method:&str,params:Value|handle(method,params).unwrap().unwrap();
        let found:api::Found=serde_json::from_value(invoke("_sill/files.find",serde_json::json!({"bases":[HostPath::from_path(&dir)],"query":"FILE","limit":10,"visits":100}))).unwrap();
        let paths=found.paths.iter().map(|p|p.to_path().unwrap()).collect::<Vec<_>>();
        assert!(paths.contains(&a)&&paths.contains(&b));
        let first:api::StatusPage=serde_json::from_value(invoke("_sill/repos.status",serde_json::json!({"repo":HostPath::from_path(&dir),"limit":1}))).unwrap();
        assert_eq!(first.entries.len(),1);
        let second:api::StatusPage=serde_json::from_value(invoke("_sill/repos.status",serde_json::json!({"repo":HostPath::from_path(&dir),"limit":1,"after":first.next}))).unwrap();
        assert_eq!(second.entries.len(),1);assert!(second.next.is_none());
        assert_ne!(first.entries[0].path.to_path().unwrap(),second.entries[0].path.to_path().unwrap());
        assert!(handle("_sill/repos.changed_lines",serde_json::json!({"repo":HostPath::from_path(&dir),"path":HostPath::from_path(&dir.join("../outside"))})).unwrap().is_err());
        assert!(handle("_sill/files.find",serde_json::json!({"bases":[],"query":"x","limit":1001})).unwrap().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
