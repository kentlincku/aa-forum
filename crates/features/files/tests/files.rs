//! Files 頁。

use aaf_api::{AuthFeature, HttpFeature};
use aaf_core::feature::BootOptions;
use aaf_core::storage::Db;
use aaf_core::{Core, Registry};
use aaf_feature_files::Files;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Wrap(Arc<Files>);
impl aaf_core::Feature for Wrap {
    fn name(&self) -> &'static str {
        aaf_core::Feature::name(&*self.0)
    }
    fn config(&self) -> &'static [aaf_core::config::ConfigKey] {
        self.0.config()
    }
}

struct Env {
    addr: String,
    owner: String,
    ann: String,
    home: tempfile::TempDir,
    legacy: tempfile::TempDir,
    outside: tempfile::TempDir,
}

async fn req(
    addr: &str,
    method: &str,
    path: &str,
    token: &str,
    extra: &str,
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..split]).to_string();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let mut body = out[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut dec = Vec::new();
        let mut rest = &body[..];
        loop {
            let nl = rest.windows(2).position(|w| w == b"\r\n").unwrap();
            let n = usize::from_str_radix(std::str::from_utf8(&rest[..nl]).unwrap().trim(), 16)
                .unwrap();
            if n == 0 {
                break;
            }
            dec.extend_from_slice(&rest[nl + 2..nl + 2 + n]);
            rest = &rest[nl + 2 + n + 2..];
        }
        body = dec;
    }
    (status, head, body)
}

async fn json(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Value) {
    let (st, _, b) = req(
        addr,
        method,
        path,
        token.unwrap_or(""),
        "",
        body.to_string().as_bytes(),
    )
    .await;
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn start(upload_mb: &str) -> Env {
    let home = tempfile::tempdir().unwrap();
    let legacy = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    // macOS 的 /var 是 symlink → 用 canonical 路徑當根（使用者應設定實際路徑）
    let home_p = home.path().canonicalize().unwrap();
    let legacy_p = legacy.path().canonicalize().unwrap();
    let files = Arc::new(Files::new(&home_p));
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Wrap(files.clone()))).unwrap();
    let opts = BootOptions {
        env: vec![
            ("AAF_UPLOAD_MB".into(), upload_mb.into()),
            (
                "AAF_FILES_LEGACY_ROOT".into(),
                legacy_p.display().to_string(),
            ),
        ],
        ..Default::default()
    };
    let core = Core::boot(&reg, Db::open_in_memory().unwrap(), opts).unwrap();
    let http: Vec<Arc<dyn HttpFeature>> = vec![files];
    let app = aaf_api::app(core, &http);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let owner = json(
        &addr,
        "POST",
        "/api/v1/auth/setup",
        None,
        json!({"name": "alice", "password": "correct horse"}),
    )
    .await
    .1["token"]
        .as_str()
        .unwrap()
        .to_string();
    json(
        &addr,
        "POST",
        "/api/v1/auth/users",
        Some(&owner),
        json!({"name": "ann", "password": "password123"}),
    )
    .await;
    let ann = json(
        &addr,
        "POST",
        "/api/v1/auth/login",
        None,
        json!({"name": "ann", "password": "password123"}),
    )
    .await
    .1["token"]
        .as_str()
        .unwrap()
        .to_string();
    Env {
        addr,
        owner,
        ann,
        home,
        legacy,
        outside,
    }
}

const UP: &str = "x-aaf-files: 1\r\n";

