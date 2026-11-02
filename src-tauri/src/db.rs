//! SQLite 存储层（设计 §3/§4.2）
//!
//! - 单文件 `corpmail.db` 于 app_data_dir；`Mutex<Connection>` 全局单写者（§13-R6）
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
    let mut conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // WAL 自动 checkpoint 阈值（页）：默认 1000（≈4MB）即可，行此 pragma
    // 是显式固化该行为——防将来依赖环境差异。本应用无长读事务
    // （所有 with_conn 闭包均为短查询），autocheckpoint 不会被持续推迟
    conn.pragma_update(None, "wal_autocheckpoint", 1000)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// schema 版本（结构性变更时 +1 并在 MIGRATIONS 追加增量）
const SCHEMA_VERSION: i64 = 4;

/// 增量迁移表：`MIGRATIONS[n]` = 从版本 n+1 升到 n+2 的 DDL
/// （只允许不可回退的加列/建索引/建表）
const MIGRATIONS: &[&str] = &[
    // v1 → v2：附件邮箱外键索引（级联删/详情附件查询免全表扫）
    // + AU 触发器收窄为列限定（unread/status 等更新不再引发 FTS 重写 churn）
    r#"
    CREATE INDEX IF NOT EXISTS idx_attach_email ON attachments(email_id);
    DROP TRIGGER IF EXISTS emails_au;
    CREATE TRIGGER emails_au AFTER UPDATE OF subject_tokens, body_tokens ON emails BEGIN
      INSERT INTO email_fts(email_fts, rowid, subject, body_text)
        VALUES('delete', old.id, old.subject_tokens, old.body_tokens);
      INSERT INTO email_fts(rowid, subject, body_text) VALUES (new.id, new.subject_tokens, new.body_tokens);
    END;
    "#,
];

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v >= SCHEMA_VERSION {
        return Ok(());
    }
    // DDL 与 user_version 必须同进同退：旧实现分开提交，首次启动建库途中被杀
    // （关机/强退）→ 表已建而 user_version 仍为 0 → 下次启动 CREATE TABLE 冲突
    // → 打不开库 → 应用永久起不来（用户只能删库，等于清空数据）。
    // SQLite 的 DDL 支持事务，整体包进 transaction 后中断即完整回滚。
    let tx = conn.transaction()?;
    if v == 0 {
        // 新库：v1 基线 + 全部增量一次性建到当前版本
        tx.execute_batch(SCHEMA_V1)?;
    }
    // 增量逐级应用（v0 建完基线后从 v1 继续）；每级独立函数，便于做
    // "列是否已存在"这类幂等判断（SQLite 的 ADD COLUMN 无 IF NOT EXISTS）
    for from in (if v == 0 { 1 } else { v })..SCHEMA_VERSION {
        apply_step(&tx, from)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

/// 应用一级增量（from → from+1）
fn apply_step(tx: &Connection, from: i64) -> rusqlite::Result<()> {
    match from {
        1 => tx.execute_batch(MIGRATIONS[0]),
        2 => migrate_v3(tx),
        3 => migrate_v4(tx),
        _ => Ok(()),
    }
}

/// v2 → v3：收件大小上限的占位标记 + 已读回写批处理标志。
/// 加列用"先查 table_info 再 ALTER"（SQLite 不支持 ADD COLUMN IF NOT EXISTS）：
/// 事务已能保证原子性，这里再兜一层——历史上出现过进程被杀后残留半迁移库。
fn migrate_v3(tx: &Connection) -> rusqlite::Result<()> {
    let has_column = |name: &str| -> rusqlite::Result<bool> {
        let mut stmt = tx.prepare("PRAGMA table_info(emails)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let col: String = row.get(1)?;
            if col == name {
                return Ok(true);
            }
        }
        Ok(false)
    };
    // too_large：1 = 邮件超过单封上限，只拉了信头（正文为占位说明）
    if !has_column("too_large")? {
        tx.execute_batch("ALTER TABLE emails ADD COLUMN too_large INTEGER NOT NULL DEFAULT 0")?;
    }
    // seen_pending：1 = 本地已读但服务器 \Seen 尚未回写成功（worker 消费）
    if !has_column("seen_pending")? {
        tx.execute_batch("ALTER TABLE emails ADD COLUMN seen_pending INTEGER NOT NULL DEFAULT 0")?;
    }
    // 部分索引：worker 每轮只扫待回写行，不为历史邮件做全表扫
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_seen_pending ON emails(seen_pending)
         WHERE seen_pending = 1",
    )
}

/// v3 → v4：联系人库（姓名 + 邮件地址），自动采集收/发件地址，
/// 撰写页按姓名或地址模糊匹配。addr 主键 NOCASE：大小写不敏感去重。
/// name 索引支持按姓名检索（LIKE 前缀/包含仍走扫描，但 8 条 LIMIT 下可接受）。
fn migrate_v4(tx: &Connection) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS contacts (
           addr       TEXT PRIMARY KEY COLLATE NOCASE,
           name       TEXT NOT NULL DEFAULT '',
           updated_at INTEGER NOT NULL DEFAULT 0,
           hits       INTEGER NOT NULL DEFAULT 1
         );
         CREATE INDEX IF NOT EXISTS idx_contacts_name ON contacts(name);",
    )
}

/// 初始 schema（设计 §3 DDL + §4.2 触发器，v1）。
///
/// 全部语句 `IF NOT EXISTS`：与 migrate 的事务配合，使"建库途中被杀 → 重入"
/// 成为幂等操作（即使事务未能完整回滚，残留对象也不会阻断下次启动）。
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS emails (
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
CREATE UNIQUE INDEX IF NOT EXISTS idx_remote ON emails(remote_uid);
CREATE INDEX IF NOT EXISTS idx_folder_time ON emails(folder, created_at DESC);

CREATE TABLE IF NOT EXISTS attachments (
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
CREATE VIRTUAL TABLE IF NOT EXISTS email_fts USING fts5(
  subject, body_text,
  content='emails', content_rowid='id',
  tokenize='unicode61', prefix='2 3'
);
-- 触发器：emails 行变化时同步 FTS。
-- 索引源为预分词影子列 subject_tokens/body_tokens（设计 §4.1：分词在应用层完成，
-- SQL 触发器无法调 jieba，故由写入方先分词落列、触发器搬运列值进 FTS）。
-- 外部内容表的 'delete' 命令要求提供与建索引时相同的值 → 删除同样取影子列。
CREATE TRIGGER IF NOT EXISTS emails_ai AFTER INSERT ON emails BEGIN
  INSERT INTO email_fts(rowid, subject, body_text) VALUES (new.id, new.subject_tokens, new.body_tokens);
END;
CREATE TRIGGER IF NOT EXISTS emails_ad AFTER DELETE ON emails BEGIN
  INSERT INTO email_fts(email_fts, rowid, subject, body_text)
    VALUES('delete', old.id, old.subject_tokens, old.body_tokens);
END;
CREATE TRIGGER IF NOT EXISTS emails_au AFTER UPDATE OF subject_tokens, body_tokens ON emails BEGIN
  INSERT INTO email_fts(email_fts, rowid, subject, body_text)
    VALUES('delete', old.id, old.subject_tokens, old.body_tokens);
  INSERT INTO email_fts(rowid, subject, body_text) VALUES (new.id, new.subject_tokens, new.body_tokens);
END;

CREATE TABLE IF NOT EXISTS settings (k TEXT PRIMARY KEY, v TEXT NOT NULL);
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
#[path = "tests/db.rs"]
mod tests;
