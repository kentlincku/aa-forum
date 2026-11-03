//! Skill 儲存（源自先前的 Python 版）。
//!
//! 檔案配置（instance 之下）：
//!   skills/<id>/SKILL.md               skill 本體（id：小寫英數、-、_，英數開頭，1–64 字）
//!   skills/_config.json                {"packs": {包: [id]}, "roles": {角色: {skills, packs}}, "rooms": {"群號": {skills, packs}}}
//!   var/skill_history/<id>/<版本>.md   每次儲存／刪除前的版本
//!   var/skill_history.jsonl            操作紀錄
//!
//! 與 Python 版差異：角色指派放在 _config.json（不寫 roles.json），改指派不會觸發 roles.json 熱重載、
//! 不會讓 agent 重啟；新內容在該角色下一回合送出。
//! 組合順序：角色 skills → 角色包 → 群 skills → 群包；重複只載一次。寫入一律暫存檔＋rename。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub const MAX_SKILL_BYTES: usize = 200_000;
pub const INDEX_MAX: usize = 120;

#[derive(Debug)]
pub struct SkillError(pub u16, pub String);

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

pub type R<T> = Result<T, SkillError>;

fn io(e: std::io::Error) -> SkillError {
    SkillError(500, e.to_string())
}

pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
        && !b[0].is_ascii_uppercase()
}

