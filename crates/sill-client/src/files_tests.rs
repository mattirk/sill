//! The file requests over the public protocol: exact path bytes, revisions, cursors, pins.
use super::*;
use serde_json::{Value, json};
use sill_proto::sessions::HostPath;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::{ffi::OsStringExt, net::UnixStream};

fn receive(reader: &mut BufReader<UnixStream>, method: &str) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["method"], method);
    value
}
fn respond(peer: &mut UnixStream, request: &Value, result: impl serde::Serialize) {
    writeln!(
        peer,
        "{}",
        json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
    )
    .unwrap();
}
fn pair() -> (Client, UnixStream) {
    let (socket, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    (Client::over(socket).unwrap(), peer)
}
fn path_of(path: &str) -> HostPath {
    HostPath::from_path(std::path::Path::new(path))
}
fn metadata(directory: bool) -> admin::FileMetadata {
    admin::FileMetadata {
        file: !directory,
        directory,
        symlink: false,
        target: None,
        length: "150".into(),
        readonly: false,
    }
}

#[test]
fn file_pages_preserve_exact_path_bytes_and_revision() {
    let (client, mut peer) = pair();
    let mut path = HostPath::from_path(&std::path::PathBuf::from(std::ffi::OsString::from_vec(
        b"/project/\xff.md".to_vec(),
    )));
    path.display = "/this-display-is-not-the-path".into();
    let expected = path.bytes_base64.clone();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        for (offset, next, chunk, eof) in [
            ("0", "3", b"abc".as_slice(), false),
            ("3", "6", b"def".as_slice(), true),
        ] {
            let req = receive(&mut reader, "_sill/files.read");
            let params: admin::FileRead = serde_json::from_value(req["params"].clone()).unwrap();
            assert_eq!(params.path.bytes_base64, expected);
            assert_eq!(params.offset, offset);
            assert_eq!(
                params.revision.as_deref(),
                if offset == "0" {
                    None
                } else {
                    Some("version-1")
                }
            );
            respond(
                &mut peer,
                &req,
                admin::FileBytes {
                    bytes_base64: admin::encode_bytes(chunk),
                    next_offset: next.into(),
                    eof,
                    revision: "version-1".into(),
                },
            );
        }
    });
    assert_eq!(read_text(&client, &path, None).unwrap(), "abcdef");
    server.join().unwrap();
}

#[test]
fn changed_revision_and_binary_bytes_are_refused() {
    for binary in [false, true] {
        let (client, mut peer) = pair();
        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let first = receive(&mut reader, "_sill/files.read");
            respond(
                &mut peer,
                &first,
                admin::FileBytes {
                    bytes_base64: admin::encode_bytes(if binary { b"a\0b" } else { b"abc" }),
                    next_offset: "3".into(),
                    eof: binary,
                    revision: "one".into(),
                },
            );
            if !binary {
                let next = receive(&mut reader, "_sill/files.read");
                respond(
                    &mut peer,
                    &next,
                    admin::FileBytes {
                        bytes_base64: admin::encode_bytes(b"def"),
                        next_offset: "6".into(),
                        eof: true,
                        revision: "two".into(),
                    },
                );
            }
        });
        let error = read_text(&client, &path_of("/file"), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(if binary { "Binary" } else { "changed" }),
            "{error}"
        );
        server.join().unwrap();
    }
}

#[test]
fn directory_cursor_and_pin_actions_use_public_protocol() {
    let (client, mut peer) = pair();
    let path = path_of("/project");
    let after = path_of("/project/last");
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        let req = receive(&mut reader, "_sill/files.list");
        let params: admin::FilesList = serde_json::from_value(req["params"].clone()).unwrap();
        assert_eq!(params.path.display, "/project");
        assert_eq!(params.after.unwrap().display, "/project/last");
        respond(
            &mut peer,
            &req,
            admin::FilesPage {
                entries: vec![],
                next: None,
            },
        );
        for method in ["_sill/sessions.pin", "_sill/sessions.unpin"] {
            let req = receive(&mut reader, method);
            let params: pins::SessionPin = serde_json::from_value(req["params"].clone()).unwrap();
            assert_eq!(params.session_id, "session-a");
            assert_eq!(params.path.display, "/project");
            respond(&mut peer, &req, json!({}));
        }
    });
    execute(
        &client,
        "session-a",
        &Request::Directory {
            path: path.clone(),
            after: Some(after),
            pages: 1,
        },
    )
    .unwrap();
    for on in [true, false] {
        execute(
            &client,
            "session-a",
            &Request::Pin {
                path: path.clone(),
                on,
            },
        )
        .unwrap();
    }
    server.join().unwrap();
}

