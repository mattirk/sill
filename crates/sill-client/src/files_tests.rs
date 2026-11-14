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
