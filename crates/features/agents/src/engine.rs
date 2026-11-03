//! agent 本體（engine）：版本、官方最新版、實測後替換本機 CLI。
//!
//! - npm 來源（例：pi）：暫存目錄 `npm install <pkg@ver>` → 叫轉接器用暫存的新本體跑一句 OK →
//!   通過才 `npm install -g --prefix <本機 CLI 的 prefix> <pkg@ver>`。
//! - github 來源（例：agy）：下載 release 的 `<name>_<os>_<arch>.tar.gz`，比對公布的 sha256 → 解壓 →
//!   實測 → 本機執行檔備份成 `.bak`，同目錄暫存檔 rename 替換。

use crate::install::{After, Jobs};
use aaf_runtime::catalog::{self, Engine};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 官方最新版：(版本, 說明)。npm 用 `npm view`；github 用 releases/latest。
pub async fn latest(npm_bin: &Path, e: &Engine) -> Result<String, String> {
    let src = e.latest.as_deref().ok_or("no official source")?;
    if let Some(pkg) = src.strip_prefix("npm:") {
        return crate::install::npm_latest(npm_bin, pkg)
            .await
            .map(|(v, _)| v);
    }
    if let Some(repo) = src.strip_prefix("github:") {
        let r = github_latest(repo).await?;
        return r["tag_name"]
            .as_str()
            .map(|t| t.trim_start_matches('v').to_string())
            .ok_or_else(|| "github: no tag".into());
    }
    Err(format!("unknown source {src}"))
}

fn valid_repo(repo: &str) -> bool {
    let mut it = repo.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && !p.starts_with('.')
                && p.len() <= 100
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    };
    ok(it.next()) && ok(it.next()) && it.next().is_none()
}

