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

/// A2 专用临时目录：base/{data,external…} 两棵树——data 是 AppState 的数据
/// 目录，external* 放在数据目录之外（模拟软链指向的"用户外部文件"，
/// 与真实扩容场景 /Volumes/BigDisk 一致而非藏在数据目录内部）
#[cfg(unix)]
fn temp_dir_a2(tag: &str) -> (AppState, PathBuf) {
    let base = std::env::temp_dir().join(format!("corpmail-a2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let data = base.join("data");
    std::fs::create_dir_all(&data).unwrap();
    (AppState::open(&data), base)
}

/// A2 整树清理（data_dir 与外部目录同属一个临时根）
#[cfg(unix)]
fn cleanup_a2(dir: &Path) {
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
    let path = state.data_dir().join("attachments").join(rel);
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
    state.data_dir().join("attachments").join(rel).exists()
}

/// 把文件 mtime 拨回 2 小时前（过孤儿宽限期）
fn age_file(state: &AppState, rel: &str) {
    let path = state.data_dir().join("attachments").join(rel);
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

/// 收件过期判定口径（2026-11-03 回归）：按邮件自身时间（COALESCE(received_at,
/// created_at)）而非入库时刻 created_at——首拉的历史邮件 created_at 都是拉取
/// 当天，按入库时间判定时设多短保留时长都不删（用户报"不生效"）
#[test]
fn clean_expires_inbox_by_received_time_not_ingest_time() {
    let (state, dir) = temp_state("expire-recv");
    let now = now_epoch();
    // 首拉形态：今天入库（created_at=now）、邮件本身 40 天前（received_at）
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails (direction, folder, status, subject, from_addr, to_addrs,
                                 subject_tokens, body_tokens, created_at, received_at)
             VALUES ('in','inbox','received','历史件','a@x.cn','b@x.cn','历史 件','正文',?1,?2)",
                rusqlite::params![now, now - 40 * 86_400],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 同封今天入库、今天接收：保留
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails (direction, folder, status, subject, from_addr, to_addrs,
                                 subject_tokens, body_tokens, created_at, received_at)
             VALUES ('in','inbox','received','新件','a@x.cn','b@x.cn','新 件','正文',?1,?1)",
                [now],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    set_days(&state, "", "30", "");
    let report = run(&state).unwrap();
    assert_eq!(report.removed_emails, 1, "按接收时间判定的历史件应删");
    assert_eq!(email_count(&state), 1, "今天的新件保留");
    // received_at 为空的老数据（NULL）回退 created_at：40 天前入库 → 删
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails (direction, folder, status, subject, from_addr, to_addrs,
                                 subject_tokens, body_tokens, created_at)
             VALUES ('in','inbox','received','无收件时间','a@x.cn','b@x.cn','无 收件 时间','正文',?1)",
            [now - 40 * 86_400],
        )
        .unwrap();
        Ok::<(), String>(())
    })
    .unwrap();
    let report2 = run(&state).unwrap();
    assert_eq!(
        report2.removed_emails, 1,
        "NULL received_at 回退 created_at 判定"
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

/// A2：孤儿回收不得跟随符号链接删除数据目录之外的文件。
/// `attachments/2026-01 → 外部目录` 是指向数据目录之外的软链时，旧实现的
/// `path.is_dir()` 跟随链接、`remove_file` 沿链接前缀 unlink 外部真实文件
/// （audit 2026-11-16 §A2 实测复现）。要求：软链目录不递归、外部文件不删、
/// 不计孤儿数
#[cfg(unix)]
#[test]
fn orphan_sweep_never_follows_symlinked_month_dir() {
    let (state, dir) = temp_dir_a2("symlink-month");
    let root = state.data_dir().join("attachments");
    std::fs::create_dir_all(&root).unwrap();

    // 外部目录（含子目录层——不造子层时 month 枚举出的文件在 edir 层即被
    // is_dir() 拦下，走不到缺陷路径）：mtime 拨回 2 小时前（远超宽限期，
    // 旧实现必判孤儿并删除）
    let ext = dir.join("external");
    let keep = ext.join("sub").join("keep.txt");
    std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
    std::fs::write(&keep, "用户数据，绝不许动").unwrap();
    let f = std::fs::File::options().write(true).open(&keep).unwrap();
    f.set_modified(
        std::time::SystemTime::now() - std::time::Duration::from_secs(2 * ORPHAN_GRACE_SECS),
    )
    .unwrap();

    // 月度目录是软链 → 外部目录
    std::os::unix::fs::symlink(&ext, root.join("2026-01")).unwrap();

    let report = run(&state).unwrap();
    assert_eq!(report.orphan_files, 0, "软链目录下的文件不得计为孤儿");
    assert!(keep.exists(), "外部文件绝不能被删（数据丢失事故）");
    assert!(
        std::fs::symlink_metadata(root.join("2026-01"))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "软链本身保留（不删链接，宁留勿错删）"
    );
    cleanup_a2(&dir);
}

/// A2：文件级软链（`attachments/2026-01/123/1_x.gz → 外部大文件`）不得计入
/// estimate 的释放体积，也不得被孤儿回收删除
#[cfg(unix)]
#[test]
fn orphan_sweep_ignores_and_spares_symlinked_files() {
    let (state, dir) = temp_dir_a2("symlink-file");
    let root = state.data_dir().join("attachments");

    // 外部大文件（4096 字节，远超宽限期）
    let ext = dir.join("external-big.bin");
    std::fs::write(&ext, vec![7u8; 4096]).unwrap();
    let f = std::fs::File::options().write(true).open(&ext).unwrap();
    f.set_modified(
        std::time::SystemTime::now() - std::time::Duration::from_secs(2 * ORPHAN_GRACE_SECS),
    )
    .unwrap();

    // 真实目录结构中的孤儿位放一个软链 → 外部大文件
    let e_dir = root.join("2026-01/123");
    std::fs::create_dir_all(&e_dir).unwrap();
    std::os::unix::fs::symlink(&ext, e_dir.join("1_x.gz")).unwrap();
    // 同目录一个真实孤儿文件（40 字节，对照组：应被正常回收）
    let real = e_dir.join("2_orphan.bin.gz");
    std::fs::write(&real, vec![7u8; 40]).unwrap();
    let f2 = std::fs::File::options().write(true).open(&real).unwrap();
    f2.set_modified(
        std::time::SystemTime::now() - std::time::Duration::from_secs(2 * ORPHAN_GRACE_SECS),
    )
    .unwrap();

    let est = estimate(&state);
    assert_eq!(est.orphan_files, 1, "软链文件不入孤儿统计");
    assert_eq!(
        est.bytes, 40,
        "预估体积只含真实孤儿（40），不含软链目标的 4096"
    );

    let report = run(&state).unwrap();
    assert_eq!(report.orphan_files, 1, "只回收真实孤儿");
    assert!(ext.exists(), "软链指向的外部大文件绝不能被删");
    assert!(!real.exists(), "真实孤儿应被回收");
    cleanup_a2(&dir);
}

/// 复查 N1：DB `file_path` 恒用 `/` 分隔写入，而 Windows 磁盘枚举出的相对
/// 路径 `to_string_lossy()` 是 `\` 分隔。归一函数必须把 `\` 换成 `/`，
/// 才能与引用集命中——否则 Windows 上被引用附件全被误判孤儿删除。
/// 纯函数形态使本机（Unix）也能复现 Windows 口径，变异验证直接有效。
#[test]
fn normalize_rel_separator_matches_db_slash_on_windows_paths() {
    // Windows 磁盘形态：month/edir/file 全用 `\`
    assert_eq!(
        normalize_rel_separator(Path::new("2026-11\\9\\1_x.xlsx.gz")),
        "2026-11/9/1_x.xlsx.gz"
    );
    // Unix 形态不受影响（已是 `/`）
    assert_eq!(
        normalize_rel_separator(Path::new("2026-11/9/1_x.xlsx.gz")),
        "2026-11/9/1_x.xlsx.gz"
    );
    // 混合形态（外部工具写入）也归一
    assert_eq!(
        normalize_rel_separator(Path::new("2026-11/9\\1_x.xlsx.gz")),
        "2026-11/9/1_x.xlsx.gz"
    );
    // 归一结果必须落在 DB 引用集中（模拟 referenced.contains）
    let referenced: HashSet<String> = ["2026-11/9/1_x.xlsx.gz".to_string()].into();
    assert!(referenced.contains(normalize_rel_separator(Path::new("2026-11\\9\\1_x.xlsx.gz")).as_str()));
}

/// 复查 N1：端到端回归保护——磁盘上真实存在的被引用附件（mtime 超宽限期）
/// 不得被孤儿回收删除，且估计/回收两侧口径一致（estimate 与 run 共用 orphan_files）
#[test]
fn referenced_attachment_survives_orphan_sweep_after_grace() {
    let (state, dir) = temp_state("n1-referenced");
    // 邮件用未来时间，避开步骤 2 过期删除（本测试只验步骤 3 孤儿回收）
    let now = now_epoch();
    let eid = insert_email(&state, "inbox", "in", "received", now);
    let rel = "2026-11/9/1_ref.bin.gz";
    insert_file_attachment(&state, eid, rel, &[9u8; 64]); // storage='file', detached=0
    let path = state.data_dir().join("attachments").join(rel);
    // 拨回 2h（远超 1h 宽限期）：仅靠引用集命中保护，不靠 mtime
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(
        std::time::SystemTime::now() - std::time::Duration::from_secs(2 * ORPHAN_GRACE_SECS),
    )
    .unwrap();

    let est = estimate(&state);
    assert_eq!(est.orphan_files, 0, "被引用的附件不是孤儿");
    let report = run(&state).unwrap();
    assert_eq!(report.orphan_files, 0);
    assert!(path.exists(), "被引用附件超宽限期后仍必须保留（数据丢失防护）");
    cleanup(&dir);
}

#[test]
fn p45_negative_retention_days_are_clamped_not_future_cutoff() {
    // 复查四 P3-2：非 UI 途径写 -1 不得让 cutoff 跑到未来清空全库
    assert_eq!(parse_days(Some("-1".into())), Some(0));
    assert_eq!(parse_days(Some("  -365  ".into())), Some(0));
    assert_eq!(parse_days(Some("30".into())), Some(30));
    assert_eq!(parse_days(Some(String::new())), None);
    assert_eq!(parse_days(None), None);
}
