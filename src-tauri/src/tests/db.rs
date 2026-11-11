//! db 模块单元测试（自 src/db.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;

fn mem() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrate(&mut conn).unwrap();
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
fn migrate_v1_to_v2_adds_index_and_narrow_trigger() {
    let conn = mem();
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION, "新库应一步建到当前版本");
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_attach_email'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "附件外键索引应存在");
    // 列限定 AU：仅更新 unread 不得触发 FTS 重写（性能：已读标记高频）
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                            subject_tokens, body_tokens, unread, created_at)
         VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', '清单',
                 '清单', '月租 单', 1, 1)",
        [],
    )
    .unwrap();
    // 塞入两条 FTS 行后改 unread：行数应保持 1（未走 delete+insert，也无残留）
    conn.execute("UPDATE emails SET unread = 0 WHERE id = 1", [])
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM email_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "unread 更新不应触发 FTS churn");
    // 影子列更新仍同步
    conn.execute(
        "UPDATE emails SET subject_tokens = '配案 清单' WHERE id = 1",
        [],
    )
    .unwrap();
    let hit: i64 = conn
        .query_row(
            "SELECT count(*) FROM email_fts WHERE email_fts MATCH '配案'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hit, 1, "影子列更新仍应同步 FTS");
}

#[test]
fn migrate_old_v1_db_upgrades_in_place() {
    // 模拟旧库：建 v1（无索引、无条件 AU 触发器）后跑 migrate
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    migrate(&mut conn).unwrap();
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_attach_email'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "存量库升级应补附件索引");
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
}

#[test]
fn migrate_is_idempotent() {
    let mut conn = mem();
    migrate(&mut conn).unwrap(); // 第二次应无副作用
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
}

/// 首次建库途中被杀：v1 语句已逐条执行完（旧实现每条自动提交），
/// 但 user_version 尚未写入 → 下次启动 user_version 仍为 0。
/// 旧实现重跑即 "table emails already exists"，打不开库 = 应用永久起不来
#[test]
fn migrate_recovers_from_interrupted_first_init() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.pragma_update(None, "user_version", 0).unwrap();
    migrate(&mut conn).unwrap();
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION, "中断残留不应阻断迁移完成");
}

/// v2 → v3 增量：加 too_large/seen_pending 两列 + 待回写部分索引，
/// 存量邮件取默认值（0/0）；重复迁移幂等不报错
#[test]
fn migrate_v2_to_v3_adds_receive_columns() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    conn.pragma_update(None, "user_version", 2i64).unwrap();
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, created_at)
         VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', 1)",
        [],
    )
    .unwrap();

    migrate(&mut conn).unwrap();
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    let (too_large, pending): (i64, i64) = conn
        .query_row("SELECT too_large, seen_pending FROM emails", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((too_large, pending), (0, 0), "存量邮件取默认值");
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_seen_pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "待回写部分索引应存在");
    // 幂等：版本已达标，再调用直接返回
    migrate(&mut conn).unwrap();
}

/// v5 → v6 增量（M1-3 密送）：加 bcc_addrs 列，存量邮件默认空串
/// （收到的邮件恒无密送）；v5 库结构 = v1+MIGRATIONS[0]+v3/v4/v5 各列
#[test]
fn migrate_v5_to_v6_adds_bcc_column() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    conn.execute_batch(
        "ALTER TABLE emails ADD COLUMN too_large INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE emails ADD COLUMN seen_pending INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX idx_seen_pending ON emails(seen_pending) WHERE seen_pending = 1;
         CREATE TABLE contacts (
           addr TEXT PRIMARY KEY COLLATE NOCASE, name TEXT NOT NULL DEFAULT '',
           updated_at INTEGER NOT NULL DEFAULT 0, hits INTEGER NOT NULL DEFAULT 1);
         CREATE INDEX idx_contacts_name ON contacts(name);
         ALTER TABLE emails ADD COLUMN body_html TEXT;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 5i64).unwrap();
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, created_at)
         VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', 1)",
        [],
    )
    .unwrap();

    migrate(&mut conn).unwrap();

    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    let bcc: String = conn
        .query_row("SELECT bcc_addrs FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bcc, "", "存量邮件密送默认空串");
    // 新库亦一步建到 v6（含列）
    let fresh = mem();
    let cols: i64 = fresh
        .query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name='bcc_addrs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols, 1, "新库应直接含 bcc_addrs 列");
}

