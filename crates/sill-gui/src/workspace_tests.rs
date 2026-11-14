use super::*;
use serde_json::{Value, json};
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
fn workspace() -> Workspace {
    let (mut w, _) = Workspace::new(true);
    w.select("a");
    w
}

#[test]
fn refresh_and_session_switch_keep_expansion_and_open_files() {
    let mut w = workspace();
    let folder = demo_path("/home/demo/project/src");
    let file = demo_path("/home/demo/project/src/main.rs");
    w.update(Action::Toggle(folder.clone()));
    w.update(Action::Open {
        path: file.clone(),
        repo: None,
    });
    w.update(Action::Refresh);
    assert!(w.state().unwrap().expanded.contains(key(&folder)));
    w.select("b");
    assert!(!w.state().unwrap().expanded.contains(key(&folder)));
    assert!(w.state().unwrap().viewer.is_none());
    w.select("a");
    assert!(w.state().unwrap().expanded.contains(key(&folder)));
    assert_eq!(
        key(&w.state().unwrap().viewer.as_ref().unwrap().path),
        key(&file)
    );
    w.update(Action::Toggle(folder.clone()));
    w.update(Action::Refresh);
    assert!(
        !w.state().unwrap().expanded.contains(key(&folder)),
        "refresh must not reopen a collapsed folder"
    );
}

#[test]
fn late_file_reply_cannot_replace_a_newer_file_or_reopen_a_closed_viewer() {
    let mut w = workspace();
    w.demo = false;
    let a = demo_path("/a.md");
    let b = demo_path("/b.md");
    w.update(Action::Open {
        path: a.clone(),
        repo: None,
    });
    let serial = w.serial;
    w.update(Action::Open {
        path: b.clone(),
        repo: None,
    });
    w.accept(Reply {
        session: "a".into(),
        serial,
        request: Request::File {
            path: a,
            repo: None,
        },
        result: Ok(Data::File("old content".into())),
    });
    assert_eq!(
        key(&w.state().unwrap().viewer.as_ref().unwrap().path),
        key(&b)
    );
    assert!(w.state().unwrap().viewer.as_ref().unwrap().text.is_empty());
    let serial = w.serial;
    w.update(Action::CloseViewer);
    w.accept(Reply {
        session: "a".into(),
        serial,
        request: Request::File {
            path: b,
            repo: None,
        },
        result: Ok(Data::File("late".into())),
    });
    assert!(w.state().unwrap().viewer.is_none());
}

#[test]
fn background_reply_only_updates_its_own_session() {
    let mut w = workspace();
    w.demo = false;
    let file = demo_path("/old.md");
    w.update(Action::Open {
        path: file.clone(),
        repo: None,
    });
    let serial = w.serial;
    w.select("b");
    w.accept(Reply {
        session: "a".into(),
        serial,
        request: Request::File {
            path: file,
            repo: None,
        },
        result: Ok(Data::File("belongs to a".into())),
    });
    assert!(w.state().unwrap().viewer.is_none());
    assert_eq!(w.states["a"].viewer.as_ref().unwrap().text, "belongs to a");
}

#[test]
fn viewer_selection_is_read_only_and_binary_file_errors_are_visible() {
    let mut w = workspace();
    let file = demo_path("/readme.md");
    w.update(Action::Open {
        path: file.clone(),
        repo: None,
    });
    let original = w.state().unwrap().viewer.as_ref().unwrap().content.text();
    w.update(Action::Text(text_editor::Action::Edit(
        text_editor::Edit::Insert('X'),
    )));
    assert_eq!(
        w.state().unwrap().viewer.as_ref().unwrap().content.text(),
        original
    );
    w.demo = false;
    w.update(Action::Open {
        path: file.clone(),
        repo: None,
    });
    w.accept(Reply {
        session: "a".into(),
        serial: w.serial,
        request: Request::File {
            path: file,
            repo: None,
        },
        result: Err("Binary file".into()),
    });
    assert_eq!(
        w.state().unwrap().viewer.as_ref().unwrap().error.as_deref(),
        Some("Binary file")
    );
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
        let error = read_text(&client, &demo_path("/file"), None)
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
    let path = demo_path("/project");
    let after = demo_path("/project/last");
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
            &demo_path("/project/file"),
            Some(&demo_path("/project"))
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
                    pins: paths.iter().map(|p| demo_path(p)).collect(),
                    next: None,
                },
            );
            let req = receive(&mut reader, "_sill/files.metadata");
            respond(&mut peer, &req, demo_metadata(true));
        }
        let req = receive(&mut reader, "_sill/files.home");
        respond(&mut peer, &req, demo_path("/home/demo"));
    });
    let Data::Roots { roots, .. } = execute(&client, "a", &Request::Roots).unwrap() else {
        panic!("roots")
    };
    assert_eq!(roots.len(), 2);
    assert!(!roots[0].global);
    assert!(roots[1].global);
    let state = State {
        roots,
        ..State::default()
    };
    assert!(state.pinned(&demo_path("/shared")));
    assert!(!state.pinned(&demo_path("/global-only")));
    server.join().unwrap();
}

#[test]
fn refreshing_a_paged_directory_retains_the_loaded_page_count() {
    let mut w = workspace();
    let path = demo_path("/home/demo/project");
    let state = w.states.get_mut("a").unwrap();
    let dir = state.directories.get_mut(key(&path)).unwrap();
    dir.entries = (0..256)
        .map(|i| admin::FileEntry {
            path: demo_path(&format!("/home/demo/project/{i}")),
            metadata: demo_metadata(false),
        })
        .collect();
    let (tx, rx) = mpsc::channel();
    w.demo = false;
    w.commands = Some(tx);
    w.update(Action::Refresh);
    assert!(matches!(rx.recv().unwrap().2, Request::Roots));
    let request = rx.recv().unwrap().2;
    assert!(matches!(request, Request::Directory { pages: 2, .. }));
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
                        path: demo_path(&format!("/entry-{page}")),
                        metadata: demo_metadata(false),
                    }],
                    next: (page == 0).then(|| demo_path("/last")),
                },
            );
        }
    });
    let Data::Directory(page) = execute(&client, "a", &request).unwrap() else {
        panic!("directory")
    };
    assert_eq!(page.entries.len(), 2);
    assert!(page.next.is_none());
    server.join().unwrap();
}
