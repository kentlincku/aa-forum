//! Durable context feature; runtime integration is optional.
pub struct Context;
pub mod ledger;
pub use ledger::Ledger;
pub mod config;
pub mod http;
impl aaf_core::Feature for Context {
    fn name(&self) -> &'static str {
        "context"
    }
    fn config(&self) -> &'static [aaf_core::config::ConfigKey] {
        config::CONFIG
    }
    fn capabilities(&self) -> &'static [aaf_core::perms::CapabilityDef] {
        &[aaf_core::perms::CapabilityDef {
            id: "context.source.read",
            description: "Read own room-scoped durable context",
            default_ranks: &[aaf_core::perms::Rank::Lead, aaf_core::perms::Rank::Worker],
        }]
    }
    fn migrations(&self) -> &'static [aaf_core::storage::Migration] {
        &[aaf_core::storage::Migration {
            version: 1,
            name: "canonical context ledger",
            sql: "
CREATE TABLE context_sources (
 id INTEGER PRIMARY KEY AUTOINCREMENT, role TEXT NOT NULL, room INTEGER NOT NULL,
 kind TEXT NOT NULL, body TEXT NOT NULL CHECK(json_valid(body)), source_key TEXT,
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
 UNIQUE(role,room,source_key)
);
CREATE INDEX context_sources_lane ON context_sources(role,room,id);
CREATE TABLE context_checkpoints (
 version INTEGER PRIMARY KEY AUTOINCREMENT, role TEXT NOT NULL, room INTEGER NOT NULL,
 cursor INTEGER NOT NULL, status TEXT NOT NULL,
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX context_checkpoints_lane ON context_checkpoints(role,room,version);
CREATE TABLE context_sessions (
 role TEXT NOT NULL, room INTEGER NOT NULL, session TEXT NOT NULL DEFAULT '',
 pending INTEGER NOT NULL DEFAULT 1, injected_cursor INTEGER NOT NULL DEFAULT 0,
 injected_version INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(role,room)
);
",
        }, aaf_core::storage::Migration {
            version: 2,
            name: "durable execution attempts",
            sql: "
CREATE TABLE context_attempts (
 id INTEGER PRIMARY KEY AUTOINCREMENT, role TEXT NOT NULL, room INTEGER NOT NULL,
 state TEXT NOT NULL DEFAULT 'inflight'
);
CREATE INDEX context_attempts_lane ON context_attempts(role,room,state);
ALTER TABLE context_sessions ADD COLUMN fault TEXT;
ALTER TABLE context_sessions ADD COLUMN active_attempt INTEGER;
ALTER TABLE context_sources ADD COLUMN attempt INTEGER;
",
        }, aaf_core::storage::Migration {
            version: 3,
            name: "logical source projection",
            sql: "
CREATE INDEX context_sources_attempt ON context_sources(role,room,attempt,kind);
CREATE VIEW context_logical_sources AS
 SELECT id,role,room,kind,body FROM context_sources WHERE kind!='assistant_chunk'
 UNION ALL
 SELECT max(id),role,room,'assistant_partial',
 json_object('text',group_concat(json_extract(body,'$.text'),''),'source_ids',json_group_array(id),'coverage','partial observable response; effects unverified')
 FROM (SELECT s.* FROM context_sources s WHERE kind='assistant_chunk'
 AND NOT EXISTS(SELECT 1 FROM context_sources o WHERE o.role=s.role AND o.room=s.room AND o.attempt=s.attempt AND o.kind='output' AND json_extract(o.body,'$.stop_reason')='end_turn') ORDER BY id)
 GROUP BY role,room,attempt;
",
        }, aaf_core::storage::Migration {
            version: 4,
            name: "bounded event catch-up cursor",
            // NULL = legacy row: the next catch-up scans from the start once, then advances.
            sql: "ALTER TABLE context_sessions ADD COLUMN event_cursor INTEGER;",
        }, aaf_core::storage::Migration {
            version: 5,
            name: "agent-reported context usage",
            // NULL = this session has not reported usage (or the agent never does).
            sql: "
ALTER TABLE context_sessions ADD COLUMN ctx_used INTEGER;
ALTER TABLE context_sessions ADD COLUMN ctx_size INTEGER;
ALTER TABLE context_sessions ADD COLUMN ctx_agent TEXT;
",
        }]
    }
}
/// Build the optional runtime extension. Invalid config blocks dispatch, never silently disables it.
pub fn runtime(core: &aaf_core::Core) -> std::sync::Arc<dyn aaf_runtime::continuity::Continuity> {
    match Ledger::configured(core) {
        Ok(l) => std::sync::Arc::new(l),
        Err(e) => std::sync::Arc::new(Unavailable(e.to_string())),
    }
}
struct Unavailable(String);
impl aaf_runtime::continuity::Continuity for Unavailable {
    fn rotate(&self, _: &aaf_runtime::dispatcher::Lane) -> aaf_core::Result<bool> {
        Err(aaf_core::Error::Config(self.0.clone()))
    }
    fn prepare(
        &self,
        _: &aaf_runtime::dispatcher::Lane,
        _: bool,
        _: &[aaf_runtime::dispatcher::InboxMessage],
    ) -> aaf_core::Result<aaf_runtime::continuity::PreparedContext> {
        Err(aaf_core::Error::Config(self.0.clone()))
    }
    fn complete(
        &self,
        _: &aaf_runtime::dispatcher::Lane,
        _: &aaf_runtime::continuity::PreparedContext,
        _: &aaf_runtime::driver::DResult<aaf_runtime::TurnResult>,
    ) -> aaf_core::Result<()> {
        Err(aaf_core::Error::Config(self.0.clone()))
    }
}