#[test]
fn diff_pages_use_fingerprint_and_live_target() {
    let (client, mut peer) = pair();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        for (offset, next, text) in [("0", Some("4"), "---\n"), ("4", None, "+++\n")] {
            let req = receive(&mut reader, "_sill/changes.diff");
            let params: changes::Diff = serde_json::from_value(req["params"].clone()).unwrap();
            assert_eq!(params.offset.as_deref(), Some(offset));
            assert_eq!(
                params.fingerprint.as_deref(),
                if offset == "0" { None } else { Some("fp") }
            );
            assert!(
                matches!(params.target, changes::Target::Live { repo, path } if repo.display == "/project" && path.display == "/project/file")
            );
            respond(
                &mut peer,
                &req,
                changes::DiffPage {
                    text: text.into(),
                    next_offset: next.map(str::to_string),
                    fingerprint: "fp".into(),
                },
            );
        }
    });
    assert_eq!(
        read_text(
            &client,
            &path_of("/project/file"),
            Some(&path_of("/project"))
        )
        .unwrap(),
        "---\n+++\n"
    );
    server.join().unwrap();
}

#[test]
fn global_pins_are_suggestions_and_session_pins_take_precedence() {
    let (client, mut peer) = pair();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        for (session, paths) in [
            (Some("a"), vec!["/shared"]),
            (None, vec!["/shared", "/global-only"]),
        ] {
            let req = receive(&mut reader, "_sill/pins.list");
            let params: pins::List = serde_json::from_value(req["params"].clone()).unwrap();
            assert_eq!(params.session_id.as_deref(), session);
            respond(
                &mut peer,
                &req,
                pins::Page {
                    pins: paths.iter().map(|p| path_of(p)).collect(),
                    next: None,
                },
            );
            let req = receive(&mut reader, "_sill/files.metadata");
            respond(&mut peer, &req, metadata(true));
        }
        let req = receive(&mut reader, "_sill/files.home");
        respond(&mut peer, &req, path_of("/home/demo"));
    });
    let Data::Roots { roots, .. } = execute(&client, "a", &Request::Roots).unwrap() else {
        panic!("roots")
    };
    assert_eq!(roots.len(), 2);
    assert!(!roots[0].global);
    assert!(roots[1].global);
    let state = FilesState {
        roots,
        ..FilesState::default()
    };
    assert!(state.pinned(&path_of("/shared")));
    assert!(!state.pinned(&path_of("/global-only")));
    server.join().unwrap();
}

#[test]
fn a_refresh_of_two_loaded_pages_follows_the_cursor_across_both() {
    let (client, mut peer) = pair();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        for page in 0..2 {
            let req = receive(&mut reader, "_sill/files.list");
            let params: admin::FilesList = serde_json::from_value(req["params"].clone()).unwrap();
            assert_eq!(
                params.after.as_ref().map(|p| p.display.as_str()),
                if page == 0 { None } else { Some("/last") }
            );
            respond(
                &mut peer,
                &req,
                admin::FilesPage {
                    entries: vec![admin::FileEntry {
                        path: path_of(&format!("/entry-{page}")),
                        metadata: metadata(false),
                    }],
                    next: (page == 0).then(|| path_of("/last")),
                },
            );
        }
    });
    let request = Request::Directory {
        path: path_of("/project"),
        after: None,
        pages: 2,
    };
    let Data::Directory(page) = execute(&client, "a", &request).unwrap() else {
        panic!("directory")
    };
    assert_eq!(page.entries.len(), 2);
    assert!(page.next.is_none());
    server.join().unwrap();
}

#[test]
fn git_status_colours_files_and_the_folders_above_them_but_not_the_root() {
    let entries = vec![
        discovery::StatusEntry {
            path: path_of("/r/docs/a.md"),
            status: "M".into(),
        },
        discovery::StatusEntry {
            path: path_of("/r/crates/x/new.rs"),
            status: "?".into(),
        },
    ];
    let mut state = FilesState::default();
    state.repos.insert(
        "pin".into(),
        Repo {
            root: Some(path_of("/r")),
            changes: index(Some(&path_of("/r")), &entries),
            entries,
            ..Repo::default()
        },
    );
    assert_eq!(
        state.change(&path_of("/r/docs/a.md")),
        Some(Change::Changed)
    );
    assert_eq!(state.change(&path_of("/r/docs")), Some(Change::Below));
    assert_eq!(
        state.change(&path_of("/r/crates/x/new.rs")),
        Some(Change::New)
    );
    assert_eq!(state.change(&path_of("/r/crates")), Some(Change::Below));
    assert_eq!(
        state.change(&path_of("/r")),
        None,
        "the root shows its branch instead"
    );
    assert_eq!(state.change(&path_of("/r/tests")), None);
}

