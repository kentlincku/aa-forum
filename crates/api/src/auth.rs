//! 帳號與登入：首次 setup 建 Owner、帳密登入（argon2）、session token（只存 SHA-256）、
//! HttpOnly＋SameSite=Strict cookie 或 Bearer header。

use crate::error::{ApiError, ApiResult};
use crate::AppState;
use aaf_core::perms::{CapabilityDef, Rank};
use aaf_core::storage::Migration;
use aaf_core::Feature;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{FromRequestParts, State};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const COOKIE_NAME: &str = "aaf_session";
const SESSION_DAYS: i64 = 30;

/// 帳號資料表由 auth 這個內建 feature 擁有。
pub struct AuthFeature;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "users and sessions",
        sql: "
        CREATE TABLE auth_users(
            name TEXT PRIMARY KEY,
            rank TEXT NOT NULL CHECK (rank IN ('owner','human')),
            pw_hash TEXT NOT NULL,
            created_at INTEGER NOT NULL);
        CREATE TABLE auth_sessions(
            token_hash TEXT PRIMARY KEY,
            user TEXT NOT NULL REFERENCES auth_users(name) ON DELETE CASCADE,
            expires_at INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 2,
        name: "role tokens",
        sql: "
        -- 角色（agent）身分：每角色一個 token，只存雜湊。寄件者一律由 token 決定。
        CREATE TABLE auth_role_tokens(
            role TEXT PRIMARY KEY,
            rank TEXT NOT NULL CHECK (rank IN ('lead','worker')),
            token_hash TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL);
    ",
    },
];

/// 角色 token 的前綴，與人類 session token 區分。
pub const ROLE_TOKEN_PREFIX: &str = "aafr_";

/// 為角色發 token（已存在則換新，舊的立即失效）。回傳明文 token，只在此刻可得。
pub fn issue_role_token(core: &aaf_core::Core, role: &str, rank: &str) -> aaf_core::Result<String> {
    if rank != "lead" && rank != "worker" {
        return Err(aaf_core::Error::Config(format!(
            "role rank must be lead or worker: {rank}"
        )));
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| aaf_core::Error::Other(e.to_string()))?;
    let token = format!("{ROLE_TOKEN_PREFIX}{}", hex::encode(raw));
    core.db.with(|c| {
        let clash: i64 = c.query_row("SELECT count(*) FROM auth_users WHERE name = ?1", [role], |r| r.get(0))?;
        if clash > 0 {
            return Err(aaf_core::Error::Config(format!("role {role} has the same name as a user account")));
        }
        c.execute(
            "INSERT INTO auth_role_tokens(role, rank, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(role) DO UPDATE SET rank = excluded.rank, token_hash = excluded.token_hash, created_at = excluded.created_at",
            (role, rank, token_hash(&token), now()),
        )?;
        Ok(())
    })?;
    Ok(token)
}

/// 撤銷角色 token（角色移除時）。
/// 是否已有同名使用者（角色名不可與使用者名衝突）。
pub fn user_exists(core: &aaf_core::Core, name: &str) -> aaf_core::Result<bool> {
    core.db.with(|c| {
        Ok(c.query_row(
            "SELECT count(*) FROM auth_users WHERE name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )? > 0)
    })
}

pub fn revoke_role_token(core: &aaf_core::Core, role: &str) -> aaf_core::Result<()> {
    core.db.with(|c| {
        Ok(
            c.execute("DELETE FROM auth_role_tokens WHERE role = ?1", [role])
                .map(|_| ())?,
        )
    })
}

const CAPS: &[CapabilityDef] = &[CapabilityDef {
    id: "auth.user.create",
    description: "建立人類帳號",
    default_ranks: &[],
}];

impl Feature for AuthFeature {
    fn name(&self) -> &'static str {
        "auth"
    }
    fn migrations(&self) -> &'static [Migration] {
        MIGRATIONS
    }
    fn capabilities(&self) -> &'static [CapabilityDef] {
        CAPS
    }
}

