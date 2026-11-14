use super::*;
use sill_core::store::Store;

struct Fixture { root:std::path::PathBuf, store:Store, session:String }
impl Fixture {
    fn new() -> Self {
        let root=std::env::temp_dir().join(format!("sill-snapshot-test-{}-{}",std::process::id(),id()));
        let store=Store::open(&root.join("store.db")).unwrap();
        let session=store.create_with("test","default",false).unwrap().id;
        Journal::new(&store).unwrap().ensure(&session,"generation").unwrap();
        Self{root,store,session}
    }
    fn note(&self, index:usize)->Event {
        Event::ItemCreated{item:Item{item_id:format!("item-{index}"),created_seq:String::new(),turn_id:None,submission_id:None,
            agent_generation_id:"generation".into(),kind:"note".into(),content:vec![Content::text("original")],tool:None}}
    }
}
impl Drop for Fixture {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.root);}}

#[test]
fn snapshot_pages_are_immutable_while_journal_advances() {
    let f=Fixture::new();let journal=Journal::new(&f.store).unwrap();
    journal.append(&f.session,None,&(0..130).map(|i|f.note(i)).collect::<Vec<_>>()).unwrap();
    let mut subscriptions=Subscriptions::default();
    let Attached::Snapshot{snapshot:lease,..}=snapshot(&journal,1,&f.session,"sub",&mut subscriptions).unwrap() else {panic!("snapshot")};
    let saved=&subscriptions.snapshots[&lease.token];
    assert!(saved.page_count>=2,"bounded pages");
    fn read(saved: &Snapshot) -> Vec<Record> {
        let mut decoder=SnapshotDecoder::default(); let mut records=vec![];
        for i in 0..saved.page_count { records.extend(decoder.records(&saved.page(i).unwrap()).unwrap()); }
        records
    }
    let original=read(saved);
    journal.append(&f.session,None,&[Event::ItemTextAppended{item_id:"item-0".into(),block_id:"text".into(),text:" changed".into()}]).unwrap();
    let saved=&subscriptions.snapshots[&lease.token];
    assert_eq!(saved.at,lease.at);
    assert_eq!(read(saved),original);
    assert_ne!(journal.cursor(&f.session).unwrap(),lease.at);
}

#[test]
fn snapshot_leases_are_removed_only_for_the_detached_connection_and_session() {
    let f=Fixture::new();let journal=Journal::new(&f.store).unwrap();
    let mut all=Subscriptions::default();
    let token=|reply:Attached|match reply{Attached::Snapshot{snapshot,..}=>snapshot.token,_=>panic!("snapshot")};
    let first=token(snapshot(&journal,1,&f.session,"one",&mut all).unwrap());
    let second=token(snapshot(&journal,2,&f.session,"two",&mut all).unwrap());
    all.remove(1,&f.session);
    assert!(!all.snapshots.contains_key(&first));
    assert!(all.snapshots.contains_key(&second));
    all.disconnect(2);assert!(all.snapshots.is_empty());
}

#[test]
fn cursor_domain_errors_survive_rpc_translation() {
    let f=Fixture::new();let journal=Journal::new(&f.store).unwrap();
    for cursor in [Cursor{session_id:"foreign".into(),seq:"0".into()},Cursor{session_id:f.session.clone(),seq:"1".into()}] {
        let error=failure(journal.replay(&f.session,&cursor,100).unwrap_err());
        assert_eq!(error.data.unwrap()["code"],"invalid_cursor");
    }
}

#[test]
fn large_snapshots_and_oversized_records_are_paged_without_losing_bytes() {
    let f=Fixture::new(); let j=Journal::new(&f.store).unwrap();
    let text="🦀\"\n".repeat(180_000);
    for i in 0..25 {
        let Event::ItemCreated{mut item}=f.note(i) else {unreachable!()};
        item.content=vec![Content::text(&text)];
        j.append(&f.session,None,&[Event::ItemCreated{item}]).unwrap();
    }
    let mut all=Subscriptions::default();
    let Attached::Snapshot{snapshot:lease,..}=snapshot(&j,1,&f.session,"large",&mut all).unwrap() else {panic!()};
    let saved=&all.snapshots[&lease.token]; assert!(saved.bytes>32*1024*1024);
    let mut decoder=SnapshotDecoder::default(); let mut count=0;
    for index in 0..saved.page_count {
        let page=saved.page(index).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len()<4*1024*1024);
        for record in decoder.records(&page).unwrap() {
            if let Record::Item{item}=record {assert_eq!(item.content[0].text.as_deref(),Some(text.as_str()));count+=1;}
        }
    }
    assert_eq!(count,25);
}

#[test]
fn snapshot_disk_leases_expire_without_another_attach() {
    let f=Fixture::new();let j=Journal::new(&f.store).unwrap();let mut all=Subscriptions::default();
    let Attached::Snapshot{snapshot:lease,..}=snapshot(&j,1,&f.session,"sub",&mut all).unwrap() else{panic!()};
    all.snapshots.get_mut(&lease.token).unwrap().expires=Instant::now()-Duration::from_secs(1);
    all.expire();assert!(all.snapshots.is_empty());
}

#[test]
fn replacement_capture_counts_old_lease_and_failure_preserves_it() {
    let f=Fixture::new();let j=Journal::new(&f.store).unwrap();let mut all=Subscriptions::default();
    let token=|reply:Attached|match reply{Attached::Snapshot{snapshot,..}=>snapshot.token,_=>panic!("snapshot")};
    let old=token(snapshot(&j,1,&f.session,"old",&mut all).unwrap());
    let other=token(snapshot(&j,2,&f.session,"other",&mut all).unwrap());
    let original=all.snapshots[&old].page(0).unwrap();
    // Simulate two full leases without allocating two GiB for an accounting regression.
    all.snapshots.get_mut(&old).unwrap().bytes=MAX_SNAPSHOT_BYTES;
    all.snapshots.get_mut(&other).unwrap().bytes=MAX_SNAPSHOT_BYTES;
    let error=snapshot(&j,1,&f.session,"replacement",&mut all).unwrap_err();
    assert_eq!(error.data.unwrap()["code"],"limit_exceeded");
    assert_eq!(all.snapshots.len(),2);
    assert_eq!(serde_json::to_value(all.snapshots[&old].page(0).unwrap()).unwrap(),serde_json::to_value(original).unwrap());
    all.remove(1,&f.session);
    snapshot(&j,1,&f.session,"replacement",&mut all).unwrap();
}
