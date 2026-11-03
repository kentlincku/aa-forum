//! 群聊附件（圖片／檔案）。
//!
//! - 內容存 `<instance>/var/attachments/<sha256>`（內容定址、同內容只存一份），DB 只存中繼資料。
//! - 流程：上傳（串流寫暫存檔、邊寫邊算雜湊與大小上限）→ 得到附件 id → 發訊息時帶 `attachments: [id]` 綁定。
//! - 圖片：PNG／JPEG／WebP，限 5 MB、2000 萬像素；重新編碼成 PNG，去掉 EXIF 與尾隨內容（同 Python 版）。
//! - 檔案：單檔上限 AAF_CHAT_FILE_MB（預設 100，0＝不限）；下載一律 attachment + nosniff + CSP sandbox。
//! - 檔名只取最後一段，拒絕空白、隱藏檔、控制字元、路徑分隔字元；超過 255 bytes 拒絕。
//! - 未綁定訊息的上傳一天後清除；沒有任何列引用的內容檔一併刪除（群永久刪除後也是）。

use crate::store::now;
use aaf_core::{Core, Error, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 20_000_000;
pub const NAME_BYTES: usize = 255;
pub const MAX_PER_MESSAGE: usize = 10;
const ORPHAN_SECS: i64 = 86_400;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Attachment {
    pub id: i64,
    pub room: i64,
    pub message: Option<i64>,
    pub uploader: String,
    pub kind: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub created_at: i64,
    #[serde(skip)]
    pub sha256: String,
}

const COLS: &str =
    "id, room, message, uploader, kind, name, mime, size, width, height, created_at, sha256";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Attachment> {
    Ok(Attachment {
        id: r.get(0)?,
        room: r.get(1)?,
        message: r.get(2)?,
        uploader: r.get(3)?,
        kind: r.get(4)?,
        name: r.get(5)?,
        mime: r.get(6)?,
        size: r.get(7)?,
        width: r.get(8)?,
        height: r.get(9)?,
        created_at: r.get(10)?,
        sha256: r.get(11)?,
    })
}

pub fn dir(instance: &Path) -> PathBuf {
    instance.join("var").join("attachments")
}

pub fn blob_path(instance: &Path, sha: &str) -> Result<PathBuf> {
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Other("bad digest".into()));
    }
    Ok(dir(instance).join(sha))
}

/// 只取最後一段檔名；拒絕空白、隱藏檔、控制字元、過長。
pub fn clean_name(raw: &str) -> Option<String> {
    let last = raw.replace('\\', "/");
    let name = last.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty()
        || name.starts_with('.')
        || name.chars().any(|c| (c as u32) < 32 || c as u32 == 127)
        || name.len() > NAME_BYTES
    {
        return None;
    }
    Some(name.to_string())
}

pub fn file_limit(core: &Core) -> Option<u64> {
    let mb: u64 = core.config.parse("AAF_CHAT_FILE_MB").unwrap_or(100);
    (mb > 0).then_some(mb * 1024 * 1024)
}

/// 串流寫入暫存檔；超過上限立即中止。回傳 (暫存路徑, 大小, sha256)。
pub async fn spool<S, E>(
    instance: &Path,
    mut body: S,
    limit: Option<u64>,
) -> Result<(PathBuf, u64, String)>
where
    S: futures_util::Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let d = dir(instance);
    tokio::fs::create_dir_all(&d)
        .await
        .map_err(|e| Error::Other(format!("attachments dir: {e}")))?;
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| Error::Other(e.to_string()))?;
    let tmp = d.join(format!(".upload-{}", hex::encode(raw)));
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await
        .map_err(|e| Error::Other(e.to_string()))?;
    let mut h = Sha256::new();
    let mut n: u64 = 0;
    let res: Result<()> = async {
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| Error::Other(format!("upload interrupted: {e}")))?;
            n += chunk.len() as u64;
            if limit.is_some_and(|l| n > l) {
                return Err(Error::Other(format!(
                    "too large: limit is {} MB",
                    limit.unwrap() / 1048576
                )));
            }
            h.update(&chunk);
            f.write_all(&chunk)
                .await
                .map_err(|e| Error::Other(e.to_string()))?;
        }
        f.flush().await.map_err(|e| Error::Other(e.to_string()))?;
        f.sync_all()
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if n == 0 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(Error::Other("empty upload".into()));
    }
    Ok((tmp, n, hex::encode(h.finalize())))
}

/// 圖片正規化：解碼（限格式與像素）→ 重新編碼成 PNG（去 EXIF、尾隨內容、動畫只留第一格）。
pub fn normalize_image(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let bad = || {
        Error::Other("image must be a valid PNG/JPEG/WebP, at most 5 MB and 20 megapixels".into())
    };
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(bad());
    }
    let fmt = image::guess_format(bytes).map_err(|_| bad())?;
    if !matches!(
        fmt,
        image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP
    ) {
        return Err(bad());
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), fmt);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let (w, h) = reader.into_dimensions().map_err(|_| bad())?;
    if w as u64 * h as u64 > MAX_IMAGE_PIXELS {
        return Err(bad());
    }
    let img = image::load_from_memory_with_format(bytes, fmt).map_err(|_| bad())?;
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|_| bad())?;
    if out.len() as u64 > MAX_IMAGE_BYTES {
        return Err(bad());
    }
    Ok((out, img.width(), img.height()))
}

/// 把暫存檔（或正規化後的圖片位元組）發布成內容定址檔；已存在就不覆寫。
fn publish_file(instance: &Path, tmp: &Path, sha: &str) -> Result<()> {
    let dst = blob_path(instance, sha)?;
    if dst.exists() {
        let _ = std::fs::remove_file(tmp);
        return Ok(());
    }
    std::fs::rename(tmp, &dst).map_err(|e| Error::Other(format!("store attachment: {e}")))
}

