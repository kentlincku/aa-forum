//! Files 頁（源自先前的 Python 版）。
//!
//! - 只給 Owner。資料區：`outputs`（可寫，預設 `<instance>/outputs`，AAF_FILES_ROOT 可改）；
//!   AAF_FILES_LEGACY_ROOT 設定時另有唯讀的 `legacy`。
//! - 路徑安全：相對路徑不得含 `..`、反斜線、NUL、絕對路徑；從根開始逐層以 O_NOFOLLOW 開啟，
//!   任何一層是 symlink 都拒絕（含根本身），避免檢查後被換路徑。
//! - 上傳：串流寫入 `<root>/<YYYYMMDD>/.uploading-<隨機>`，邊寫邊檢查上限（AAF_UPLOAD_MB，預設 1024，0＝不限），
//!   fsync 後以 hard link 排他發布；同名就改 `<名>-1.<副檔名>`…，不覆寫既有檔。需帶 `x-aaf-files: 1`（擋 CSRF 式表單送出）。
//! - 列表／搜尋：遞迴列出檔案（不跟 symlink、略過隱藏檔），可依路徑子字串搜尋，新到舊排序，每頁 100。
//! - 下載：一律 attachment＋nosniff＋CSP sandbox。

use aaf_api::error::{ApiError, ApiResult};
use aaf_api::{AppState, HttpFeature, User};
use aaf_core::config::ConfigKey;
use aaf_core::perms::Rank;
use aaf_core::Feature;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub mod safe;

pub const NAME_BYTES: usize = 255;
pub const PAGE: usize = 100;
const MAX_SCAN: usize = 200_000;

const CONFIG: &[ConfigKey] = &[
    ConfigKey {
        key: "AAF_FILES_ROOT",
        default: "",
        description: "Files 頁可寫資料區；空白＝<instance>/outputs",
    },
    ConfigKey {
        key: "AAF_FILES_LEGACY_ROOT",
        default: "",
        description: "Files 頁唯讀舊資料區（絕對路徑）；空白＝不提供",
    },
    ConfigKey {
        key: "AAF_UPLOAD_MB",
        default: "1024",
        description: "Files 頁單檔上傳上限（MB）；0＝不限",
    },
];

pub struct Files {
    instance: PathBuf,
}