/// 已驗證的呼叫者：人類帳號（Owner／Human）或角色（Lead／Worker）。
#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub name: String,
    #[serde(serialize_with = "ser_rank")]
    pub rank: Rank,
}

impl User {
    pub fn is_role(&self) -> bool {
        matches!(self.rank, Rank::Lead | Rank::Worker)
    }
}

fn ser_rank<S: serde::Serializer>(r: &Rank, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match r {
        Rank::Owner => "owner",
        Rank::Human => "human",
        Rank::Lead => "lead",
        Rank::Worker => "worker",
    })
}

impl User {
    /// 檢查權限能力。
    pub fn require(&self, s: &AppState, capability: &str) -> ApiResult<()> {
        Ok(s.core.policy.check(self.rank, capability)?)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn hash_password(pw: &str) -> ApiResult<String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| ApiError::Internal(e.to_string()))?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .to_string())
}

fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

/// 密碼規則：至少 8 字元。
fn check_password(pw: &str) -> ApiResult<()> {
    if pw.chars().count() < 8 {
        return Err(ApiError::BadRequest(
            "password must be at least 8 characters".into(),
        ));
    }
    Ok(())
}

fn check_name(name: &str) -> ApiResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "name: 1-32 chars of [A-Za-z0-9_-]".into(),
        ))
    }
}

fn token_from(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(t) = v.strip_prefix("Bearer ") {
            return Some(t.trim().to_string());
        }
    }
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == COOKIE_NAME).then(|| v.to_string())
        })
}

impl FromRequestParts<AppState> for User {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, s: &AppState) -> Result<Self, Self::Rejection> {
        let token = token_from(&parts.headers).ok_or(ApiError::Unauthorized)?;
        let h = token_hash(&token);
        if token.starts_with(ROLE_TOKEN_PREFIX) {
            let row = s.core.db.with(|c| {
                Ok(c.query_row(
                    "SELECT role, rank FROM auth_role_tokens WHERE token_hash = ?1",
                    [&h],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .ok())
            })?;
            let (name, rank) = row.ok_or(ApiError::Unauthorized)?;
            return Ok(User {
                name,
                rank: if rank == "lead" {
                    Rank::Lead
                } else {
                    Rank::Worker
                },
            });
        }
        let row = s.core.db.with(|c| {
            Ok(c.query_row(
                "SELECT u.name, u.rank FROM auth_sessions s JOIN auth_users u ON u.name = s.user WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                (&h, now()),
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok())
        })?;
        let (name, rank) = row.ok_or(ApiError::Unauthorized)?;
        Ok(User {
            name,
            rank: if rank == "owner" {
                Rank::Owner
            } else {
                Rank::Human
            },
        })
    }
}

#[derive(Deserialize)]
pub struct Credentials {
    pub name: String,
    pub password: String,
}

fn session_response(s: &AppState, user: &str) -> ApiResult<impl IntoResponse> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| ApiError::Internal(e.to_string()))?;
    let token = hex::encode(raw);
    let expires = now() + SESSION_DAYS * 86400;
    s.core.db.with(|c| {
        c.execute("DELETE FROM auth_sessions WHERE expires_at <= ?1", [now()])?;
        c.execute(
            "INSERT INTO auth_sessions(token_hash, user, expires_at) VALUES (?1, ?2, ?3)",
            (token_hash(&token), user, expires),
        )?;
        Ok(())
    })?;
    let cookie = format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_DAYS * 86400
    );
    let mut h = HeaderMap::new();
    h.insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    Ok((
        h,
        Json(serde_json::json!({"token": token, "user": user, "expires_at": expires})),
    ))
}

fn user_count(s: &AppState) -> ApiResult<i64> {
    Ok(s.core
        .db
        .with(|c| Ok(c.query_row("SELECT count(*) FROM auth_users", [], |r| r.get(0))?))?)
}