/// v6 → v7 增量（M2-1 星标）：加 starred 列，存量邮件默认 0（未星标）；
/// 翻转取反语义由业务层（toggle_star）承担，迁移只铺列
#[test]
fn migrate_v6_to_v7_adds_starred_column() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    conn.execute_batch(
        "ALTER TABLE emails ADD COLUMN too_large INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE emails ADD COLUMN seen_pending INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX idx_seen_pending ON emails(seen_pending) WHERE seen_pending = 1;
         CREATE TABLE contacts (
           addr TEXT PRIMARY KEY COLLATE NOCASE, name TEXT NOT NULL DEFAULT '',
           updated_at INTEGER NOT NULL DEFAULT 0, hits INTEGER NOT NULL DEFAULT 1);
         CREATE INDEX idx_contacts_name ON contacts(name);
         ALTER TABLE emails ADD COLUMN body_html TEXT;
         ALTER TABLE emails ADD COLUMN bcc_addrs TEXT NOT NULL DEFAULT '';",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 6i64).unwrap();
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, created_at)
         VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', 1)",
        [],
    )
    .unwrap();

    migrate(&mut conn).unwrap();

    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    let starred: i64 = conn
        .query_row("SELECT starred FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(starred, 0, "存量邮件默认未星标");
    // 取反语句在 v7 库上直接可用（业务层 toggle_star 的 SQL 形态）
    conn.execute("UPDATE emails SET starred = 1 - starred WHERE id = 1", [])
        .unwrap();
    let flipped: i64 = conn
        .query_row("SELECT starred FROM emails WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(flipped, 1, "星标取反语句应可用");
    // 新库亦一步建到 v7（含列）
    let fresh = mem();
    let cols: i64 = fresh
        .query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name='starred'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols, 1, "新库应直接含 starred 列");
}

/// v7 → v8 增量（M2-2 会话视图）：加 thread_key 列（TEXT 可空）+ idx_thread_key 索引。
/// 存量行迁移后默认 NULL（启动时 backfill_thread_keys 补齐）；新库直接建到 v8
#[test]
fn migrate_v7_to_v8_adds_thread_key_column() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    conn.execute_batch(
        "ALTER TABLE emails ADD COLUMN too_large INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE emails ADD COLUMN seen_pending INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX idx_seen_pending ON emails(seen_pending) WHERE seen_pending = 1;
         CREATE TABLE contacts (
           addr TEXT PRIMARY KEY COLLATE NOCASE, name TEXT NOT NULL DEFAULT '',
           updated_at INTEGER NOT NULL DEFAULT 0, hits INTEGER NOT NULL DEFAULT 1);
         CREATE INDEX idx_contacts_name ON contacts(name);
         ALTER TABLE emails ADD COLUMN body_html TEXT;
         ALTER TABLE emails ADD COLUMN bcc_addrs TEXT NOT NULL DEFAULT '';
         ALTER TABLE emails ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 7i64).unwrap();
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject, created_at)
         VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', '会议', 1)",
        [],
    )
    .unwrap();

    migrate(&mut conn).unwrap();

    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    // 存量行迁移后 thread_key 为 NULL（待回填），列存在
    let cols: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name='thread_key'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols, 1, "v8 应有 thread_key 列");
    let key: Option<String> = conn
        .query_row("SELECT thread_key FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(key, None, "存量行迁移后 thread_key 应为 NULL（待回填）");
    // 索引存在
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_thread_key'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "应建 idx_thread_key 索引");
    // 新库一步建到 v8（含列）
    let fresh = mem();
    let cols: i64 = fresh
        .query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name='thread_key'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols, 1, "新库应直接含 thread_key 列");
}

/// v8 → v9 增量（M4-2 定时发送）：加 scheduled_at 列（NOT NULL DEFAULT 0）
/// + idx_scheduled_due 部分索引（只收 scheduled_at > 0 行，服务于调度扫描）
#[test]
fn migrate_v8_to_v9_adds_scheduled_at_column() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    // 沿 v2→v8 逐级铺设（与 v7→v8 测试同法，DDL 取自各迁移实体）
    conn.execute_batch(
        "ALTER TABLE emails ADD COLUMN too_large INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE emails ADD COLUMN seen_pending INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX idx_seen_pending ON emails(seen_pending) WHERE seen_pending = 1;
         CREATE TABLE contacts (
           addr TEXT PRIMARY KEY COLLATE NOCASE, name TEXT NOT NULL DEFAULT '',
           updated_at INTEGER NOT NULL DEFAULT 0, hits INTEGER NOT NULL DEFAULT 1);
         CREATE INDEX idx_contacts_name ON contacts(name);
         ALTER TABLE emails ADD COLUMN body_html TEXT;
         ALTER TABLE emails ADD COLUMN bcc_addrs TEXT NOT NULL DEFAULT '';
         ALTER TABLE emails ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE emails ADD COLUMN thread_key TEXT;
         CREATE INDEX idx_thread_key ON emails(thread_key);",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 8i64).unwrap();
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, created_at)
         VALUES ('out', 'draft', 'draft', 'a@x.cn', 'b@x.cn', 1)",
        [],
    )
    .unwrap();

    migrate(&mut conn).unwrap();

    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    let cols: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name='scheduled_at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols, 1, "v9 应有 scheduled_at 列");
    let at: i64 = conn
        .query_row("SELECT scheduled_at FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(at, 0, "存量行默认非定时");
    // 部分索引：手动写定时值可见于索引；派发态（0）不进索引
    conn.execute("UPDATE emails SET scheduled_at = 12345", [])
        .unwrap();
    let in_idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM emails WHERE scheduled_at > 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(in_idx, 1);
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_scheduled_due'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "应建 idx_scheduled_due 部分索引");
    // 新库一步建到 v9（含列）
    let fresh = mem();
    let cols: i64 = fresh
        .query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name='scheduled_at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols, 1, "新库应直接含 scheduled_at 列");
}