impl Files {
    pub fn new(instance: impl Into<PathBuf>) -> Self {
        Self {
            instance: instance.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Space {
    pub id: &'static str,
    pub label: &'static str,
    pub writable: bool,
    pub path: String,
    pub exists: bool,
}

fn spaces(core: &aaf_core::Core, instance: &Path) -> Vec<(Space, PathBuf)> {
    let get = |k: &str| {
        core.config
            .get(k)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let outputs = match get("AAF_FILES_ROOT") {
        s if s.is_empty() => instance.join("outputs"),
        s => PathBuf::from(s),
    };
    let mut v = vec![(outputs, "outputs", "outputs", true)];
    let legacy = get("AAF_FILES_LEGACY_ROOT");
    if !legacy.is_empty() {
        v.push((PathBuf::from(legacy), "legacy", "legacy (read-only)", false));
    }
    v.into_iter()
        .filter(|(p, ..)| p.is_absolute())
        .map(|(p, id, label, writable)| {
            (
                Space {
                    id,
                    label,
                    writable,
                    path: p.display().to_string(),
                    exists: p.is_dir(),
                },
                p,
            )
        })
        .collect()
}

fn owner(u: &User) -> ApiResult<()> {
    if u.rank != Rank::Owner {
        return Err(ApiError::Forbidden(
            "the Files page is for the owner only".into(),
        ));
    }
    Ok(())
}

fn pick(s: &AppState, inst: &Path, id: &str) -> ApiResult<(Space, PathBuf)> {
    spaces(&s.core, inst)
        .into_iter()
        .find(|(sp, _)| sp.id == id)
        .ok_or_else(|| ApiError::BadRequest("unknown space".into()))
}

/// 相對路徑檢查：只允許一般名稱元件。
pub fn check_rel(rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains('\\') || rel.contains('\0') {
        return None;
    }
    let p = Path::new(rel);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(x) => out.push(x),
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// 只取最後一段檔名；拒絕空白、隱藏檔、控制字元、過長。
pub fn clean_name(raw: &str) -> Option<String> {
    let s = raw.replace('\\', "/");
    let name = s.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty()
        || name.starts_with('.')
        || name.chars().any(|c| (c as u32) < 32 || c as u32 == 127)
        || name.len() > NAME_BYTES
    {
        return None;
    }
    Some(name.to_string())
}

/// 同名另存：`<stem>-<n><suffix>`，總長不超過 255 bytes（截 stem，不切斷多位元組字）。
pub fn numbered(original: &str, n: u32) -> String {
    if n == 0 {
        return original.to_string();
    }
    let (mut stem, mut suffix) = match original.rfind('.') {
        Some(i) if i > 0 => (&original[..i], &original[i..]),
        _ => (original, ""),
    };
    let tag = format!("-{n}");
    if tag.len() + suffix.len() >= NAME_BYTES {
        stem = original;
        suffix = "";
    }
    let budget = NAME_BYTES - tag.len() - suffix.len();
    let mut cut = stem.len().min(budget);
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{tag}{suffix}", &stem[..cut])
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub rel: String,
    pub size: u64,
    pub modified: i64,
}

/// 遞迴列出檔案（不跟 symlink、略過隱藏與上傳中的暫存）。
fn scan(root: &Path) -> (Vec<Entry>, bool) {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    let mut partial = false;
    if std::fs::symlink_metadata(root)
        .map(|m| !m.is_dir())
        .unwrap_or(true)
    {
        return (out, false);
    }
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let r = rel.join(&name);
            if ft.is_dir() {
                stack.push(r);
            } else if ft.is_file() {
                let Ok(m) = e.metadata() else { continue };
                let modified = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                out.push(Entry {
                    rel: r.to_string_lossy().to_string(),
                    size: m.len(),
                    modified,
                });
                if out.len() >= MAX_SCAN {
                    partial = true;
                    return (out, partial);
                }
            }
        }
    }
    (out, partial)
}

#[derive(Deserialize)]
struct ListQ {
    #[serde(default = "outputs")]
    space: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    offset: usize,
}

fn outputs() -> String {
    "outputs".into()
}

async fn me(
    State(s): State<AppState>,
    axum::Extension(f): axum::Extension<Arc<Files>>,
    u: User,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let sp: Vec<Space> = spaces(&s.core, &f.instance)
        .into_iter()
        .map(|x| x.0)
        .collect();
    Ok(Json(
        json!({"spaces": sp, "upload_limit": upload_limit(&s.core)}),
    ))
}

async fn list(
    State(s): State<AppState>,
    axum::Extension(f): axum::Extension<Arc<Files>>,
    u: User,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let (sp, root) = pick(&s, &f.instance, &q.space)?;
    let needle = q.q.trim().to_lowercase();
    if needle.chars().count() > 200 || q.offset > 1_000_000 {
        return Err(ApiError::BadRequest("invalid search".into()));
    }
    let (mut all, partial) = tokio::task::spawn_blocking(move || scan(&root))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if !needle.is_empty() {
        all.retain(|e| e.rel.to_lowercase().contains(&needle));
    }
    all.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.rel.cmp(&b.rel)));
    let total = all.len();
    let page: Vec<Entry> = all.into_iter().skip(q.offset).take(PAGE).collect();
    Ok(Json(
        json!({"space": sp.id, "files": page, "total": total, "offset": q.offset, "limit": PAGE, "partial": partial}),
    ))
}

#[derive(Deserialize)]
struct DlQ {
    #[serde(default = "outputs")]
    space: String,
    rel: String,
}

async fn download(
    State(s): State<AppState>,
    axum::Extension(f): axum::Extension<Arc<Files>>,
    u: User,
    Query(q): Query<DlQ>,
) -> ApiResult<axum::response::Response> {
    owner(&u)?;
    let (_, root) = pick(&s, &f.instance, &q.space)?;
    let rel = check_rel(&q.rel).ok_or(ApiError::NotFound)?;
    let fd: OwnedFd = safe::open_file(&root, &rel).map_err(|_| ApiError::NotFound)?;
    let file = std::fs::File::from(fd);
    let meta = file.metadata().map_err(|_| ApiError::NotFound)?;
    if !meta.is_file() {
        return Err(ApiError::NotFound);
    }
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(
        tokio::fs::File::from_std(file),
    ));
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let enc: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    use axum::http::{header, HeaderValue};
    let mut res = axum::response::Response::new(body);
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.len()));
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=download; filename*=UTF-8''{enc}"
        ))
        .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    Ok(res)
}

