//! Immutable attachment objects. Canonical bytes stay in the daemon database; jail-visible
//! copies are materialized from them, so a jail write cannot change retry identity.
use std::path::Path;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use sill_core::{acp, store::Store};
use sill_proto::{rpc::Error, sfp::{Upload, UploadCreate, UploadCreated, UploadGet}};

pub const MAX_UPLOAD_BYTES: usize = 1024 * 1024;
/// The image types models accept as image blocks; other images (SVG, TIFF, HEIC…) go as files.
const IMAGE_BLOCK_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
fn domain(code: &str, message: &str) -> anyhow::Error { Error::domain(code, message).into() }
fn initialize(store: &Store) -> Result<()> {
    store.conn().execute_batch("CREATE TABLE IF NOT EXISTS sfp_uploads (
        session_id TEXT NOT NULL, upload_id TEXT NOT NULL, name TEXT NOT NULL,
        mime TEXT NOT NULL, bytes BLOB NOT NULL, PRIMARY KEY(session_id, upload_id));
        CREATE TRIGGER IF NOT EXISTS sfp_uploads_delete AFTER DELETE ON sessions BEGIN
        DELETE FROM sfp_uploads WHERE session_id=OLD.id; END;")?;
    Ok(())
}
fn session(store: &Store, id: &str) -> Result<sill_core::store::Session> {
    store.sessions()?.into_iter().find(|s| s.id == id).ok_or_else(|| domain("not_found", "session not found"))
}

