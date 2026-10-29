//! SQLite 存储层（设计 §3/§4.2）
//!
//! - 单文件 `mailbox.db` 于 app_data_dir；`Mutex<Connection>` 全局单写者（§13-R6）
//! - WAL + NORMAL：读写不互阻（spike 实测写入吞吐足够）
//! - schema 含 emails / attachments / email_fts（FTS5 外部内容表）/ settings
//! - FTS5 行与 emails 行的一致性由 AI/AD/AU 触发器保证（事务内同步）
//! - 时间戳一律 epoch 秒（展示层 jiff 格式化）

use rusqlite::Connection;
use std::path::Path;

/// 打开（不存在则创建）数据库并完成 PRAGMA 与 schema 初始化。
///
/// 幂等：`user_version` 标记 schema 版本，未升级则跳过 DDL。
pub fn open(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

/// schema 版本（结构性变更时 +1 并在 MIGRATIONS 追加增量）
const SCHEMA_VERSION: i64 = 1;

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v >= SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch(SCHEMA_V1)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
}

/// 初始 schema（设计 §3 DDL + §4.2 触发器，v1）
const SCHEMA_V1: &str = r#"
CREATE TABLE emails (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  direction     TEXT NOT NULL,                -- 'in' 收件 | 'out' 发出（含草稿）
  folder        TEXT NOT NULL,                -- 'inbox'|'sent'|'draft'
  status        TEXT NOT NULL DEFAULT 'draft',
                -- out: draft|queued|sending|sent|failed；in: 'received'（固定）
  subject       TEXT NOT NULL DEFAULT '',
  from_addr     TEXT NOT NULL,                -- in=对方地址；out=登录账号
  from_name     TEXT DEFAULT '',              -- in=对方显示名
  to_addrs      TEXT NOT NULL,                -- 逗号分隔纯地址
  cc_addrs      TEXT DEFAULT '',
  body_text     TEXT DEFAULT '',              -- 纯文本（检索与展示源）
  body_kind     TEXT DEFAULT 'plain',         -- 收件正文原始类型 plain|html
  subject_tokens TEXT NOT NULL DEFAULT '',    -- jieba 预分词空格串（FTS 索引源，M5 写入）
  body_tokens   TEXT NOT NULL DEFAULT '',     -- 同上
  attach_cnt    INTEGER NOT NULL DEFAULT 0,   -- 列表 📎 冗余
  attach_bytes  INTEGER NOT NULL DEFAULT 0,   -- 附件原始总大小
  unread        INTEGER NOT NULL DEFAULT 1,   -- 仅 inbox 用（0/1）
  remote_uid    TEXT,                         -- IMAP: '{uidvalidity}:{uid}'（去重键）
  message_id    TEXT,                         -- Message-ID 头（回复链定位）
  in_reply_to   INTEGER,                      -- 本地回复链（指向被回复邮件 id）
  error         TEXT,                         -- failed 原因
  created_at    INTEGER NOT NULL,             -- epoch 秒
  sent_at       INTEGER,                      -- out 终态时间 epoch 秒
  received_at   INTEGER                       -- in 收到时间 epoch 秒
);
CREATE UNIQUE INDEX idx_remote ON emails(remote_uid);
CREATE INDEX idx_folder_time ON emails(folder, created_at DESC);

CREATE TABLE attachments (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  email_id    INTEGER NOT NULL REFERENCES emails(id) ON DELETE CASCADE,
  filename    TEXT NOT NULL,
  mime        TEXT DEFAULT 'application/octet-stream',
  size_orig   INTEGER NOT NULL,
  size_gz     INTEGER NOT NULL,
  storage     TEXT NOT NULL DEFAULT 'inline',  -- 'inline' DB 内 BLOB | 'file' 独立文件
  data        BLOB,                             -- inline 时的 gzip 流
  file_path   TEXT,                             -- attachments/{YYYY-MM}/{email_id}/{n}_{name}.gz
  sha256      TEXT NOT NULL,
  detached    INTEGER NOT NULL DEFAULT 0        -- 大附件文件已被清理（元数据保留）
);