/// v9 → v10 增量（A5 原文排查 + B2 邮件模板）：加 raw_headers 列
/// （NOT NULL DEFAULT ''，存量行为空串）+ 建 templates 表
#[test]
fn migrate_v9_to_v10_adds_raw_headers_and_templates() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    // 沿 v2→v9 逐级铺设（DDL 取自各迁移实体）
    conn.execute_batch(
        "ALTER TABLE emails ADD COLUMN too_large INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE emails ADD COLUMN seen_pending INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX idx_seen_pending ON emails(seen_pending) WHERE seen_pending = 1;
         CREATE TABLE contacts (
           addr TEXT PRIMARY KEY COLLATE NOCASE, name TEXT NOT NULL DEFAULT '',
           updated_at INTEGER NOT NULL DEFAULT 0, hits INTEGER NOT NULL DEFAULT 1);
         CREATE INDEX idx_contacts_name ON contacts(name);
         ALTER TABLE emails ADD COLUMN body_html TEXT;
         ALTER TABLE emails ADD COLUMN bcc_addrs TEXT NOT NULL DEFAULT '';
         ALTER TABLE emails ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE emails ADD COLUMN thread_key TEXT;
         CREATE INDEX idx_thread_key ON emails(thread_key);
         ALTER TABLE emails ADD COLUMN scheduled_at INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX idx_scheduled_due ON emails(scheduled_at) WHERE scheduled_at > 0;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 9i64).unwrap();
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, created_at)
         VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', 1)",
        [],
    )
    .unwrap();

    migrate(&mut conn).unwrap();

    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    let cols: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name='raw_headers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols, 1, "v10 应有 raw_headers 列");
    let raw: String = conn
        .query_row("SELECT raw_headers FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw, "", "存量行信头为空串（源码页提示旧版本未保存）");
    // templates 表可用（B2 后续项依赖）
    conn.execute(
        "INSERT INTO templates(name, body_text, body_html, created_at, updated_at)
         VALUES ('月度报送', '请查收本月清单。', '<p>请查收本月清单。</p>', 1, 1)",
        [],
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM templates", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "templates 表应可写入");
    // 新库一步建到 v10（含列与表）
    let fresh = mem();
    let cols: i64 = fresh
        .query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name='raw_headers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols, 1, "新库应直接含 raw_headers 列");
    let tbl: i64 = fresh
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='templates'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tbl, 1, "新库应直接含 templates 表");
}

#[test]
fn open_sets_busy_timeout_on_every_connection() {
    let dir = std::env::temp_dir().join(format!("mbx-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("corpmail.db");
    let mut a = open(&path).unwrap();
    let ms: i64 = a
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ms, 10_000, "所有经 db::open 的连接都要有 10s 等锁窗口");
    // A 持写事务时 B 的写入应等待，而不是立即 SQLITE_BUSY 被上层吞掉
    let b = open(&path).unwrap();
    a.execute("CREATE TABLE IF NOT EXISTS t(x)", []).unwrap();
    let tx = a.transaction().unwrap();
    tx.execute("INSERT INTO t VALUES (1)", []).unwrap();
    let writer = std::thread::spawn(move || b.execute("INSERT INTO t VALUES (2)", []).map(|_| ()));
    std::thread::sleep(std::time::Duration::from_millis(150));
    tx.commit().unwrap();
    assert!(
        writer.join().unwrap().is_ok(),
        "BUSY 应在 busy_timeout 窗口内等待到锁释放"
    );
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}