#[test]
fn folders_list_as_the_frontend_lists_them() {
    let mut state = FilesState::default();
    let dir = path_of("/r");
    let serial = 7;
    let request = state.prepare(
        serial,
        Request::Directory {
            path: dir.clone(),
            after: None,
            pages: 1,
        },
    );
    let entry = |name: &str, directory: bool| admin::FileEntry {
        path: path_of(&format!("/r/{name}")),
        metadata: metadata(directory),
    };
    let page = admin::FilesPage {
        entries: vec![
            entry(".gitignore", false),
            entry("README.md", false),
            entry(".git", true),
            entry("tests", true),
            entry("crates", true),
            entry("AGENTS.md", false),
        ],
        next: None,
    };
    state.accept(Reply {
        session: "s".into(),
        serial,
        request,
        result: Ok(Data::Directory(page)),
    });
    let names: Vec<&str> = state.directories[key(&dir)]
        .entries
        .iter()
        .map(|e| name(&e.path))
        .collect();
    assert_eq!(
        names,
        [
            "crates",
            "tests",
            ".git",
            "AGENTS.md",
            "README.md",
            ".gitignore"
        ]
    );
}

/// A workspace session's repository reads its workspace, with every path given back under the
/// repository (docs/parallel-coding.md §12): what the tree, reader and chips use.
#[test]
fn aliased_paths_read_the_workspace_and_answer_under_the_repository() {
    let pairs = vec![(std::path::PathBuf::from("/home/me/repo"), std::path::PathBuf::from("/ws/repo-1"))];
    let run = aliased(&Request::Directory { path: path_of("/home/me/repo/src"), after: Some(path_of("/home/me/repo/src/a")), pages: 1 }, &pairs);
    let Request::Directory { path, after, .. } = &run else { panic!() };
    assert_eq!(path.to_path().unwrap(), std::path::Path::new("/ws/repo-1/src"));
    assert_eq!(after.as_ref().unwrap().to_path().unwrap(), std::path::Path::new("/ws/repo-1/src/a"));
    // the repository itself, and paths elsewhere untouched
    let Request::Repo { path: p, counts: true } = aliased(&Request::Repo { path: path_of("/home/me/repo"), counts: true }, &pairs) else { panic!() };
    assert_eq!(p.to_path().unwrap(), std::path::Path::new("/ws/repo-1"));
    let Request::Inspect(p) = aliased(&Request::Inspect(path_of("/home/me/repository")), &pairs) else { panic!() };
    assert_eq!(p.to_path().unwrap(), std::path::Path::new("/home/me/repository"));
    let Request::Pin { path, .. } = aliased(&Request::Pin { path: path_of("/home/me/repo"), on: true }, &pairs) else { panic!() };
    assert_eq!(path.to_path().unwrap(), std::path::Path::new("/home/me/repo"), "pins stay the repository's");
    // replies come back under the repository, displayed so
    let page = admin::FilesPage { entries: vec![admin::FileEntry { path: path_of("/ws/repo-1/src/b.rs"), metadata: metadata(false) }], next: None };
    let Data::Directory(back) = unaliased(Data::Directory(page), &pairs) else { panic!() };
    assert_eq!(back.entries[0].path.to_path().unwrap(), std::path::Path::new("/home/me/repo/src/b.rs"));
    assert_eq!(back.entries[0].path.display, "/home/me/repo/src/b.rs");
    let repo = Data::Repo { root: Some(path_of("/ws/repo-1")), head: "main".into(), entries: vec![discovery::StatusEntry { path: path_of("/ws/repo-1/x"), status: "M".into() }], summary: String::new(), lines: None, counts: vec![(path_of("/ws/repo-1/x"), Some((1, 2)))], truncated: false };
    let Data::Repo { root, entries, counts, .. } = unaliased(repo, &pairs) else { panic!() };
    assert_eq!(root.unwrap().to_path().unwrap(), std::path::Path::new("/home/me/repo"));
    assert_eq!(entries[0].path.to_path().unwrap(), std::path::Path::new("/home/me/repo/x"));
    assert_eq!(counts[0].0.to_path().unwrap(), std::path::Path::new("/home/me/repo/x"), "a row's counts are its entry's");
    // the table: set, changed or not, cleared
    let table = Aliases::default();
    assert!(table.set("s", pairs.clone()));
    assert!(!table.set("s", pairs.clone()));
    assert_eq!(table.of("s"), pairs);
    assert!(table.set("s", vec![]));
    assert!(table.of("s").is_empty());
}