pub(super) fn create(store: &Store, request: UploadCreate) -> Result<UploadCreated> {
    session(store, &request.session_id)?;
    if request.upload_id.is_empty() || request.upload_id.len() > 128 || request.upload_id.chars().any(char::is_control) {
        return Err(Error::invalid_params("upload_id must contain 1–128 bytes without control characters").into());
    }
    if request.name.is_empty() || request.name.len() > 255 || request.name.chars().any(char::is_control)
        || Path::new(&request.name).file_name().and_then(|n|n.to_str()) != Some(request.name.as_str())
        || matches!(request.name.as_str(), "." | "..") {
        return Err(Error::invalid_params("invalid attachment name").into());
    }
    if request.mime.is_empty() || request.mime.len() > 256 || request.mime.chars().any(char::is_control) {
        return Err(Error::invalid_params("invalid attachment MIME type").into());
    }
    let bytes = decode(&request.data_base64)?;
    initialize(store)?;
    let old: Option<(String,String,Vec<u8>)> = store.conn().query_row(
        "SELECT name,mime,bytes FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2",
        params![request.session_id, request.upload_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((name,mime,old_bytes)) = old {
        if name != request.name || mime != request.mime || bytes != old_bytes { return Err(domain("id_conflict", "upload ID already identifies different input")); }
    } else {
        store.conn().execute("INSERT INTO sfp_uploads(session_id,upload_id,name,mime,bytes) VALUES (?1,?2,?3,?4,?5)",
            params![request.session_id,request.upload_id,request.name,request.mime,bytes])?;
    }
    Ok(UploadCreated { upload_id:request.upload_id })
}

pub(super) fn get(store: &Store, request: UploadGet) -> Result<Upload> {
    session(store, &request.session_id)?;
    initialize(store)?;
    let object: Option<(String,String,Vec<u8>)> = store.conn().query_row("SELECT name,mime,bytes FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2",
        params![request.session_id,request.upload_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let (name,mime,bytes)=object.ok_or_else(||domain("not_found","upload not found in this session"))?;
    Ok(Upload { upload_id: request.upload_id, name, mime, data_base64: acp::base64(&bytes) })
}

/// An inline attachment becomes an upload named by its submission and position, so the journal
/// references it and a retry of the same submission finds the same object.
pub(super) fn from_inline(store: &Store, session_id: &str, submission_id: &str, index: usize, name: &str, mime: &str, data_base64: &str) -> Result<(String, String)> {
    let upload_id = truncate(format!("inline-{index}-{submission_id}"), 128);
    // inline names are labels ("screenshot-1.png (1180×518)"); an upload's is a file name
    let name = truncate(name.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect(), 255);
    let name = if name.is_empty() || name == "." || name == ".." { "image".to_string() } else { name };
    create(store, UploadCreate { session_id: session_id.into(), upload_id: upload_id.clone(), name: name.clone(), mime: mime.into(), data_base64: data_base64.into() })?;
    Ok((upload_id, name))
}

fn image_block(mime: &str, images: bool) -> bool { images && IMAGE_BLOCK_MIMES.contains(&mime) }

/// At most `max` bytes, cut at a character boundary (`create` limits bytes, not characters).
fn truncate(mut s: String, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) { end -= 1; }
    s.truncate(end);
    s
}

/// Verify the reference without filesystem writes before accepting a submission.
pub(super) fn validate(store: &Store, session_id: &str, upload_id: &str, name: &str, mime: &str) -> Result<()> {
    session(store,session_id)?;
    initialize(store)?;
    let metadata: Option<(String,String)> = store.conn().query_row("SELECT name,mime FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2",
        params![session_id,upload_id], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let metadata=metadata.ok_or_else(||domain("not_found","upload not found in this session"))?;
    if metadata != (name.into(),mime.into()) { return Err(Error::invalid_params("attachment metadata differs from immutable upload").into()); }
    Ok(())
}

/// Materialize a new private copy for this dispatch. Do not use jail-modifiable bytes as the
/// canonical object. The prepared command persists this chosen path for same-attempt recovery.
/// An image the agent can take (`images`: it said so in `initialize`) goes as an image block, as it
/// did when it was inline; anything else is a staged file.
pub(super) fn resolve(store: &Store, session_id: &str, upload_id: &str, images: bool) -> Result<acp::Attachment> {
    let row=session(store,session_id)?;
    initialize(store)?;
    let object: Option<(String,String,Vec<u8>)> = store.conn().query_row("SELECT name,mime,bytes FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2",
        params![session_id,upload_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let (name,mime,bytes)=object.ok_or_else(||domain("not_found","upload not found in this session"))?;
    if image_block(&mime, images) {
        return Ok(acp::Attachment{name,mime,kind:acp::AttachmentKind::Image{data_base64:acp::base64(&bytes)}});
    }
    let home=std::path::PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let policy=sill_core::session::policy_of(&row,&home)?;
    let state=sill_core::session::state_dir(rustix::process::getuid().as_raw(),session_id)?;
    let (host,jail)=sill_core::session::uploads_dir(&policy,&state,&home).ok_or_else(||domain("unsupported","policy has no private uploads directory"))?;
    let file=super::sfp::id();
    let path=host.join(&file);
    sill_core::safe_fs::write_file_beneath(&state,path.strip_prefix(&state)?,&bytes,0o644)?;
    Ok(acp::Attachment{name,mime,kind:acp::AttachmentKind::File{jail_path:jail.join(file)}})
}

pub(super) fn decode(text: &str) -> Result<Vec<u8>> {
    if text.len() > MAX_UPLOAD_BYTES.div_ceil(3)*4 { return Err(domain("limit_exceeded","upload exceeds 1 MiB decoded limit")); }
    if text.len()%4 != 0 { return Err(Error::invalid_params("base64 must use canonical padding").into()); }
    let value=|b:u8|->Result<u8> { match b { b'A'..=b'Z'=>Ok(b-b'A'), b'a'..=b'z'=>Ok(b-b'a'+26), b'0'..=b'9'=>Ok(b-b'0'+52), b'+'=>Ok(62), b'/'=>Ok(63), _=>Err(Error::invalid_params("invalid base64").into()) } };
    let mut out=Vec::with_capacity(text.len()/4*3);
    let groups=text.as_bytes().chunks_exact(4);
    let count=groups.len();
    for (i,c) in groups.enumerate() {
        let a=value(c[0])?; let b=value(c[1])?;
        out.push((a<<2)|(b>>4));
        if c[2]==b'=' {
            if c[3]!=b'=' || i+1!=count || b&15!=0 { return Err(Error::invalid_params("invalid base64 padding").into()); }
        } else {
            let d=value(c[2])?; out.push((b<<4)|(d>>2));
            if c[3]==b'=' {
                if i+1!=count || d&3!=0 { return Err(Error::invalid_params("invalid base64 padding").into()); }
            } else { out.push((d<<6)|value(c[3])?); }
        }
    }
    if out.len()>MAX_UPLOAD_BYTES { return Err(domain("limit_exceeded","upload exceeds 1 MiB decoded limit")); }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_images_the_agent_and_model_take_become_image_blocks() {
        assert!(image_block("image/png", true) && image_block("image/webp", true));
        assert!(!image_block("image/svg+xml", true) && !image_block("image/heic", true), "staged as files");
        assert!(!image_block("image/png", false), "an agent without image support gets a file");
    }
    #[test]
    fn truncation_counts_bytes_at_character_boundaries() {
        assert_eq!(truncate("ää".into(), 3), "ä");
        assert_eq!(truncate("abc".into(), 128), "abc");
    }
    #[test]
    fn upload_retry_is_immutable_scoped_and_removed_with_session() {
        let root=std::env::temp_dir().join(format!("sill-upload-test-{}-{}",std::process::id(),super::super::sfp::id()));
        let store=Store::open(&root.join("store.db")).unwrap();
        let session=store.create_with("test","default",false).unwrap();
        let mut request=UploadCreate{session_id:session.id.clone(),upload_id:"upload".into(),name:"notes.txt".into(),mime:"text/plain".into(),data_base64:acp::base64(b"hello")};
        assert_eq!(create(&store,request.clone()).unwrap(),create(&store,request.clone()).unwrap());
        validate(&store,&session.id,"upload","notes.txt","text/plain").unwrap();
        assert!(validate(&store,&session.id,"upload","other.txt","text/plain").is_err());
        request.data_base64=acp::base64(b"changed");
        assert_eq!(create(&store,request.clone()).unwrap_err().downcast_ref::<Error>().unwrap().data.as_ref().unwrap()["code"],"id_conflict");
        request.session_id="missing".into(); assert!(create(&store,request).is_err());
        store.delete(&session.id).unwrap();
        assert_eq!(store.conn().query_row("SELECT count(*) FROM sfp_uploads",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        drop(store); std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn base64_decoding_enforces_padding_and_decoded_bounds() {
        for bytes in [b"".as_slice(),b"a",b"ab",b"abc",b"hello world"] { assert_eq!(decode(&acp::base64(bytes)).unwrap(),bytes); }
        for bad in ["a", "=AAA", "AA=A", "AB==", "AAB=", "AA==AAAA", "AA\n="] { assert!(decode(bad).is_err(),"{bad}"); }
        assert!(decode(&acp::base64(&vec![0;MAX_UPLOAD_BYTES+1])).is_err());
    }
}