#[allow(clippy::too_many_arguments)]
/// 上傳完成後建立附件列。`as_image`：依副檔名／內容判斷要當圖片（可預覽）處理。
pub fn create(
    core: &Core,
    instance: &Path,
    room: i64,
    uploader: &str,
    name: &str,
    tmp: PathBuf,
    size: u64,
    sha: String,
    as_image: bool,
) -> Result<Attachment> {
    let (kind, mime, size, sha, w, h) = if as_image {
        let bytes = std::fs::read(&tmp).map_err(|e| Error::Other(e.to_string()));
        let _ = std::fs::remove_file(&tmp);
        let (png, w, h) = normalize_image(&bytes?)?;
        let sha = hex::encode(Sha256::digest(&png));
        let dst = blob_path(instance, &sha)?;
        if !dst.exists() {
            let t = dst.with_extension("tmp");
            std::fs::write(&t, &png).map_err(|e| Error::Other(e.to_string()))?;
            std::fs::rename(&t, &dst).map_err(|e| Error::Other(e.to_string()))?;
        }
        (
            "image",
            "image/png".to_string(),
            png.len() as u64,
            sha,
            Some(w as i64),
            Some(h as i64),
        )
    } else {
        publish_file(instance, &tmp, &sha)?;
        (
            "file",
            "application/octet-stream".to_string(),
            size,
            sha,
            None,
            None,
        )
    };
    let name = if kind == "image" && !name.to_ascii_lowercase().ends_with(".png") {
        format!("{}.png", name.rsplit_once('.').map(|x| x.0).unwrap_or(name))
    } else {
        name.to_string()
    };
    core.db.with(|c| {
        c.execute(
            "INSERT INTO chat_attachments(room, uploader, kind, name, mime, size, sha256, width, height, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![room, uploader, kind, name, mime, size as i64, sha, w, h, now()],
        )?;
        let id = c.last_insert_rowid();
        Ok(c.query_row(&format!("SELECT {COLS} FROM chat_attachments WHERE id = ?1"), [id], row)?)
    })
}

/// 發訊息時綁定：必須是同一群、同一上傳者、尚未綁定。
pub fn bind(
    tx: &rusqlite::Transaction,
    room: i64,
    uploader: &str,
    message: i64,
    ids: &[i64],
) -> Result<Vec<Attachment>> {
    if ids.len() > MAX_PER_MESSAGE {
        return Err(Error::Other(format!(
            "at most {MAX_PER_MESSAGE} attachments per message"
        )));
    }
    let mut out = Vec::new();
    for id in ids {
        let n = tx.execute(
            "UPDATE chat_attachments SET message = ?1 WHERE id = ?2 AND room = ?3 AND uploader = ?4 AND message IS NULL",
            params![message, id, room, uploader],
        )?;
        if n != 1 {
            return Err(Error::Other(format!(
                "attachment {id} is not an unsent upload of yours in this room"
            )));
        }
        out.push(tx.query_row(
            &format!("SELECT {COLS} FROM chat_attachments WHERE id = ?1"),
            [id],
            row,
        )?);
    }
    Ok(out)
}

pub fn for_messages(
    c: &rusqlite::Connection,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<Attachment>>> {
    let mut map: std::collections::HashMap<i64, Vec<Attachment>> = Default::default();
    if ids.is_empty() {
        return Ok(map);
    }
    let slots = vec!["?"; ids.len()].join(",");
    let mut st = c.prepare(&format!(
        "SELECT {COLS} FROM chat_attachments WHERE message IN ({slots}) ORDER BY id"
    ))?;
    for a in st.query_map(rusqlite::params_from_iter(ids), row)? {
        let a = a?;
        map.entry(a.message.unwrap_or_default())
            .or_default()
            .push(a);
    }
    Ok(map)
}

pub fn get(c: &rusqlite::Connection, room: i64, id: i64) -> Result<Option<Attachment>> {
    Ok(c.query_row(
        &format!("SELECT {COLS} FROM chat_attachments WHERE id = ?1 AND room = ?2"),
        params![id, room],
        row,
    )
    .optional()?)
}

pub fn human_size(n: i64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1_048_576 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1_048_576.0)
    }
}

/// 清理：刪掉一天前未綁定的上傳；刪掉沒有任何列引用的內容檔。回傳刪除的內容檔數。
pub fn gc(core: &Core, instance: &Path) -> Result<usize> {
    let referenced: std::collections::HashSet<String> = core.db.with(|c| {
        c.execute(
            "DELETE FROM chat_attachments WHERE message IS NULL AND created_at < ?1",
            [now() - ORPHAN_SECS],
        )?;
        let mut st = c.prepare("SELECT DISTINCT sha256 FROM chat_attachments")?;
        let v = st
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    })?;
    let d = dir(instance);
    let Ok(rd) = std::fs::read_dir(&d) else {
        return Ok(0);
    };
    let mut removed = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let stale_tmp = name.starts_with(".upload-")
            && e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().unwrap_or_default().as_secs() > ORPHAN_SECS as u64);
        // 剛發布、還沒寫入 DB 的內容檔不能刪：只刪一小時以上沒被引用的
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().unwrap_or_default().as_secs() > 3600);
        if ((name.len() == 64 && old && !referenced.contains(&name)) || stale_tmp)
            && std::fs::remove_file(e.path()).is_ok()
        {
            removed += 1;
        }
    }
    Ok(removed)
}
