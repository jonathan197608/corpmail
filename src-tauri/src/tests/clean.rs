//! clean 模块单元测试（自 src/clean.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::state::AppState;
use std::path::PathBuf;

fn temp_state(tag: &str) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("corpmail-m5-clean-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (AppState::open(&dir), dir)
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// 直接插一封邮件（绕过业务层；测试造过期数据）
fn insert_email(state: &AppState, folder: &str, direction: &str, status: &str, time: i64) -> i64 {
    state.with_conn(|conn| {
        conn.query_row(
            "INSERT INTO emails (direction, folder, status, subject, from_addr, to_addrs,
                                      subject_tokens, body_tokens, created_at, sent_at, received_at)
                 VALUES (?1, ?2, ?3, '测试主题', 'a@x.cn', 'b@x.cn', '测试 主题', '正文',
                         ?4, ?5, ?6)
                 RETURNING id",
            rusqlite::params![direction, folder, status, time, time, time],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    })
}

/// 插一条独立文件附件并落盘真实文件
fn insert_file_attachment(state: &AppState, email_id: i64, rel: &str, bytes: &[u8]) {
    let path = state.data_dir.join("attachments").join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    state
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO attachments (email_id, filename, size_orig, size_gz, storage, file_path, sha256)
                 VALUES (?1, 'a.bin', ?2, ?2, 'file', ?3, 'deadbeef')",
                rusqlite::params![email_id, bytes.len() as i64, rel],
            )
            .unwrap()
        });
}

fn set_days(state: &AppState, clean: &str, inbox: &str, sent: &str) {
    state
        .set_batch(&[
            (keys::CLEAN_SENT_BULK_DAYS, clean),
            (keys::KEEP_INBOX_DAYS, inbox),
            (keys::KEEP_SENT_DAYS, sent),
        ])
        .unwrap();
}

fn detached_of(state: &AppState, email_id: i64) -> Vec<i64> {
    state.with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT detached FROM attachments WHERE email_id = ?1 ORDER BY id")
            .unwrap();
        stmt.query_map([email_id], |r| r.get::<_, i64>(0))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect()
    })
}

fn email_count(state: &AppState) -> i64 {
    state.with_conn(|conn| {
        conn.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap()
    })
}

fn attach_file_exists(state: &AppState, rel: &str) -> bool {
    state.data_dir.join("attachments").join(rel).exists()
}

/// 把文件 mtime 拨回 2 小时前（过孤儿宽限期）
fn age_file(state: &AppState, rel: &str) {
    let path = state.data_dir.join("attachments").join(rel);
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * ORPHAN_GRACE_SECS);
    f.set_modified(old).unwrap();
}

#[test]
fn clean_detaches_sent_bulk_by_days_and_respects_blank() {
    let (state, dir) = temp_state("detach");
    let now = now_epoch();
    // 已发送 10 天的邮件 + 30 字节独立附件
    let old = insert_email(&state, "sent", "out", "sent", now - 10 * 86_400);
    insert_file_attachment(&state, old, "2026-09/1/1_a.bin.gz", &[7u8; 30]);
    // 3 天前发送的邮件（5 天策略内不清理）
    let recent = insert_email(&state, "sent", "out", "sent", now - 3 * 86_400);
    insert_file_attachment(&state, recent, "2026-10/2/1_a.bin.gz", &[7u8; 30]);

    set_days(&state, "5", "", "");
    let report = run(&state).unwrap();
    assert_eq!(report.detached_attachments, 1, "只清 10 天前那封");
    assert_eq!(detached_of(&state, old), vec![1], "超期附件应置 detached");
    assert_eq!(detached_of(&state, recent), vec![0], "策略内附件不动");
    assert!(
        !attach_file_exists(&state, "2026-09/1/1_a.bin.gz"),
        "文件应删除"
    );
    assert!(attach_file_exists(&state, "2026-10/2/1_a.bin.gz"));
    assert_eq!(report.freed_bytes, 30, "释放字节数 = 文件实际大小");

    // 留空 = 永久保留：再跑不动 recent 那封
    set_days(&state, "", "", "");
    let report2 = run(&state).unwrap();
    assert_eq!(report2.detached_attachments, 0, "留空应永久保留");

    // 0 = 发送成功即清理
    set_days(&state, "0", "", "");
    let report3 = run(&state).unwrap();
    assert_eq!(report3.detached_attachments, 1, "0 天应立即清理 recent");
    assert!(!attach_file_exists(&state, "2026-10/2/1_a.bin.gz"));
    cleanup(&dir);
}