async fn setup_state(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(
        serde_json::json!({"needs_setup": user_count(&s)? == 0}),
    ))
}

/// 首次設定：沒有任何帳號時建立 Owner。之後一律 409。
async fn setup(
    State(s): State<AppState>,
    Json(c): Json<Credentials>,
) -> ApiResult<impl IntoResponse> {
    check_name(&c.name)?;
    check_password(&c.password)?;
    let hash = hash_password(&c.password)?;
    let created = s.core.db.with(|db| {
        let tx = db.transaction()?;
        let n: i64 = tx.query_row("SELECT count(*) FROM auth_users", [], |r| r.get(0))?;
        if n > 0 {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO auth_users(name, rank, pw_hash, created_at) VALUES (?1, 'owner', ?2, ?3)",
            (&c.name, &hash, now()),
        )?;
        tx.commit()?;
        Ok(true)
    })?;
    if !created {
        return Err(ApiError::Conflict("already set up".into()));
    }
    s.core.bus.publish(
        "auth.user.created",
        1,
        serde_json::json!({"name": c.name, "rank": "owner"}),
    )?;
    session_response(&s, &c.name)
}

async fn login(
    State(s): State<AppState>,
    Json(c): Json<Credentials>,
) -> ApiResult<impl IntoResponse> {
    let hash: Option<String> = s.core.db.with(|db| {
        Ok(db
            .query_row(
                "SELECT pw_hash FROM auth_users WHERE name = ?1",
                [&c.name],
                |r| r.get(0),
            )
            .ok())
    })?;
    // 帳號不存在時仍對一個真實雜湊跑一次驗證，避免以回應時間探測帳號
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let dummy =
        DUMMY.get_or_init(|| hash_password("dummy-password-for-timing").unwrap_or_default());
    let ok = verify_password(&c.password, hash.as_deref().unwrap_or(dummy)) && hash.is_some();
    if !ok {
        return Err(ApiError::Unauthorized);
    }
    session_response(&s, &c.name)
}

async fn logout(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    if let Some(t) = token_from(&headers) {
        s.core.db.with(|c| {
            Ok(c.execute(
                "DELETE FROM auth_sessions WHERE token_hash = ?1",
                [token_hash(&t)],
            )?)
        })?;
    }
    let mut h = HeaderMap::new();
    h.insert(
        SET_COOKIE,
        HeaderValue::from_static("aaf_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
    );
    Ok((h, Json(serde_json::json!({"ok": true}))))
}

async fn me(u: User) -> Json<User> {
    Json(u)
}

/// Owner 建立其他人類帳號。
async fn create_user(
    State(s): State<AppState>,
    u: User,
    Json(c): Json<Credentials>,
) -> ApiResult<impl IntoResponse> {
    u.require(&s, "auth.user.create")?;
    check_name(&c.name)?;
    check_password(&c.password)?;
    let hash = hash_password(&c.password)?;
    let clash: i64 = s.core.db.with(|db| {
        Ok(db.query_row(
            "SELECT count(*) FROM auth_role_tokens WHERE role = ?1",
            [&c.name],
            |r| r.get(0),
        )?)
    })?;
    if clash > 0 {
        return Err(ApiError::Conflict("name is used by an agent role".into()));
    }
    let inserted = s.core.db.with(|db| {
        Ok(db.execute("INSERT OR IGNORE INTO auth_users(name, rank, pw_hash, created_at) VALUES (?1, 'human', ?2, ?3)", (&c.name, &hash, now()))?)
    })?;
    if inserted == 0 {
        return Err(ApiError::Conflict("user exists".into()));
    }
    s.core.bus.publish(
        "auth.user.created",
        1,
        serde_json::json!({"name": c.name, "rank": "human", "by": u.name}),
    )?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/setup", get(setup_state).post(setup))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/users", post(create_user))
}
