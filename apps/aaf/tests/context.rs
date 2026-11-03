use std::process::Command;

#[test]
fn context_cli_exposes_raw_source_retrieval() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut socket = loop {
            if let Ok((s, _)) = listener.accept() {
                break s;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut b = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut b).unwrap();
            request.push(b[0]);
        }
        assert!(String::from_utf8(request).unwrap().starts_with("GET /api/v1/context/rooms/7/roles/writer/sources?after=0&limit=20&source=3&offset=0&raw=true HTTP/1.1"));
        let body = r#"{"source":3,"text":"raw observation","next_offset":null}"#;
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let out = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .args(["context", "read", "--raw", "--source", "3"])
        .env("AAF_URL", format!("http://{addr}"))
        .env("AAF_ROLE", "writer")
        .env("AAF_ROOM", "7")
        .env("AAF_TOKEN", "fake-cli-token")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    server.join().unwrap();
}

#[test]
fn context_is_registered_and_cli_uses_scoped_authenticated_api() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .args(["--home", home.path().to_str().unwrap(), "features"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l == "context"),
        "durable feature must be registered in the real binary"
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = vec![];
        let mut b = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut b).unwrap();
            request.push(b[0]);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /api/v1/context/rooms/7/roles/writer/sources?after=0&limit=20&source=3&offset=4 HTTP/1.1"));
        assert!(request.contains("Authorization: Bearer test-token"));
        let body = r#"{"source":3,"text":"canary","next_offset":null}"#;
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    let out = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .args(["context", "read", "--source", "3", "--offset", "4"])
        .env("AAF_URL", format!("http://{addr}"))
        .env("AAF_ROLE", "writer")
        .env("AAF_ROOM", "7")
        .env("AAF_TOKEN", "test-token")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["text"],
        "canary"
    );
    server.join().unwrap();
}
#[test]
fn context_cli_rejects_malformed_json_success_response() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut s, _) = listener.accept().unwrap();
        let mut b = [0; 2048];
        let _ = s.read(&mut b).unwrap();
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\n{broken")
            .unwrap();
    });
    let out = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .args(["context", "read"])
        .env("AAF_URL", format!("http://{addr}"))
        .env("AAF_ROLE", "writer")
        .env("AAF_ROOM", "7")
        .env("AAF_TOKEN", "test-token")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        !out.status.success(),
        "malformed context response must not be silently reported as null"
    );
}
