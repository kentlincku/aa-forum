//! 不跟隨 symlink 的檔案操作：從 `/` 開始逐層 openat(O_NOFOLLOW)，以目錄 fd 綁住路徑，
//! 避免「檢查後被換成 symlink」的競態（同 Python 版 open_member）。

use rustix::fs::{self, AtFlags, Mode, OFlags};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};

fn err(e: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

/// 逐層開到 `root/rel` 的父目錄，回傳 (父目錄 fd, 最後一段名稱)。root 本身也不可是 symlink。
fn walk(root: &Path, rel: &Path) -> io::Result<(OwnedFd, String)> {
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "root must be absolute",
        ));
    }
    let mut parts: Vec<String> = Vec::new();
    for c in root.components().chain(rel.components()) {
        match c {
            Component::RootDir => {}
            Component::Normal(x) => parts.push(x.to_string_lossy().to_string()),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid path")),
        }
    }
    let last = parts
        .pop()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty path"))?;
    let mut dir = fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(err)?;
    for p in parts {
        dir = fs::openat(
            dir.as_fd(),
            p.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(err)?;
    }
    Ok((dir, last))
}

/// 唯讀開啟一般檔案；路徑上任何 symlink 都拒絕。
pub fn open_file(root: &Path, rel: &Path) -> io::Result<OwnedFd> {
    let (dir, last) = walk(root, rel)?;
    fs::openat(
        dir.as_fd(),
        last.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(err)
}

/// 開啟（必要時建立）`root/<name>` 目錄。
pub fn open_or_create_dir(root: &Path, name: &str) -> io::Result<OwnedFd> {
    let rootfd = {
        let (parent, last) = walk(root, Path::new(""))?;
        fs::openat(
            parent.as_fd(),
            last.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(err)?
    };
    match fs::mkdirat(rootfd.as_fd(), name, Mode::from_raw_mode(0o770)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(err(e)),
    }
    fs::openat(
        rootfd.as_fd(),
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(err)
}

pub fn create_new(dir: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    fs::openat(
        dir.as_fd(),
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o660),
    )
    .map_err(err)
}

/// 排他發布：目標已存在回 AlreadyExists，不覆寫。
pub fn link_new(dir: &OwnedFd, from: &str, to: &str) -> io::Result<()> {
    fs::linkat(dir.as_fd(), from, dir.as_fd(), to, AtFlags::empty()).map_err(err)
}

pub fn unlink(dir: &OwnedFd, name: &str) -> io::Result<()> {
    fs::unlinkat(dir.as_fd(), name, AtFlags::empty()).map_err(err)
}
