//! In-process integration test: a real listener thread and a real client over an
//! [`interprocess`] local socket, exercising the handshake and one request.

use std::thread;

use loopreview_control::client::Client;
use loopreview_control::protocol::{
    Hello, PROTOCOL_VERSION, Reply, Request, Response, SessionInfo,
};
use loopreview_control::transport::{self, incoming};

/// A minimal server that answers a hello and one `Get`, mirroring the real TUI's
/// handshake, so the client path is covered end to end over a socket.
fn serve_one(socket: String, session_id: String) {
    let listener = transport::listen(&socket).expect("listen");
    thread::spawn(move || {
        // One connection is enough for the test.
        if let Some(mut conn) = incoming(&listener).map(transport::Connection::new).next() {
            // Handshake.
            let hello: Request = conn.read().expect("read hello");
            assert_eq!(hello, Request::Hello { version: 1 });
            conn.write(&Response::Ok(Reply::Hello(Hello {
                protocol: PROTOCOL_VERSION,
                session: session_id.clone(),
            })))
            .expect("write hello");
            // One request.
            let request: Request = conn.read().expect("read request");
            assert_eq!(request, Request::Get);
            conn.write(&Response::Ok(Reply::Session(SessionInfo {
                id: session_id.clone(),
                pid: std::process::id(),
                repo: Some("/repo".into()),
                source: "working tree".into(),
            })))
            .expect("write reply");
        }
    });
}

#[test]
fn client_handshake_and_get_over_a_socket() {
    let dir = std::env::temp_dir();
    let id = format!("it-{}", std::process::id());
    let socket = transport::socket_id(&dir, &id);
    serve_one(socket.clone(), id.clone());

    // The listener thread may need a beat to bind before we connect.
    let mut client = None;
    for _ in 0..50 {
        match Client::connect(&socket) {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    let mut client = client.expect("connect to the test server");
    assert_eq!(client.session(), id);

    match client.call(&Request::Get).expect("Get succeeds") {
        Reply::Session(info) => {
            assert_eq!(info.id, id);
            assert_eq!(info.source, "working tree");
        }
        other => panic!("unexpected reply: {other:?}"),
    }

    #[cfg(not(windows))]
    let _ = std::fs::remove_file(&socket);
}
