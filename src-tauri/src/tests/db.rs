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
    assert_eq!(v, 3, "新库应一步建到当前版本");
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
    assert_eq!(v, 3);
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