-- 全文检索：FTS5 外部内容表（不存原文副本；分词在应用层完成后空格拼接入库，设计 §4）
-- 列名与内容表 emails 对齐（subject / body_text）：FTS5 外部内容表按列名回查内容表
-- （count(*) 等路径），列名错位在 SQLite 3.45 会触发 "no such column"（3.54 已不敏感）
CREATE VIRTUAL TABLE email_fts USING fts5(
  subject, body_text,
  content='emails', content_rowid='id',
  tokenize='unicode61', prefix='2 3'
);
-- 触发器：emails 行变化时同步 FTS。
-- 索引源为预分词影子列 subject_tokens/body_tokens（设计 §4.1：分词在应用层完成，
-- SQL 触发器无法调 jieba，故由写入方先分词落列、触发器搬运列值进 FTS）。
-- 外部内容表的 'delete' 命令要求提供与建索引时相同的值 → 删除同样取影子列。
CREATE TRIGGER emails_ai AFTER INSERT ON emails BEGIN
  INSERT INTO email_fts(rowid, subject, body_text) VALUES (new.id, new.subject_tokens, new.body_tokens);
END;
CREATE TRIGGER emails_ad AFTER DELETE ON emails BEGIN
  INSERT INTO email_fts(email_fts, rowid, subject, body_text)
    VALUES('delete', old.id, old.subject_tokens, old.body_tokens);
END;
CREATE TRIGGER emails_au AFTER UPDATE ON emails BEGIN
  INSERT INTO email_fts(email_fts, rowid, subject, body_text)
    VALUES('delete', old.id, old.subject_tokens, old.body_tokens);
  INSERT INTO email_fts(rowid, subject, body_text) VALUES (new.id, new.subject_tokens, new.body_tokens);
END;

CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT NOT NULL);
"#;

/// 多行查询（prepare → query_map → collect 的共用封装；mail/clean 复用，消除散落的样板）
pub fn collect_rows<T>(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, map)?;
    rows.collect()
}

/// settings 读写（TEXT 键值，业务层负责类型转换与默认值）
pub mod settings {
    use rusqlite::{Connection, OptionalExtension};

    pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
        conn.query_row("SELECT v FROM settings WHERE k = ?1", [key], |r| r.get(0))
            .optional()
    }

    pub fn set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO settings(k, v) VALUES (?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            [key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn schema_creates_all_tables_and_triggers() {
        let conn = mem();
        for t in ["emails", "attachments", "settings", "email_fts"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "表 {t} 应存在");
        }
        for t in ["emails_ai", "emails_ad", "emails_au"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "触发器 {t} 应存在");
        }
    }

    #[test]
    fn fts_triggers_keep_index_in_sync() {
        let conn = mem();
        conn.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                subject_tokens, body_tokens, created_at)
             VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', '激励清单',
                     '激励 清单', '月租 单', 1)",
            [],
        )
        .unwrap();
        // 触发器把预分词影子列写入 FTS 索引
        let n: i64 = conn
            .query_row("SELECT count(*) FROM email_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let hit: i64 = conn
            .query_row(
                "SELECT count(*) FROM email_fts WHERE email_fts MATCH '激励'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1, "两字词应命中预分词索引（§4.1 spike 同构）");
        // 更新同步（先 delete 旧行再 insert 新行）
        conn.execute(
            "UPDATE emails SET subject = '配案', subject_tokens = '配案' WHERE id = 1",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM email_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "更新后 FTS 行数不变");
        for (word, expect) in [("激励", 0), ("配案", 1)] {
            let hit: i64 = conn
                .query_row(
                    "SELECT count(*) FROM email_fts WHERE email_fts MATCH ?1",
                    [word],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(hit, expect, "更新后 {word} 应命中 {expect} 条");
        }
        // 删除同步
        conn.execute("DELETE FROM emails WHERE id = 1", []).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM email_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn settings_roundtrip_and_upsert() {
        let conn = mem();
        assert_eq!(settings::get(&conn, "nope").unwrap(), None);
        settings::set(&conn, "account_prefix", "demo").unwrap();
        settings::set(&conn, "account_prefix", "demo2").unwrap();
        assert_eq!(
            settings::get(&conn, "account_prefix").unwrap().as_deref(),
            Some("demo2"),
            "同键再写应覆盖"
        );
    }

    #[test]
    fn attachments_cascade_with_email() {
        let conn = mem();
        conn.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, created_at)
             VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachments(email_id, filename, size_orig, size_gz, sha256)
             VALUES (1, 'a.xlsx', 10, 5, 'deadbeef')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM emails WHERE id = 1", []).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "外键级联应连带删除附件行");
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = mem();
        migrate(&conn).unwrap(); // 第二次应无副作用
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