/// A file saved from the reader goes where it was read from: the workspace copy, never the
/// person's checkout (which a save reached on 2026-11-14, carrying the agent's uncommitted text).
#[test]
fn a_saved_file_goes_to_the_workspace_it_was_read_from() {
    let files = Files::start(std::sync::Arc::new(|| anyhow::bail!("no daemon")), |_| true);
    let file = path_of("/home/me/repo/README.md");
    assert_eq!(files.workspace_path("s", &file).to_path().unwrap(), std::path::Path::new("/home/me/repo/README.md"));
    files.set_aliases("s", vec![(std::path::PathBuf::from("/home/me/repo"), std::path::PathBuf::from("/ws/repo-1"))]);
    assert_eq!(files.workspace_path("s", &file).to_path().unwrap(), std::path::Path::new("/ws/repo-1/README.md"));
    assert_eq!(files.workspace_path("other", &file).to_path().unwrap(), std::path::Path::new("/home/me/repo/README.md"), "another session's files are its own");
    assert_eq!(files.workspace_path("s", &path_of("/home/me/notes.md")).to_path().unwrap(), std::path::Path::new("/home/me/notes.md"));
}

/// The Diff view's per-file counts come with every look at the repository (docs/sill-gui.md
/// "Diff"): a file that stays modified — edited further, or committed and edited again — shows
/// its lines as they are, with the totals they add up to. Outside the Diff view no file is counted.
#[test]
fn a_files_counts_follow_it_while_its_status_stays_the_same() {
    let mut state = FilesState::default();
    let repo = path_of("/r");
    let file = path_of("/r/a.rs");
    let roots = |state: &mut FilesState, serial| {
        let request = state.prepare(serial, Request::Roots);
        state.accept(Reply {
            session: "s".into(),
            serial,
            request,
            result: Ok(Data::Roots {
                roots: vec![Root { path: repo.clone(), global: false, metadata: Some(metadata(true)), error: None }],
                home: path_of("/home/me"),
            }),
        })
    };
    let looked = |state: &mut FilesState, serial, request: Request, counts: (u64, u64)| {
        let Request::Repo { counts: asked, .. } = &request else { panic!("{request:?}") };
        let asked = *asked;
        let request = state.prepare(serial, request);
        state.accept(Reply {
            session: "s".into(),
            serial,
            request,
            result: Ok(Data::Repo {
                root: Some(repo.clone()),
                head: "main".into(),
                entries: vec![discovery::StatusEntry { path: file.clone(), status: "M".into() }],
                summary: String::new(),
                lines: Some((counts.0.to_string(), counts.1.to_string())),
                counts: if asked { vec![(file.clone(), Some(counts))] } else { vec![] },
                truncated: false,
            }),
        });
    };
    // the tree and the chips alone: the totals, no file counted
    let repo_of = |follow: Vec<Request>| follow.into_iter().find(|r| matches!(r, Request::Repo { .. })).unwrap();
    let request = repo_of(roots(&mut state, 1).follow);
    assert!(matches!(request, Request::Repo { counts: false, .. }), "{request:?}");
    looked(&mut state, 2, request, (35, 1));
    assert!(state.repos[key(&repo)].counts.is_empty());
    // the Diff view opens: the repository again, its files counted
    let mut requests = state.set_diff_mode(true);
    assert!(matches!(requests[..], [Request::Repo { counts: true, .. }]), "{requests:?}");
    looked(&mut state, 3, requests.remove(0), (35, 1));
    assert_eq!(state.repos[key(&repo)].counts[key(&file)], Some((35, 1)));
    // committed and edited again: the same file, the same letter, other lines
    let request = repo_of(roots(&mut state, 4).follow);
    assert!(matches!(request, Request::Repo { counts: true, .. }), "{request:?}");
    looked(&mut state, 5, request, (2, 2));
    let read = &state.repos[key(&repo)];
    assert_eq!(read.counts[key(&file)], Some((2, 2)));
    assert_eq!(read.lines, Some(("2".into(), "2".into())));
}
