use super::*;
use sill_core::store::Store;

struct Fixture {
    root: std::path::PathBuf,
    store: Store,
    session: String,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("sill-snapshot-test-{}-{}", std::process::id(), id()));
        let store = Store::open(&root.join("store.db")).unwrap();
        let session = store.create_with("test", "default", false).unwrap().id;
        Journal::new(&store).unwrap().ensure(&session, "generation").unwrap();
        Self { root, store, session }
    }
    fn note(&self, index: usize) -> Event {
        Event::ItemCreated {
            item: Item {
                item_id: format!("item-{index}"),
                created_seq: String::new(),
                turn_id: None,
                submission_id: None,
                agent_generation_id: "generation".into(),
                kind: "note".into(),
                content: vec![Content::text("original")],
                tool: None,
            },
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn snapshot_pages_are_immutable_while_journal_advances() {
    let f = Fixture::new();
    let journal = Journal::new(&f.store).unwrap();
    journal.append(&f.session, None, &(0..130).map(|i| f.note(i)).collect::<Vec<_>>()).unwrap();
    let mut subscriptions = Subscriptions::default();
    let Attached::Snapshot { snapshot: lease, .. } = snapshot(&journal, 1, &f.session, "sub", &mut subscriptions).unwrap() else { panic!("snapshot") };
    let saved = &subscriptions.snapshots[&lease.token];
    assert!(saved.page_count >= 2, "bounded pages");
    fn read(saved: &Snapshot) -> Vec<Record> {
        let mut decoder = SnapshotDecoder::default();
        let mut records = vec![];
        for i in 0..saved.page_count {
            records.extend(decoder.records(&saved.page(i).unwrap()).unwrap());
        }
        records
    }
    let original = read(saved);
    journal.append(&f.session, None, &[Event::ItemTextAppended { item_id: "item-0".into(), block_id: "text".into(), text: " changed".into() }]).unwrap();
    let saved = &subscriptions.snapshots[&lease.token];
    assert_eq!(saved.at, lease.at);
    assert_eq!(read(saved), original);
    assert_ne!(journal.cursor(&f.session).unwrap(), lease.at);
}

#[test]
fn snapshot_leases_are_removed_only_for_the_detached_connection_and_session() {
    let f = Fixture::new();
    let journal = Journal::new(&f.store).unwrap();
    let mut all = Subscriptions::default();
    let token = |reply: Attached| match reply {
        Attached::Snapshot { snapshot, .. } => snapshot.token,
        _ => panic!("snapshot"),
    };
    let first = token(snapshot(&journal, 1, &f.session, "one", &mut all).unwrap());
    let second = token(snapshot(&journal, 2, &f.session, "two", &mut all).unwrap());
    all.remove(1, &f.session);
    assert!(!all.snapshots.contains_key(&first));
    assert!(all.snapshots.contains_key(&second));
    all.disconnect(2);
    assert!(all.snapshots.is_empty());
}

#[test]
fn cursor_domain_errors_survive_rpc_translation() {
    let f = Fixture::new();
    let journal = Journal::new(&f.store).unwrap();
    for cursor in [Cursor { session_id: "foreign".into(), seq: "0".into() }, Cursor { session_id: f.session.clone(), seq: "1".into() }] {
        let error = failure(journal.replay(&f.session, &cursor, 100).unwrap_err());
        assert_eq!(error.data.unwrap()["code"], "invalid_cursor");
    }
}

#[test]
fn large_snapshots_and_oversized_records_are_paged_without_losing_bytes() {
    let f = Fixture::new();
    let j = Journal::new(&f.store).unwrap();
    let text = "🦀\"\n".repeat(180_000);
    for i in 0..25 {
        let Event::ItemCreated { mut item } = f.note(i) else { unreachable!() };
        item.content = vec![Content::text(&text)];
        j.append(&f.session, None, &[Event::ItemCreated { item }]).unwrap();
    }
    let mut all = Subscriptions::default();
    let Attached::Snapshot { snapshot: lease, .. } = snapshot(&j, 1, &f.session, "large", &mut all).unwrap() else { panic!() };
    let saved = &all.snapshots[&lease.token];
    assert!(saved.bytes > 32 * 1024 * 1024);
    let mut decoder = SnapshotDecoder::default();
    let mut count = 0;
    for index in 0..saved.page_count {
        let page = saved.page(index).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() < 4 * 1024 * 1024);
        for record in decoder.records(&page).unwrap() {
            if let Record::Item { item } = record {
                assert_eq!(item.content[0].text.as_deref(), Some(text.as_str()));
                count += 1;
            }
        }
    }
    assert_eq!(count, 25);
}

#[test]
fn snapshot_disk_leases_expire_without_another_attach() {
    let f = Fixture::new();
    let j = Journal::new(&f.store).unwrap();
    let mut all = Subscriptions::default();
    let Attached::Snapshot { snapshot: lease, .. } = snapshot(&j, 1, &f.session, "sub", &mut all).unwrap() else { panic!() };
    all.snapshots.get_mut(&lease.token).unwrap().expires = Instant::now() - Duration::from_secs(1);
    all.expire();
    assert!(all.snapshots.is_empty());
}

#[test]
fn replacement_capture_counts_old_lease_and_failure_preserves_it() {
    let f = Fixture::new();
    let j = Journal::new(&f.store).unwrap();
    let mut all = Subscriptions::default();
    let token = |reply: Attached| match reply {
        Attached::Snapshot { snapshot, .. } => snapshot.token,
        _ => panic!("snapshot"),
    };
    let old = token(snapshot(&j, 1, &f.session, "old", &mut all).unwrap());
    let other = token(snapshot(&j, 2, &f.session, "other", &mut all).unwrap());
    let original = all.snapshots[&old].page(0).unwrap();
    // Simulate two full leases without allocating two GiB for an accounting regression.
    all.snapshots.get_mut(&old).unwrap().bytes = MAX_SNAPSHOT_BYTES;
    all.snapshots.get_mut(&other).unwrap().bytes = MAX_SNAPSHOT_BYTES;
    let error = snapshot(&j, 1, &f.session, "replacement", &mut all).unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "limit_exceeded");
    assert_eq!(all.snapshots.len(), 2);
    assert_eq!(serde_json::to_value(all.snapshots[&old].page(0).unwrap()).unwrap(), serde_json::to_value(original).unwrap());
    all.remove(1, &f.session);
    snapshot(&j, 1, &f.session, "replacement", &mut all).unwrap();
}

