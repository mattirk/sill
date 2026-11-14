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
    assert!(saved.pages.len()>=2,"bounded pages");
    let original=saved.records.clone();
    journal.append(&f.session,None,&[Event::ItemTextAppended{item_id:"item-0".into(),block_id:"text".into(),text:" changed".into()}]).unwrap();
    let saved=&subscriptions.snapshots[&lease.token];
    assert_eq!(saved.at,lease.at);
    assert_eq!(saved.records,original);
    let mut start=0;let mut assembled=vec![];
    loop {
        let end=saved.pages[&start];
        assembled.extend_from_slice(&saved.records[start..end]);
        if end==saved.records.len(){break;} start=end;
    }
    assert_eq!(assembled,original);
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