async fn curl(args: &[&str], secs: u64) -> Result<Vec<u8>, String> {
    let exe = catalog::which("curl").ok_or("curl not found")?;
    let o = tokio::time::timeout(
        Duration::from_secs(secs),
        tokio::process::Command::new(exe)
            .args(["-sSfL", "--proto", "=https"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "download timed out".to_string())?
    .map_err(|e| format!("curl: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "curl: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(o.stdout)
}

async fn github_latest(repo: &str) -> Result<Value, String> {
    if !valid_repo(repo) {
        return Err("invalid repo".into());
    }
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let b = curl(&["-H", "Accept: application/vnd.github+json", &url], 30).await?;
    serde_json::from_slice(&b).map_err(|e| format!("github: {e}"))
}

fn platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    (os, arch)
}

/// 本機 npm 全域安裝的 prefix：從 CLI 的實際路徑（…/lib/node_modules/<pkg>/…）推得。
fn npm_prefix_of(bin: &Path, pkg: &str) -> Option<PathBuf> {
    let real = std::fs::canonicalize(bin).ok()?;
    let s = real.to_string_lossy();
    let marker = format!("/lib/node_modules/{pkg}/");
    let i = s.find(&marker)?;
    Some(PathBuf::from(&s[..i]))
}

pub struct EngineUpgrade {
    pub agent: String,
    pub engine: Engine,
    pub version: String,
    /// 目前轉接器的 ACP 指令
    pub adapter_cmd: Vec<String>,
    pub adapter_version: Option<String>,
    pub check_timeout: Duration,
    pub after: Option<After>,
}

impl Jobs {
    /// 本體升級：暫存安裝＋實測，通過才替換本機 CLI。
    pub fn start_engine_upgrade(
        &self,
        instance: &Path,
        npm_bin: PathBuf,
        up: EngineUpgrade,
        timeout: Duration,
    ) -> Result<crate::install::Job, String> {
        let e = &up.engine;
        if e.source != "cli" || e.env.is_none() {
            return Err("this agent's engine is not managed here".into());
        }
        let cur = catalog::engine_bin(e).ok_or("the agent's CLI is not installed")?;
        if !up
            .version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
            || up.version.is_empty()
            || up.version.len() > 40
        {
            return Err("invalid version".into());
        }
        let src = e.latest.clone().ok_or("no official source")?;
        let label = match (src.strip_prefix("npm:"), src.strip_prefix("github:")) {
            (Some(p), _) => format!("{p}@{}", up.version),
            (_, Some(r)) => format!("{r}@{}", up.version),
            _ => return Err("unknown source".into()),
        };
        let job = self.begin_kind(instance, "engine", &up.agent, &label)?;
        let stage = instance.join("var").join("engine-stage").join(&job.id);
        let work = instance.join("work").join("_probe");
        let (me, instance, jid) = (self.clone(), instance.to_path_buf(), job.id.clone());
        tokio::spawn(async move {
            let say = |t: String| me.say(&jid, &t);
            let ok = 'run: {
                let _ = std::fs::create_dir_all(&stage);
                let _ = std::fs::create_dir_all(&work);
                say(format!("current {}: {}\n", up.agent, cur.display()));
                // 1. 暫存安裝
                say("1/3 get the new version into a staging folder\n".into());
                let new_bin = if let Some(pkg) = src.strip_prefix("npm:") {
                    let _ = std::fs::write(stage.join("package.json"), "{\"private\":true}\n");
                    let spec = format!("{pkg}@{}", up.version);
                    if !catalog::valid_npm(&spec) {
                        say("invalid package\n".into());
                        break 'run false;
                    }
                    let cmd = vec![
                        npm_bin.to_string_lossy().to_string(),
                        "install".into(),
                        "--no-audit".into(),
                        "--no-fund".into(),
                        spec.clone(),
                    ];
                    if me.run_cmd(&jid, &cmd, &stage, timeout).await != 0 {
                        say("staging install failed — nothing changed\n".into());
                        break 'run false;
                    }
                    // 只取檔名：version_cmd 可能是本機 CLI 的絕對路徑，join 絕對路徑會變回舊的那一個
                    let name = Path::new(&up.engine.version_cmd.as_ref().unwrap()[0])
                        .file_name()
                        .map(|n| n.to_owned())
                        .unwrap_or_default();
                    let b = stage.join("node_modules").join(".bin").join(name);
                    if !b.exists() {
                        say(format!(
                            "{} not found after staging install — nothing changed\n",
                            b.display()
                        ));
                        break 'run false;
                    }
                    b
                } else {
                    let repo = src.strip_prefix("github:").unwrap();
                    match github_asset(repo, &up.version, &stage, &|t| say(t)).await {
                        Ok(p) => p,
                        Err(err) => {
                            say(format!("{err} — nothing changed\n"));
                            break 'run false;
                        }
                    }
                };
                let ver = catalog::engine_version(
                    &Engine {
                        version_cmd: Some(vec![
                            new_bin.to_string_lossy().to_string(),
                            "--version".into(),
                        ]),
                        ..up.engine.clone()
                    },
                    &stage,
                )
                .await;
                say(format!(
                    "staged version: {}\n",
                    ver.as_deref().unwrap_or("?")
                ));
                // 2. 實測：目前轉接器＋新本體
                say("\n2/3 test the current adapter with the new version (one \"Reply with exactly: OK\")\n".into());
                let env = vec![(
                    up.engine.env.clone().unwrap(),
                    new_bin.to_string_lossy().to_string(),
                )];
                let check =
                    catalog::check_model_env(&up.adapter_cmd, &env, &work, "", up.check_timeout)
                        .await;
                crate::compat::record(
                    &instance,
                    &up.agent,
                    up.adapter_version.as_deref(),
                    ver.as_deref(),
                    &check,
                    "engine upgrade test",
                );
                if check["ok"] != true {
                    say(format!(
                        "test failed: {} — nothing changed, the current version stays\n",
                        check["error"].as_str().unwrap_or("?")
                    ));
                    break 'run false;
                }
                say(format!(
                    "test ok in {}s: model {} replied {:?}\n\n3/3 replace the installed {}\n",
                    check["secs"],
                    check["model"].as_str().unwrap_or("?"),
                    check["reply"].as_str().unwrap_or(""),
                    up.agent
                ));
                // 3. 替換本機
                if let Some(pkg) = src.strip_prefix("npm:") {
                    let Some(prefix) = npm_prefix_of(&cur, pkg) else {
                        say(format!(
                            "cannot tell how {} was installed (not an npm global install of {pkg}); update it yourself\n",
                            cur.display()
                        ));
                        break 'run false;
                    };
                    let cmd = vec![
                        npm_bin.to_string_lossy().to_string(),
                        "install".into(),
                        "-g".into(),
                        "--no-audit".into(),
                        "--no-fund".into(),
                        "--prefix".into(),
                        prefix.to_string_lossy().to_string(),
                        format!("{pkg}@{}", up.version),
                    ];
                    if me.run_cmd(&jid, &cmd, &stage, timeout).await != 0 {
                        say("global install failed\n".into());
                        break 'run false;
                    }
                } else if let Err(err) = replace_file(&new_bin, &cur) {
                    say(format!("replacing failed: {err}\n"));
                    break 'run false;
                } else {
                    say(format!(
                        "replaced {} (old copy kept as {}.bak)\n",
                        cur.display(),
                        cur.display()
                    ));
                }
                let now = catalog::engine_version(&up.engine, &stage).await;
                say(format!(
                    "installed version now: {}\n",
                    now.as_deref().unwrap_or("?")
                ));
                if now != ver {
                    say("version after replace does not match the tested one\n".into());
                    break 'run false;
                }
                crate::compat::record(
                    &instance,
                    &up.agent,
                    up.adapter_version.as_deref(),
                    now.as_deref(),
                    &check,
                    "engine upgrade",
                );
                if let Some(f) = up.after {
                    say(format!("{}\n", f().await));
                }
                true
            };
            let _ = std::fs::remove_dir_all(&stage);
            me.finish_pub(&jid, ok, if ok { 0 } else { 1 });
        });
        Ok(job)
    }
}

/// 下載 github release 的本平台壓縮檔，比對 sha256，解壓，回執行檔路徑。
async fn github_asset(
    repo: &str,
    version: &str,
    stage: &Path,
    say: &(dyn Fn(String) + Sync),
) -> Result<PathBuf, String> {
    if !valid_repo(repo) {
        return Err("invalid repo".into());
    }
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/{version}");
    let rel: Value = serde_json::from_slice(
        &curl(&["-H", "Accept: application/vnd.github+json", &url], 30).await?,
    )
    .map_err(|e| format!("github: {e}"))?;
    let (os, arch) = platform();
    let want = format!("_{os}_{arch}.tar.gz");
    let asset = rel["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(&want)))
        .ok_or_else(|| format!("release {version} has no {want} file"))?;
    let name = asset["name"].as_str().unwrap_or_default();
    let digest = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .ok_or("release does not publish a sha256 for this file; refusing to install")?
        .to_string();
    let dl = asset["browser_download_url"]
        .as_str()
        .filter(|u| u.starts_with("https://github.com/"))
        .ok_or("unexpected download URL")?;
    say(format!("download {name}\n"));
    let tgz = stage.join(name);
    curl(&["-o", &tgz.to_string_lossy(), dl], 600).await?;
    let got = sha256_file(&tgz).await?;
    if got != digest {
        return Err(format!("sha256 mismatch: got {got}, expected {digest}"));
    }
    say(format!("sha256 ok {digest}\n"));
    let out = stage.join("x");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let st = tokio::process::Command::new("/usr/bin/tar")
        .args(["-xzf", &tgz.to_string_lossy(), "-C", &out.to_string_lossy()])
        .status()
        .await
        .map_err(|e| format!("tar: {e}"))?;
    if !st.success() {
        return Err("tar failed".into());
    }
    // 壓縮檔裡只有一個執行檔
    let mut bins = vec![];
    for e in std::fs::read_dir(&out)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::symlink_metadata(e.path()).map_err(|e| e.to_string())?;
        if m.is_file() && m.permissions().mode() & 0o111 != 0 {
            bins.push(e.path());
        }
    }
    match bins.as_slice() {
        [b] => Ok(b.clone()),
        _ => Err(format!(
            "expected one executable in {name}, found {}",
            bins.len()
        )),
    }
}

async fn sha256_file(p: &Path) -> Result<String, String> {
    let o = tokio::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256", &p.to_string_lossy()])
        .output()
        .await
        .map_err(|e| format!("shasum: {e}"))?;
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .next()
        .map(Into::into)
        .ok_or_else(|| "shasum: no output".into())
}

/// 把 new 原子替換成 dst（dst 若是 symlink 則替換它指向的檔案）；舊檔備份成 .bak。
fn replace_file(new: &Path, dst: &Path) -> Result<(), String> {
    let target = std::fs::canonicalize(dst).map_err(|e| e.to_string())?;
    let dir = target.parent().ok_or("no parent dir")?;
    let tmp = dir.join(format!(
        ".{}.aaf-new-{}",
        target.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    std::fs::copy(new, &tmp).map_err(|e| format!("copy: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    let bak = target.with_file_name(format!(
        "{}.bak",
        target.file_name().unwrap().to_string_lossy()
    ));
    std::fs::copy(&target, &bak).map_err(|e| format!("backup: {e}"))?;
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename: {e}")
    })
}

/// 卡片要的本體資訊（不連網）。
pub async fn info(instance: &Path, e: &Engine) -> Value {
    let root = catalog::vendor_dir(instance);
    let ver = catalog::engine_version(e, &root).await;
    let bin = if e.source == "cli" {
        catalog::engine_bin(e).map(|p| p.display().to_string())
    } else {
        None
    };
    json!({"source": e.source, "package": e.package, "version": ver, "bin": bin,
        "managed": e.source == "cli" && e.env.is_some() && e.latest.is_some(), "official": e.latest})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repo_and_prefix() {
        assert!(valid_repo("google-antigravity/antigravity-cli"));
        assert!(!valid_repo("a/b/c") && !valid_repo("../x") && !valid_repo("a b/c"));
        let d = tempfile::tempdir().unwrap();
        let pkg = d.path().join("lib/node_modules/@x/y/dist");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("cli.js"), "").unwrap();
        let bin = d.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(pkg.join("cli.js"), bin.join("y")).unwrap();
        assert_eq!(
            npm_prefix_of(&bin.join("y"), "@x/y"),
            Some(std::fs::canonicalize(d.path()).unwrap())
        );
        assert_eq!(npm_prefix_of(&bin.join("y"), "@x/z"), None);
    }

    #[test]
    fn replace_keeps_backup_and_follows_symlink() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::write(&real, "old").unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let new = d.path().join("new");
        std::fs::write(&new, "new").unwrap();
        replace_file(&new, &link).unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(d.path().join("real.bak")).unwrap(),
            "old"
        );
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