/// Explicit benchmark, excluded even from the optional conformance run unless sized.
/// Run one size per process so Linux VmHWM is meaningful:
/// SILL_SFP_BENCH_MIB=100 cargo test -p sill
///     snapshot_capture_benchmark -- --ignored --nocapture --test-threads=1
#[test]
#[ignore = "large-session benchmark; requires SILL_SFP_BENCH_MIB"]
fn snapshot_capture_benchmark() {
    let Ok(size) = std::env::var("SILL_SFP_BENCH_MIB") else {
        eprintln!("benchmark skipped: set SILL_SFP_BENCH_MIB");
        return;
    };
    let mib: usize = size.parse().expect("integer MiB");
    assert!((4..=500).contains(&mib), "use 4..=500 MiB");
    let mut f = Fixture::new();
    let other = f.store.create_with("test", "default", false).unwrap().id;
    let j = Journal::new(&f.store).unwrap();
    j.ensure(&other, "generation").unwrap();
    // Ordinary 64 KiB records plus a fragmented 2 MiB UTF-8 record. Write through
    // the journal, including event compaction and canonical persistence.
    let ordinary = "x".repeat(64 * 1024);
    let large = "🦀".repeat(512 * 1024);
    let count = (mib - 2) * 16 + 1;
    let seed = Instant::now();
    for i in 0..count {
        let Event::ItemCreated { mut item } = f.note(i) else { unreachable!() };
        item.content = vec![Content::text(if i == 0 { &large } else { &ordinary })];
        j.append(&f.session, None, &[Event::ItemCreated { item }]).unwrap();
    }
    let seeded = seed.elapsed();
    let at = j.cursor(&f.session).unwrap();
    // Same shared-store serialization as Hub. The competing writer is released
    // only after capture owns the lock; this measures a worst-case collision.
    let write_event = f.note(count);
    let shared = std::sync::Mutex::new(&mut f.store);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let shared_ref = &shared;
        let writer = scope.spawn(move || {
            ready_rx.recv().unwrap();
            let start = Instant::now();
            let store = shared_ref.lock().unwrap();
            let waited = start.elapsed();
            Journal::new(&store).unwrap().append(&other, None, &[write_event]).unwrap();
            (waited, start.elapsed())
        });
        let store = shared.lock().unwrap();
        let mut all = Subscriptions::default();
        let start = Instant::now();
        ready_tx.send(()).unwrap();
        let Attached::Snapshot { snapshot: lease, .. } = snapshot(&Journal::new(&store).unwrap(), 1, &f.session, "bench", &mut all).unwrap() else {
            unreachable!()
        };
        let capture = start.elapsed();
        drop(store);
        let (waited, write) = writer.join().unwrap();
        let saved = &all.snapshots[&lease.token];
        assert_eq!(saved.at, at);
        let read = Instant::now();
        let mut decoder = SnapshotDecoder::default();
        let mut seen = std::collections::HashSet::new();
        for index in 0..saved.page_count {
            for record in decoder.records(&saved.page(index).unwrap()).unwrap() {
                if let Record::Item { item } = record {
                    assert!(seen.insert(item.item_id.clone()), "duplicate item");
                    let expected = if item.item_id == "item-0" { &large } else { &ordinary };
                    assert_eq!(item.content[0].text.as_deref(), Some(expected.as_str()));
                }
            }
        }
        assert_eq!(seen.len(), count);
        let decode = read.elapsed();
        let db_bytes: u64 = std::fs::read_dir(&f.root).unwrap().map(|p| p.unwrap().metadata().unwrap().len()).sum();
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let peak = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap_or("VmHWM: unavailable");
        println!(
            "SFP_BENCH mib={mib} items={count} seed_s={:.3} capture_s={:.3} other_wait_s={:.3} other_write_s={:.3} decode_s={:.3} pages={} spool_bytes={} db_bytes={db_bytes} {peak}",
            seeded.as_secs_f64(),
            capture.as_secs_f64(),
            waited.as_secs_f64(),
            write.as_secs_f64(),
            decode.as_secs_f64(),
            saved.page_count,
            saved.bytes
        );
    });
}
