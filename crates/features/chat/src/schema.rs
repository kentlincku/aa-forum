use aaf_core::storage::Migration;

pub const MIGRATIONS: &[Migration] = &[
    Migration {
    version: 1,
    name: "rooms members messages deliveries",
    sql: "
        CREATE TABLE chat_rooms(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','archived','deleted')),
            frozen INTEGER NOT NULL DEFAULT 0,
            folder TEXT NOT NULL DEFAULT '',
            pinned INTEGER NOT NULL DEFAULT 0,
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            deleted_at INTEGER,
            deleted_by TEXT);
        CREATE TABLE chat_members(
            room INTEGER NOT NULL REFERENCES chat_rooms(id) ON DELETE CASCADE,
            member TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('user','role')),
            added_at INTEGER NOT NULL,
            PRIMARY KEY (room, member));
        CREATE TABLE chat_messages(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            room INTEGER NOT NULL REFERENCES chat_rooms(id) ON DELETE CASCADE,
            author TEXT NOT NULL,
            author_kind TEXT NOT NULL CHECK (author_kind IN ('user','role','system')),
            body TEXT NOT NULL,
            reply_to INTEGER,
            thread_id INTEGER,
            created_at INTEGER NOT NULL);
        CREATE INDEX chat_messages_room ON chat_messages(room, id);
        CREATE TABLE chat_deliveries(
            message INTEGER NOT NULL REFERENCES chat_messages(id) ON DELETE CASCADE,
            recipient TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','done','cancelled')),
            PRIMARY KEY (message, recipient));
        CREATE INDEX chat_deliveries_queue ON chat_deliveries(recipient, state);
        CREATE TABLE chat_reads(
            room INTEGER NOT NULL REFERENCES chat_rooms(id) ON DELETE CASCADE,
            member TEXT NOT NULL,
            through INTEGER NOT NULL,
            PRIMARY KEY (room, member));
        CREATE TABLE chat_sessions(
            role TEXT NOT NULL,
            room INTEGER NOT NULL,          -- 0＝角色的預設 lane
            session_id TEXT NOT NULL,
            PRIMARY KEY (role, room));
        -- 稽核：永久刪除後仍保留（不設外鍵）
        CREATE TABLE chat_room_audit(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            room INTEGER NOT NULL,
            room_name TEXT NOT NULL,
            action TEXT NOT NULL,
            actor TEXT NOT NULL,
            detail TEXT NOT NULL DEFAULT '',
            at INTEGER NOT NULL);
    ",
},
    Migration {
        version: 2,
        name: "tasks",
        sql: "
        -- 任務板：每個任務屬於一個群；指派對象必須是群成員。
        CREATE TABLE chat_tasks(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            room INTEGER NOT NULL REFERENCES chat_rooms(id) ON DELETE CASCADE,
            title TEXT NOT NULL,
            spec TEXT NOT NULL DEFAULT '',
            owner TEXT NOT NULL,
            assignee TEXT,
            state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','claimed','done','blocked','cancelled')),
            lease_until INTEGER,
            result TEXT NOT NULL DEFAULT '',
            message INTEGER,                -- 建立時貼在群裡的那則訊息
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL);
        CREATE INDEX chat_tasks_room ON chat_tasks(room, state);
        ALTER TABLE chat_messages ADD COLUMN task_id INTEGER;
    ",
    },
    Migration {
        version: 3,
        name: "attachments",
        sql: "
        -- 附件：內容存 <instance>/var/attachments/<sha256>（內容定址、去重），DB 只存中繼資料。
        -- 先上傳（message 為 NULL），發訊息時綁定；未綁定的超過一天由背景工作清掉。
        CREATE TABLE chat_attachments(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            room INTEGER NOT NULL REFERENCES chat_rooms(id) ON DELETE CASCADE,
            message INTEGER REFERENCES chat_messages(id) ON DELETE CASCADE,
            uploader TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('image','file')),
            name TEXT NOT NULL,
            mime TEXT NOT NULL,
            size INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            width INTEGER,
            height INTEGER,
            created_at INTEGER NOT NULL);
        CREATE INDEX chat_attachments_msg ON chat_attachments(message);
        CREATE INDEX chat_attachments_sha ON chat_attachments(sha256);
    ",
    },
    Migration {
        version: 4,
        name: "approval",
        sql: "
        -- 群自動核准：開啟時 agent 的權限請求（ACP request_permission）自動選允許一次。
        -- until：YYYY-MM-DD（含當日，AAF_TZ）；NULL＝不設到期，手動關閉為止。
        CREATE TABLE chat_room_approval(
            room INTEGER PRIMARY KEY REFERENCES chat_rooms(id) ON DELETE CASCADE,
            enabled INTEGER NOT NULL,
            until TEXT,
            actor TEXT NOT NULL,
            changed_at INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 5,
        name: "search",
        sql: "
        -- 全文搜尋：trigram 斷詞（中英文子字串皆可，3 字以上走索引；更短的查詢由程式改用 LIKE）。
        CREATE VIRTUAL TABLE chat_messages_fts USING fts5(body, content='chat_messages', content_rowid='id', tokenize='trigram');
        INSERT INTO chat_messages_fts(rowid, body) SELECT id, body FROM chat_messages;
        CREATE TRIGGER chat_messages_fts_ai AFTER INSERT ON chat_messages BEGIN
            INSERT INTO chat_messages_fts(rowid, body) VALUES (new.id, new.body);
        END;
        CREATE TRIGGER chat_messages_fts_ad AFTER DELETE ON chat_messages BEGIN
            INSERT INTO chat_messages_fts(chat_messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
        END;
        CREATE TRIGGER chat_messages_fts_au AFTER UPDATE OF body ON chat_messages BEGIN
            INSERT INTO chat_messages_fts(chat_messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
            INSERT INTO chat_messages_fts(rowid, body) VALUES (new.id, new.body);
        END;
    ",
    },
    Migration {
        version: 6,
        name: "task_wakes",
        sql: "
        -- 停住的任務被手動叫醒的紀錄（同一任務 N 分鐘內不重複叫醒）。
        CREATE TABLE chat_task_wakes(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            task INTEGER NOT NULL REFERENCES chat_tasks(id) ON DELETE CASCADE,
            target TEXT NOT NULL,
            by TEXT NOT NULL,
            message INTEGER,
            at INTEGER NOT NULL);
        CREATE INDEX chat_task_wakes_task ON chat_task_wakes(task, at);
    ",
    },
    Migration {
        version: 7,
        name: "room_leads",
        sql: "
        -- 群級 lead：每群 0 或 1 位，由 owner 在群面板指定；有指定時以群為準。
        CREATE TABLE chat_room_leads(
            room INTEGER PRIMARY KEY REFERENCES chat_rooms(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            by TEXT NOT NULL,
            at INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 8,
        name: "settings",
        sql: "
        -- 在頁面上改的設定（優先於 .aaf.env／環境變數）；沒有這列＝用設定檔的值。
        CREATE TABLE chat_settings(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            by TEXT NOT NULL,
            at INTEGER NOT NULL);
    ",
    },
];