pub fn valid_pack(name: &str) -> bool {
    let n = name.chars().count();
    (1..=40).contains(&n)
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

fn check_id(id: &str) -> R<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(SkillError(
            400,
            "skill id: lowercase letters, digits, - and _ (1-64, starting with a letter or digit)"
                .into(),
        ))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Assign {
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub packs: Vec<String>,
}

impl Assign {
    fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.packs.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub packs: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub roles: BTreeMap<String, Assign>,
    #[serde(default)]
    pub rooms: BTreeMap<String, Assign>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillInfo {
    pub id: String,
    pub description: String,
    pub packs: Vec<String>,
    pub used_by: Vec<String>,
    pub bytes: usize,
    pub modified: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Version {
    pub version: String,
    pub bytes: u64,
}

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
}

fn atomic_write(path: &Path, text: &str) -> R<()> {
    let dir = path
        .parent()
        .ok_or_else(|| SkillError(500, "bad path".into()))?;
    std::fs::create_dir_all(dir).map_err(io)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

fn now_secs() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// frontmatter 的 description（支援單行與 | / > 多行）。
pub fn description(text: &str) -> String {
    let head: String = text.chars().take(2000).collect();
    let lines: Vec<&str> = head.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if let Some(rest) = l.strip_prefix("description:") {
            let first = rest.trim();
            let d = if ["|", ">", "|-", ">-", ""].contains(&first) {
                lines[i + 1..]
                    .iter()
                    .take_while(|x| x.starts_with(' ') || x.starts_with('\t'))
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                first.to_string()
            };
            return d
                .trim_matches(|c| c == '\'' || c == '"')
                .chars()
                .take(300)
                .collect();
        }
    }
    String::new()
}

impl Store {
    pub fn new(instance: &Path) -> Self {
        Self {
            root: instance.to_path_buf(),
        }
    }
    pub fn dir(&self) -> PathBuf {
        self.root.join("skills")
    }
    fn file(&self, id: &str) -> PathBuf {
        self.dir().join(id).join("SKILL.md")
    }
    fn hist(&self) -> PathBuf {
        self.root.join("var").join("skill_history")
    }

    pub fn config(&self) -> Config {
        std::fs::read_to_string(self.dir().join("_config.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    fn save_config(&self, c: &Config) -> R<()> {
        atomic_write(
            &self.dir().join("_config.json"),
            &(serde_json::to_string_pretty(c).unwrap() + "\n"),
        )
    }

    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                // 不跟 symlink：skill 目錄與 SKILL.md 都必須是實體
                let dir_ok = e.file_type().is_ok_and(|t| t.is_dir());
                let f = e.path().join("SKILL.md");
                let file_ok = std::fs::symlink_metadata(&f).is_ok_and(|m| m.is_file());
                (valid_id(&n) && dir_ok && file_ok).then_some(n)
            })
            .collect();
        v.sort();
        v
    }

    pub fn read(&self, id: &str) -> R<String> {
        check_id(id)?;
        let f = self.file(id);
        if !std::fs::symlink_metadata(&f).is_ok_and(|m| m.is_file()) {
            return Err(SkillError(404, format!("skill {id} not found")));
        }
        std::fs::read(&f)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .map_err(io)
    }

    pub fn usage(&self) -> BTreeMap<String, Vec<String>> {
        let c = self.config();
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut add = |s: &str, who: String| out.entry(s.to_string()).or_default().push(who);
        for (kind, map) in [("role", &c.roles), ("room", &c.rooms)] {
            for (k, a) in map {
                for s in &a.skills {
                    add(s, format!("{kind} {k}"));
                }
                for p in &a.packs {
                    for s in c.packs.get(p).into_iter().flatten() {
                        add(s, format!("{kind} {k} (via pack {p})"));
                    }
                }
            }
        }
        for (p, members) in &c.packs {
            for s in members {
                add(s, format!("pack {p}"));
            }
        }
        out
    }

    pub fn listing(&self) -> Vec<SkillInfo> {
        let c = self.config();
        let used = self.usage();
        self.ids()
            .into_iter()
            .filter_map(|id| {
                let f = self.file(&id);
                let text = std::fs::read(&f)
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).to_string())?;
                let modified = std::fs::metadata(&f)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let mut packs: Vec<String> = c
                    .packs
                    .iter()
                    .filter(|(_, m)| m.contains(&id))
                    .map(|(p, _)| p.clone())
                    .collect();
                packs.sort();
                Some(SkillInfo {
                    description: description(&text),
                    packs,
                    used_by: used.get(&id).cloned().unwrap_or_default(),
                    bytes: text.len(),
                    modified,
                    id,
                })
            })
            .collect()
    }

    // ---------- 版本紀錄 ----------
    fn log(&self, actor: &str, action: &str, target: &str, note: &str) {
        let line = serde_json::json!({"at": now_secs(), "actor": actor, "action": action, "skill": target, "note": note.chars().take(300).collect::<String>()});
        let p = self.root.join("var").join("skill_history.jsonl");
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    fn snapshot(&self, id: &str) -> R<Option<String>> {
        let f = self.file(id);
        if !f.exists() {
            return Ok(None);
        }
        let d = self.hist().join(id);
        std::fs::create_dir_all(&d).map_err(io)?;
        let z = jiff::Zoned::now();
        let base = format!(
            "{}-{:03}",
            z.strftime("%Y%m%d-%H%M%S"),
            z.subsec_nanosecond() / 1_000_000
        );
        let mut ver = base.clone();
        let mut n = 1;
        while d.join(format!("{ver}.md")).exists() {
            ver = format!("{base}-{n}");
            n += 1;
        }
        std::fs::copy(&f, d.join(format!("{ver}.md"))).map_err(io)?;
        Ok(Some(ver))
    }

    pub fn history(&self, id: &str) -> R<(Vec<Version>, Vec<serde_json::Value>)> {
        check_id(id)?;
        let d = self.hist().join(id);
        let mut vers: Vec<Version> = std::fs::read_dir(&d)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                let v = n.strip_suffix(".md")?.to_string();
                Some(Version {
                    version: v,
                    bytes: e.metadata().ok()?.len(),
                })
            })
            .collect();
        vers.sort_by(|a, b| b.version.cmp(&a.version));
        let log: Vec<serde_json::Value> =
            std::fs::read_to_string(self.root.join("var").join("skill_history.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|e| e["skill"] == id)
                .collect();
        let start = log.len().saturating_sub(100);
        Ok((vers, log[start..].to_vec()))
    }

    pub fn read_version(&self, id: &str, ver: &str) -> R<String> {
        check_id(id)?;
        let ok = {
            let p: Vec<&str> = ver.split('-').collect();
            (p.len() == 3 || p.len() == 4)
                && p[0].len() == 8
                && p[1].len() == 6
                && p[2].len() == 3
                && p.iter()
                    .all(|x| !x.is_empty() && x.len() <= 8 && x.bytes().all(|b| b.is_ascii_digit()))
        };
        if !ok {
            return Err(SkillError(400, "invalid version".into()));
        }
        std::fs::read_to_string(self.hist().join(id).join(format!("{ver}.md")))
            .map_err(|_| SkillError(404, "no such version".into()))
    }

    // ---------- 寫入 ----------
    pub fn save(
        &self,
        id: &str,
        text: &str,
        actor: &str,
        create: bool,
        note: &str,
    ) -> R<serde_json::Value> {
        check_id(id)?;
        if text.len() > MAX_SKILL_BYTES {
            return Err(SkillError(
                413,
                format!("skill is larger than {} KB", MAX_SKILL_BYTES / 1000),
            ));
        }
        if text.trim().is_empty() {
            return Err(SkillError(400, "content must not be empty".into()));
        }
        let f = self.file(id);
        let exists = std::fs::symlink_metadata(&f).is_ok_and(|m| m.is_file());
        if create && exists {
            return Err(SkillError(409, format!("skill {id} already exists")));
        }
        if !create && !exists {
            return Err(SkillError(404, format!("skill {id} not found")));
        }
        if exists && std::fs::read_to_string(&f).ok().as_deref() == Some(text) {
            return Ok(serde_json::json!({"ok": true, "changed": false}));
        }
        let ver = self.snapshot(id)?;
        atomic_write(&f, text)?;
        let default_note = ver
            .as_ref()
            .map(|v| format!("previous {v}"))
            .unwrap_or_default();
        self.log(
            actor,
            if create { "create" } else { "edit" },
            id,
            if note.is_empty() { &default_note } else { note },
        );
        Ok(serde_json::json!({"ok": true, "changed": true, "previous": ver}))
    }

    pub fn restore(&self, id: &str, ver: &str, actor: &str) -> R<serde_json::Value> {
        let text = self.read_version(id, ver)?;
        let r = self.save(id, &text, actor, false, &format!("restore {ver}"))?;
        self.log(actor, "restore", id, ver);
        Ok(r)
    }

    pub fn delete(&self, id: &str, actor: &str) -> R<serde_json::Value> {
        check_id(id)?;
        if !self.file(id).exists() {
            return Err(SkillError(404, format!("skill {id} not found")));
        }
        if let Some(u) = self.usage().get(id) {
            return Err(SkillError(
                409,
                format!("skill is in use; unassign first: {}", u.join(", ")),
            ));
        }
        let ver = self.snapshot(id)?;
        std::fs::remove_dir_all(self.dir().join(id)).map_err(io)?;
        self.log(
            actor,
            "delete",
            id,
            &format!("before delete {}", ver.clone().unwrap_or_default()),
        );
        Ok(serde_json::json!({"ok": true, "previous": ver}))
    }

    pub fn set_pack(&self, name: &str, members: &[String], actor: &str) -> R<Vec<String>> {
        if !valid_pack(name) {
            return Err(SkillError(
                400,
                "pack name: letters, digits, - and _ (1-40)".into(),
            ));
        }
        let known: HashSet<String> = self.ids().into_iter().collect();
        let bad: Vec<&String> = members.iter().filter(|s| !known.contains(*s)).collect();
        if !bad.is_empty() {
            return Err(SkillError(
                400,
                format!(
                    "unknown skills: {}",
                    bad.iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        let mut c = self.config();
        let mut m: Vec<String> = members.to_vec();
        m.sort();
        m.dedup();
        c.packs.insert(name.to_string(), m.clone());
        self.save_config(&c)?;
        self.log(actor, "pack", name, &members.join(","));
        Ok(m)
    }

    pub fn delete_pack(&self, name: &str, actor: &str) -> R<()> {
        let mut c = self.config();
        if !c.packs.contains_key(name) {
            return Err(SkillError(404, "no such pack".into()));
        }
        let users: Vec<String> = c
            .roles
            .iter()
            .filter(|(_, a)| a.packs.iter().any(|p| p == name))
            .map(|(r, _)| format!("role {r}"))
            .chain(
                c.rooms
                    .iter()
                    .filter(|(_, a)| a.packs.iter().any(|p| p == name))
                    .map(|(r, _)| format!("room {r}")),
            )
            .collect();
        if !users.is_empty() {
            return Err(SkillError(
                409,
                format!("pack is in use; unassign first: {}", users.join(", ")),
            ));
        }
        c.packs.remove(name);
        self.save_config(&c)?;
        self.log(actor, "pack-delete", name, "");
        Ok(())
    }

    fn validate(&self, a: &Assign) -> R<Assign> {
        let known: HashSet<String> = self.ids().into_iter().collect();
        let c = self.config();
        let bad: Vec<String> = a
            .skills
            .iter()
            .filter(|s| !known.contains(*s))
            .cloned()
            .chain(
                a.packs
                    .iter()
                    .filter(|p| !c.packs.contains_key(*p))
                    .map(|p| format!("pack {p}")),
            )
            .collect();
        if !bad.is_empty() {
            return Err(SkillError(400, format!("not found: {}", bad.join(", "))));
        }
        let dedup = |v: &[String]| {
            let mut seen = HashSet::new();
            v.iter()
                .filter(|x| seen.insert((*x).clone()))
                .cloned()
                .collect::<Vec<_>>()
        };
        Ok(Assign {
            skills: dedup(&a.skills),
            packs: dedup(&a.packs),
        })
    }

    pub fn assign(&self, kind: &str, key: &str, a: &Assign, actor: &str) -> R<Assign> {
        let a = self.validate(a)?;
        let mut c = self.config();
        let map = if kind == "role" {
            &mut c.roles
        } else {
            &mut c.rooms
        };
        if a.is_empty() {
            map.remove(key);
        } else {
            map.insert(key.to_string(), a.clone());
        }
        self.save_config(&c)?;
        self.log(
            actor,
            &format!("assign-{kind}"),
            key,
            &format!("skills={:?} packs={:?}", a.skills, a.packs),
        );
        Ok(a)
    }

    /// 角色（＋群）實際載入的 skill id，依序、去重。
    pub fn compose(&self, role: &str, room: Option<i64>) -> Vec<String> {
        self.compose_with(role, room, &[], &[])
    }

    /// compose＋額外的包、減掉的包（群級 lead：被指定者在該群加 `lead` 包；
    /// 群已指定別人時，全域 lead 在該群拿掉 `lead` 包）。
    pub fn compose_with(
        &self,
        role: &str,
        room: Option<i64>,
        extra_packs: &[&str],
        drop_packs: &[&str],
    ) -> Vec<String> {
        let c = self.config();
        let mut order: Vec<String> = Vec::new();
        let mut push = |a: Option<&Assign>| {
            if let Some(a) = a {
                order.extend(a.skills.iter().cloned());
                for p in a.packs.iter().filter(|p| !drop_packs.contains(&p.as_str())) {
                    order.extend(c.packs.get(p).into_iter().flatten().cloned());
                }
            }
        };
        push(c.roles.get(role));
        if let Some(r) = room {
            push(c.rooms.get(&r.to_string()));
        }
        for p in extra_packs {
            order.extend(c.packs.get(*p).into_iter().flatten().cloned());
        }
        let mut seen = HashSet::new();
        order
            .into_iter()
            .filter(|s| valid_id(s) && seen.insert(s.clone()) && self.file(s).exists())
            .collect()
    }

    /// 給 agent 的內容：指派的 skill 全文 + 其餘 skill 的清單（名稱、說明、路徑，需要時自行開檔）。
    pub fn context(&self, role: &str, room: Option<i64>) -> String {
        self.context_with(role, room, &[], &[])
    }

    pub fn context_with(
        &self,
        role: &str,
        room: Option<i64>,
        extra_packs: &[&str],
        drop_packs: &[&str],
    ) -> String {
        let loaded = self.compose_with(role, room, extra_packs, drop_packs);
        let mut out = String::new();
        for id in &loaded {
            if let Ok(t) = self.read(id) {
                out.push_str(&format!("\n[Skill assigned to you: {id}]\n{t}\n"));
            }
        }
        let rest: Vec<String> = self
            .ids()
            .into_iter()
            .filter(|s| !loaded.contains(s))
            .collect();
        if !rest.is_empty() {
            out.push_str("\nAvailable skills (open the SKILL.md with your file tools when relevant; skip unrelated ones):\n");
            for id in rest.iter().take(INDEX_MAX) {
                let d = self.read(id).map(|t| description(&t)).unwrap_or_default();
                let d = if d.is_empty() {
                    "(no description)".to_string()
                } else {
                    d.chars().take(160).collect()
                };
                out.push_str(&format!("- {id}: {d} ({})\n", self.file(id).display()));
            }
            if rest.len() > INDEX_MAX {
                out.push_str(&format!(
                    "- … {} more in {}\n",
                    rest.len() - INDEX_MAX,
                    self.dir().display()
                ));
            }
        }
        if out.is_empty() {
            return out;
        }
        format!("[AA Forum skills — this block is sent once per session and again only when it changes]{out}\n")
    }
}