fn upload_limit(core: &aaf_core::Core) -> Option<u64> {
    let mb: u64 = core.config.parse("AAF_UPLOAD_MB").unwrap_or(1024);
    (mb > 0).then_some(mb * 1024 * 1024)
}

#[derive(Deserialize)]
struct UpQ {
    #[serde(default = "outputs")]
    space: String,
    name: String,
}

async fn upload(
    State(s): State<AppState>,
    axum::Extension(f): axum::Extension<Arc<Files>>,
    u: User,
    Query(q): Query<UpQ>,
    req: axum::extract::Request,
) -> ApiResult<(axum::http::StatusCode, Json<Value>)> {
    owner(&u)?;
    if req
        .headers()
        .get("x-aaf-files")
        .and_then(|v| v.to_str().ok())
        != Some("1")
    {
        return Err(ApiError::Forbidden("upload from the Files page".into()));
    }
    let (sp, root) = pick(&s, &f.instance, &q.space)?;
    if !sp.writable {
        return Err(ApiError::Forbidden("this space is read-only".into()));
    }
    let original =
        clean_name(&q.name).ok_or_else(|| ApiError::BadRequest("invalid file name".into()))?;
    let limit = upload_limit(&s.core);
    let too_big =
        || ApiError::PayloadTooLarge(format!("limit is {} MB", limit.unwrap_or(0) / 1048576));
    if let Some(len) = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if limit.is_some_and(|l| len > l) {
            return Err(too_big());
        }
    }
    // 根目錄不存在就建立（只建根本身，且不可是 symlink）
    if !root.exists() {
        std::fs::create_dir_all(&root).map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    let now = jiff::Zoned::now();
    let day = now.strftime("%Y%m%d").to_string();
    let dayfd = safe::open_or_create_dir(&root, &day)
        .map_err(|e| ApiError::Internal(format!("open day dir: {e}")))?;
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| ApiError::Internal(e.to_string()))?;
    let tmp = format!(".uploading-{}", hex::encode(raw));
    let out = safe::create_new(&dayfd, &tmp).map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut file = tokio::fs::File::from_std(std::fs::File::from(out));
    let mut stream = req.into_body().into_data_stream();
    let mut written: u64 = 0;
    let res: ApiResult<()> = async {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| ApiError::BadRequest(format!("upload interrupted: {e}")))?;
            written += chunk.len() as u64;
            if limit.is_some_and(|l| written > l) {
                return Err(too_big());
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
        }
        file.flush()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        file.sync_all()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        Ok(())
    }
    .await;
    drop(file);
    let published = match res {
        Ok(()) => (0..10_000u32).find_map(|n| {
            let name = numbered(&original, n);
            match safe::link_new(&dayfd, &tmp, &name) {
                Ok(()) => Some(Ok(name)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(e) => Some(Err(ApiError::Internal(e.to_string()))),
            }
        }),
        Err(e) => Some(Err(e)),
    };
    let _ = safe::unlink(&dayfd, &tmp);
    let stored = published.unwrap_or_else(|| {
        Err(ApiError::Conflict(
            "too many files with the same name".into(),
        ))
    })?;
    s.core.bus.publish(
        "files.uploaded",
        1,
        json!({"space": sp.id, "rel": format!("{day}/{stored}"), "size": written}),
    )?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(
            json!({"file": {"original_name": original, "stored_name": stored, "renamed": stored != original, "size": written,
            "rel": format!("{day}/{stored}"), "path": root.join(&day).join(&stored).display().to_string()},
            "uploaded_at": now.strftime("%Y-%m-%d %H:%M:%S").to_string()}),
        ),
    ))
}

impl Feature for Files {
    fn name(&self) -> &'static str {
        "files"
    }
    fn config(&self) -> &'static [ConfigKey] {
        CONFIG
    }
}

impl HttpFeature for Files {
    fn name(&self) -> &'static str {
        "files"
    }
    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/me", get(me))
            .route("/list", get(list))
            .route("/download", get(download))
            .route(
                "/upload",
                post(upload).layer(axum::extract::DefaultBodyLimit::disable()),
            )
            .layer(axum::Extension(Arc::new(Files {
                instance: self.instance.clone(),
            })))
    }
    fn nav(&self) -> Vec<(&'static str, &'static str)> {
        vec![("Files", "/files")]
    }
    /// files.* 事件只給 Owner。
    fn can_see(&self, _s: &AppState, u: &User, ev: &aaf_contract::EventEnvelope) -> bool {
        !ev.kind.starts_with("files.") || u.rank == Rank::Owner
    }
}
