use super::*;
use serde_json::{json, Value};
use std::{sync::mpsc, thread};

fn fixture_state() -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().canonicalize().unwrap().join("state");
    (temporary, state)
}

#[test]
fn singleton_publishes_and_removes_the_authenticated_endpoint() {
    let (_temporary, state) = fixture_state();
    let listener = Listener::open(&state).unwrap().unwrap();
    assert!(Listener::open(&state).unwrap().is_none());
    let endpoint = listener.endpoint.clone();
    assert!(endpoint.exists());

    let request = json!({"query": "Какие процедуры применить?"});
    let expected = request.clone();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some((reply, request)) =
                listener.accept::<Value>(Duration::from_secs(1)).unwrap()
            {
                assert_eq!(request, expected);
                reply.reply(&json!({"ok":true}));
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    });
    let reply: Value = send(&state, &request, Instant::now() + Duration::from_secs(1)).unwrap();
    assert_eq!(reply, json!({"ok": true}));
    server.join().unwrap();
    assert!(!endpoint.exists());
    assert!(Listener::open(&state).unwrap().is_some());
}

#[test]
fn an_unrecognized_listener_never_receives_the_query() {
    let (_temporary, state) = fixture_state();
    let directory = private_directory(&state, true).unwrap();
    let impostor = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let stale = Endpoint {
        version: VERSION,
        port: impostor.local_addr().unwrap().port(),
        secret: [9; 32],
    };
    let protected = platform::protect(&serde_json::to_vec(&stale).unwrap()).unwrap();
    io::write(
        &directory.join(ENDPOINT_NAME),
        &URL_SAFE_NO_PAD.encode(protected),
        false,
        None,
    )
    .unwrap();
    let (observed, captured) = mpsc::channel();
    let peer = thread::spawn(move || {
        let (mut stream, _) = impostor.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut hello = [0; 32];
        stream.read_exact(&mut hello).unwrap();
        stream.write_all(&[0; 64]).unwrap();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).unwrap();
        observed.send(rest).unwrap();
    });
    let request = json!({"query": "private text"});
    assert!(send::<_, Value>(&state, &request, Instant::now() + Duration::from_secs(1)).is_err());
    assert!(captured
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .is_empty());
    peer.join().unwrap();
}

#[test]
fn fragmented_input_does_not_reset_the_overall_deadline() {
    let receiver = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = receiver.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut stream, _) = receiver.accept().unwrap();
        for _ in 0..64 {
            if stream.write_all(&[1]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
    });
    let mut stream = TcpStream::connect(address).unwrap();
    let start = Instant::now();
    assert!(read_all(
        &mut stream,
        start + Duration::from_millis(120),
        &mut [0; 64]
    )
    .is_err());
    assert!(start.elapsed() < Duration::from_millis(500));
    drop(stream);
    peer.join().unwrap();
}

#[test]
fn hmac_matches_the_sha256_reference_vector() {
    let mut key = [0; 32];
    key[..20].fill(0x0b);
    let actual: String = hmac(&key, &[b"Hi There"])
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        actual,
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
}