#[tokio::test]
async fn owner_only_and_spaces() {
    let e = start("1024").await;
    assert_eq!(
        json(&e.addr, "GET", "/api/v1/files/me", Some(&e.ann), json!({}))
            .await
            .0,
        403
    );
    assert_eq!(
        json(
            &e.addr,
            "GET",
            "/api/v1/files/list",
            Some(&e.ann),
            json!({})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        req(
            &e.addr,
            "POST",
            "/api/v1/files/upload?name=a.txt",
            &e.ann,
            UP,
            b"x"
        )
        .await
        .0,
        403
    );
    let (st, me) = json(
        &e.addr,
        "GET",
        "/api/v1/files/me",
        Some(&e.owner),
        json!({}),
    )
    .await;
    assert_eq!(st, 200);
    let ids: Vec<&str> = me["spaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["outputs", "legacy"]);
    assert_eq!(me["spaces"][1]["writable"], false);
    assert_eq!(
        json(
            &e.addr,
            "GET",
            "/api/v1/files/list?space=nope",
            Some(&e.owner),
            json!({})
        )
        .await
        .0,
        400
    );
}

#[tokio::test]
async fn upload_dedupe_download_search() {
    let e = start("1024").await;
    // 需帶 x-aaf-files 標頭
    assert_eq!(
        req(
            &e.addr,
            "POST",
            "/api/v1/files/upload?name=a.txt",
            &e.owner,
            "",
            b"x"
        )
        .await
        .0,
        403
    );
    let mut stored = vec![];
    for body in [&b"first"[..], b"second", b"third"] {
        let (st, _, b) = req(
            &e.addr,
            "POST",
            "/api/v1/files/upload?name=%E5%A0%B1%E5%91%8A.md",
            &e.owner,
            UP,
            body,
        )
        .await;
        assert_eq!(st, 201, "{}", String::from_utf8_lossy(&b));
        let v: Value = serde_json::from_slice(&b).unwrap();
        stored.push(v["file"]["stored_name"].as_str().unwrap().to_string());
        assert!(v["file"]["path"].as_str().unwrap().starts_with(
            e.home
                .path()
                .canonicalize()
                .unwrap()
                .join("outputs")
                .to_str()
                .unwrap()
        ));
    }
    assert_eq!(
        stored,
        ["報告.md", "報告-1.md", "報告-2.md"],
        "never overwrite; numbered copies"
    );
    // 不合法檔名
    for bad in [".env", "%2E%2E", "a%0Ab.txt", ""] {
        assert_eq!(
            req(
                &e.addr,
                "POST",
                &format!("/api/v1/files/upload?name={bad}"),
                &e.owner,
                UP,
                b"x"
            )
            .await
            .0,
            400,
            "{bad}"
        );
    }
    // 名稱只取最後一段
    let (_, _, b) = req(
        &e.addr,
        "POST",
        "/api/v1/files/upload?name=..%2F..%2Fevil.sh",
        &e.owner,
        UP,
        b"x",
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap()["file"]["stored_name"],
        "evil.sh"
    );
    // 唯讀區不可上傳
    assert_eq!(
        req(
            &e.addr,
            "POST",
            "/api/v1/files/upload?space=legacy&name=a.txt",
            &e.owner,
            UP,
            b"x"
        )
        .await
        .0,
        403
    );
    // 沒有暫存檔殘留
    let day = std::fs::read_dir(e.home.path().join("outputs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(std::fs::read_dir(&day).unwrap().all(|f| !f
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with('.')));

    // 列表 + 搜尋（不分大小寫、含中文）
    let (_, l) = json(
        &e.addr,
        "GET",
        "/api/v1/files/list",
        Some(&e.owner),
        json!({}),
    )
    .await;
    assert_eq!(l["total"], 4);
    let (_, l) = json(
        &e.addr,
        "GET",
        "/api/v1/files/list?q=%E5%A0%B1%E5%91%8A-1",
        Some(&e.owner),
        json!({}),
    )
    .await;
    assert_eq!(l["total"], 1);
    let rel = l["files"][0]["rel"].as_str().unwrap().to_string();
    // 下載
    let enc: String = rel
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let (st, head, body) = req(
        &e.addr,
        "GET",
        &format!("/api/v1/files/download?rel={enc}"),
        &e.owner,
        "",
        b"",
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(body, b"second");
    let h = head.to_ascii_lowercase();
    assert!(
        h.contains("content-disposition: attachment")
            && h.contains("x-content-type-options: nosniff")
            && h.contains("content-security-policy: sandbox"),
        "{head}"
    );
    // legacy 可讀
    std::fs::write(e.legacy.path().join("old.txt"), "legacy data").unwrap();
    let (st, _, body) = req(
        &e.addr,
        "GET",
        "/api/v1/files/download?space=legacy&rel=old.txt",
        &e.owner,
        "",
        b"",
    )
    .await;
    assert_eq!((st, body.as_slice()), (200, &b"legacy data"[..]));
}

#[tokio::test]
async fn symlinks_and_traversal_are_refused() {
    let e = start("1024").await;
    let out = e.home.path().canonicalize().unwrap().join("outputs");
    std::fs::create_dir_all(out.join("sub")).unwrap();
    std::fs::write(e.outside.path().join("secret.txt"), "TOP SECRET").unwrap();
    std::os::unix::fs::symlink(e.outside.path().join("secret.txt"), out.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(e.outside.path(), out.join("dirlink")).unwrap();
    std::fs::write(out.join("sub/ok.txt"), "fine").unwrap();
    for rel in [
        "link.txt",
        "dirlink/secret.txt",
        "..%2F..%2Fetc%2Fpasswd",
        "%2Fetc%2Fpasswd",
        "sub%2F..%2F..%2Fx",
        "sub",
    ] {
        let (st, _, body) = req(
            &e.addr,
            "GET",
            &format!("/api/v1/files/download?rel={rel}"),
            &e.owner,
            "",
            b"",
        )
        .await;
        assert_eq!(st, 404, "{rel}");
        assert!(!String::from_utf8_lossy(&body).contains("TOP SECRET"));
    }
    assert_eq!(
        req(
            &e.addr,
            "GET",
            "/api/v1/files/download?rel=sub%2Fok.txt",
            &e.owner,
            "",
            b""
        )
        .await
        .0,
        200
    );
    // 列表不跟 symlink
    let (_, l) = json(
        &e.addr,
        "GET",
        "/api/v1/files/list",
        Some(&e.owner),
        json!({}),
    )
    .await;
    let rels: Vec<&str> = l["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["rel"].as_str().unwrap())
        .collect();
    assert_eq!(rels, ["sub/ok.txt"]);
    // 根目錄本身被換成 symlink：一律拒絕
    std::fs::remove_dir_all(&out).unwrap();
    std::os::unix::fs::symlink(e.outside.path(), &out).unwrap();
    assert_eq!(
        req(
            &e.addr,
            "GET",
            "/api/v1/files/download?rel=secret.txt",
            &e.owner,
            "",
            b""
        )
        .await
        .0,
        404
    );
    assert_eq!(
        req(
            &e.addr,
            "POST",
            "/api/v1/files/upload?name=x.txt",
            &e.owner,
            UP,
            b"x"
        )
        .await
        .0,
        500
    );
    assert!(
        !e.outside.path().read_dir().unwrap().any(|f| f
            .unwrap()
            .file_name()
            .to_string_lossy()
            .len()
            == 8),
        "no day dir created outside"
    );
}

#[tokio::test]
async fn upload_size_limit() {
    let e = start("1").await;
    let big = vec![b'x'; 1024 * 1024 + 1];
    assert_eq!(
        req(
            &e.addr,
            "POST",
            "/api/v1/files/upload?name=big.bin",
            &e.owner,
            UP,
            &big
        )
        .await
        .0,
        413
    );
    assert_eq!(
        req(
            &e.addr,
            "POST",
            "/api/v1/files/upload?name=ok.bin",
            &e.owner,
            UP,
            &big[..1024]
        )
        .await
        .0,
        201
    );
}

#[test]
fn numbered_names_fit_255_bytes() {
    use aaf_feature_files::numbered;
    assert_eq!(numbered("a.txt", 0), "a.txt");
    assert_eq!(numbered("a.txt", 3), "a-3.txt");
    assert_eq!(numbered("noext", 1), "noext-1");
    let long = format!("{}.txt", "測".repeat(84)); // 252 + 4 bytes > 255 after tag
    let n = numbered(&long, 12);
    assert!(
        n.len() <= 255 && n.ends_with("-12.txt"),
        "{} {}",
        n.len(),
        n
    );
}