#[test]
fn clean_removes_expired_emails_but_never_drafts() {
    let (state, dir) = temp_state("expire");
    let now = now_epoch();
    // inbox 40 天前（30 天策略超期）+ file 附件
    let inbox_old = insert_email(&state, "inbox", "in", "received", now - 40 * 86_400);
    insert_file_attachment(&state, inbox_old, "2026-09/1/1_a.bin.gz", &[7u8; 30]);
    // inbox 10 天前（策略内）
    // inbox 10 天前（策略内）；sent 60 天前 keep_sent 留空不删；草稿 100 天前永不删
    insert_email(&state, "inbox", "in", "received", now - 10 * 86_400);
    insert_email(&state, "sent", "out", "sent", now - 60 * 86_400);
    insert_email(&state, "draft", "out", "draft", now - 100 * 86_400);

    set_days(&state, "", "30", "");
    let report = run(&state).unwrap();
    assert_eq!(report.removed_emails, 1, "只删超期 inbox 一封");
    assert_eq!(email_count(&state), 3, "其余三封保留");
    assert!(
        !attach_file_exists(&state, "2026-09/1/1_a.bin.gz"),
        "删邮件连带附件文件"
    );
    // FTS 触发器同步
    let fts: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM email_fts JOIN emails e ON e.id = email_fts.rowid",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(fts, 3, "FTS 行随邮件删除同步");

    // keep_sent=30：60 天前的 sent 也删，草稿仍保留
    set_days(&state, "", "30", "30");
    let report2 = run(&state).unwrap();
    assert_eq!(report2.removed_emails, 1, "删超期 sent");
    assert_eq!(email_count(&state), 2, "inbox_new 与草稿保留");
    let remaining: Vec<String> = state.with_conn(|c| {
        let mut stmt = c.prepare("SELECT folder FROM emails ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect()
    });
    assert!(
        remaining.contains(&"draft".to_string()),
        "草稿永不自动删：{remaining:?}"
    );
    assert!(
        remaining.contains(&"inbox".to_string()),
        "策略内 inbox 保留：{remaining:?}"
    );
    cleanup(&dir);
}

#[test]
fn clean_sweeps_stale_orphans_but_keeps_fresh_and_referenced() {
    let (state, dir) = temp_state("orphan");
    // 有引用文件（过宽限期）→ 保留
    let email = insert_email(&state, "sent", "out", "sent", now_epoch());
    insert_file_attachment(&state, email, "2026-10/1/1_ref.bin.gz", &[7u8; 30]);
    age_file(&state, "2026-10/1/1_ref.bin.gz");
    // 无引用孤儿（过宽限期）→ 回收
    insert_file_attachment(&state, email, "2026-10/1/2_orphan.bin.gz", &[7u8; 40]);
    // 制造孤儿：删附件行留文件
    state.with_conn(|c| {
        c.execute(
            "DELETE FROM attachments WHERE file_path = '2026-10/1/2_orphan.bin.gz'",
            [],
        )
        .unwrap();
    });
    age_file(&state, "2026-10/1/2_orphan.bin.gz");
    // 刚落盘无引用（宽限期内）→ 本轮跳过
    insert_file_attachment(&state, email, "2026-10/1/3_fresh.bin.gz", &[7u8; 50]);
    state.with_conn(|c| {
        c.execute(
            "DELETE FROM attachments WHERE file_path = '2026-10/1/3_fresh.bin.gz'",
            [],
        )
        .unwrap();
    });

    set_days(&state, "", "", "");
    let report = run(&state).unwrap();
    assert_eq!(report.orphan_files, 1, "只回收过宽限期的孤儿");
    assert_eq!(report.freed_bytes, 40);
    assert!(
        attach_file_exists(&state, "2026-10/1/1_ref.bin.gz"),
        "有引用保留"
    );
    assert!(
        attach_file_exists(&state, "2026-10/1/3_fresh.bin.gz"),
        "宽限期内保留"
    );
    assert!(
        !attach_file_exists(&state, "2026-10/1/2_orphan.bin.gz"),
        "孤儿回收"
    );

    // detached 残留文件也由孤儿回收兜底清掉（fresh 仍在宽限期内不动）
    state.with_conn(|c| {
        c.execute("UPDATE attachments SET detached = 1", [])
            .unwrap();
    });
    let report2 = run(&state).unwrap();
    assert_eq!(report2.orphan_files, 1, "detached 残留由孤儿回收兜底");
    assert!(
        !attach_file_exists(&state, "2026-10/1/1_ref.bin.gz"),
        "detached 残留应回收"
    );
    assert!(
        attach_file_exists(&state, "2026-10/1/3_fresh.bin.gz"),
        "宽限期内仍保留"
    );
    cleanup(&dir);
}

#[test]
fn estimate_matches_pending_cleanup() {
    let (state, dir) = temp_state("estimate");
    let now = now_epoch();
    let old = insert_email(&state, "sent", "out", "sent", now - 10 * 86_400);
    insert_file_attachment(&state, old, "2026-09/1/1_a.bin.gz", &[7u8; 30]);
    insert_file_attachment(&state, old, "2026-09/1/2_orphan.bin.gz", &[7u8; 40]);
    state.with_conn(|c| {
        c.execute(
            "DELETE FROM attachments WHERE file_path = '2026-09/1/2_orphan.bin.gz'",
            [],
        )
        .unwrap();
    });
    age_file(&state, "2026-09/1/2_orphan.bin.gz");

    set_days(&state, "5", "", "");
    let est = estimate(&state);
    assert_eq!(est.detach_files, 1, "一个大附件待清理");
    assert_eq!(est.orphan_files, 1, "一个孤儿文件");
    assert_eq!(est.bytes, 70, "30 + 40");
    assert_eq!(est.expire_emails, 0, "keep 留空不删邮件");

    let report = run(&state).unwrap();
    assert_eq!(report.freed_bytes, est.bytes, "实际释放应与预估一致");
    assert_eq!(report.detached_attachments, est.detach_files);
    assert_eq!(report.orphan_files, est.orphan_files);
    // 再估：已清空
    let est2 = estimate(&state);
    assert_eq!(est2, Estimate::default(), "清理后预估归零");
    cleanup(&dir);
}
