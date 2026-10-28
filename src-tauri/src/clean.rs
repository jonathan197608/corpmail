//! 存储清理器（M5，设计 §5.2）
//!
//! 三步全流程（启动时 + 每日调度 + 存储页手动触发）：
//! 1. 大附件文件清理：已发送邮件超期 → 删独立附件文件、置 `detached=1`（元数据保留，
//!    详情页注明"文件已按策略清理"并禁用另存）；`clean_sent_bulk_days = 0` → 发送成功即清理
//! 2. 过期邮件删除：inbox 按 `created_at`、sent 按 `sent_at` 超期整封硬删
//!    （触发器连 FTS 行、外键级联附件行、应用层删独立文件）；**草稿永不自动删**
//! 3. 孤儿文件回收：`attachments/` 下 DB 无引用的文件删除 + 空目录回收（崩溃残留防护）
//!
//! 天数语义（与设置页一致）：数字 = 保留天数（0 = 立即清理）；**留空/非法 = 永久保留**。
//!
//! 一致性设计：DB 变更（步骤 1 置位 / 步骤 2 删行）在单事务内提交，文件删除在提交后执行；
//! 文件删除失败或崩溃残留由步骤 3 孤儿回收兜底（detached 行不再计入引用集合）。
//! 孤儿回收带 1 小时 mtime 宽限期，防与并发收件线程的"文件已落盘、行未提交"窗口竞态。

use crate::mail::now_epoch;
use crate::state::{AppState, keys};
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

/// 清理报告（run 返回；存储页展示）
#[derive(Serialize, Default, Debug, PartialEq, Eq)]
pub struct CleanReport {
    /// 实际释放磁盘字节数（独立附件文件；inline BLOB 在库内不还磁盘、不计入）
    pub freed_bytes: u64,
    /// 步骤 1：置 detached 的附件数
    pub detached_attachments: u64,
    /// 步骤 2：整封删除的邮件数
    pub removed_emails: u64,
    /// 步骤 3：回收的孤儿文件数
    pub orphan_files: u64,
}

/// 清理预估（estimate 返回；存储页"预计可释放"明细）
#[derive(Serialize, Default, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// 预计释放字节数（合计）
    pub bytes: u64,
    /// 待清理大附件文件数（步骤 1）
    pub detach_files: u64,
    /// 待删除过期邮件数（步骤 2）
    pub expire_emails: u64,
    /// 孤儿文件数（步骤 3）
    pub orphan_files: u64,
}

/// 孤儿文件 mtime 宽限（秒）：新落盘附件不立即判孤儿，防与收件线程竞态
const ORPHAN_GRACE_SECS: u64 = 3600;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

/// 清理策略快照（锁外读取；None = 永久保留）
struct Policy {
    clean_sent_bulk_days: Option<i64>,
    keep_inbox_days: Option<i64>,
    keep_sent_days: Option<i64>,
}

/// 天数设置语义：数字（含 0 = 立即）→ Some；留空/非法/未设 → None（永久保留）
fn policy(state: &AppState) -> Policy {
    let days = |k: &str| {
        state
            .get_or(k, &crate::state::defaults())
            .and_then(|v| v.trim().parse::<i64>().ok())
    };
    Policy {
        clean_sent_bulk_days: days(keys::CLEAN_SENT_BULK_DAYS),
        keep_inbox_days: days(keys::KEEP_INBOX_DAYS),
        keep_sent_days: days(keys::KEEP_SENT_DAYS),
    }
}

fn cutoff(now: i64, days: i64) -> i64 {
    now - days.saturating_mul(86_400)
}

/// 步骤 1 目标：已发送邮件超期的未清理独立附件（(附件 id, file_path, size_gz)）
fn detach_targets(
    conn: &rusqlite::Connection,
    cutoff: Option<i64>,
) -> Result<Vec<(i64, String, i64)>, String> {
    let Some(cutoff) = cutoff else {
        return Ok(Vec::new());
    };
    let mut stmt = conn
        .prepare(
            "SELECT a.id, COALESCE(a.file_path, ''), a.size_gz
             FROM attachments a JOIN emails e ON e.id = a.email_id
             WHERE a.storage = 'file' AND a.detached = 0
               AND e.direction = 'out' AND e.status = 'sent'
               AND e.sent_at IS NOT NULL AND e.sent_at < ?1",
        )
        .map_err(e)?;
    let rows = stmt
        .query_map([cutoff], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(e)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(e)?;
    Ok(rows)
}

/// 步骤 2 目标：过期邮件 id（inbox 按 created_at、sent 按 sent_at）
fn expire_targets(
    conn: &rusqlite::Connection,
    (inbox_cut, sent_cut): (Option<i64>, Option<i64>),
) -> Result<Vec<i64>, String> {
    let mut ids: Vec<i64> = Vec::new();
    if let Some(cut) = inbox_cut {
        let mut stmt = conn
            .prepare("SELECT id FROM emails WHERE folder = 'inbox' AND created_at < ?1")
            .map_err(e)?;
        let rows = stmt
            .query_map([cut], |r| r.get::<_, i64>(0))
            .map_err(e)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(e)?;
        ids.extend(rows);
    }
    if let Some(cut) = sent_cut {
        let mut stmt = conn
            .prepare(
                "SELECT id FROM emails
                 WHERE folder = 'sent' AND sent_at IS NOT NULL AND sent_at < ?1",
            )
            .map_err(e)?;
        let rows = stmt
            .query_map([cut], |r| r.get::<_, i64>(0))
            .map_err(e)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(e)?;
        ids.extend(rows);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// 邮件的独立附件相对路径（删行前收集；文件删除在提交后）
fn file_paths_of(conn: &rusqlite::Connection, email_id: i64) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT file_path FROM attachments WHERE email_id = ?1 AND storage = 'file'")
        .map_err(e)?;
    let rows = stmt
        .query_map([email_id], |r| r.get::<_, String>(0))
        .map_err(e)?
        .filter_map(|x| x.ok())
        .filter(|p| !p.is_empty())
        .collect();
    Ok(rows)
}

/// 引用集合（storage='file' 且未 detached；detached 行的残留文件由孤儿回收清）
fn referenced_paths(conn: &rusqlite::Connection) -> Result<HashSet<String>, String> {
    let mut stmt = conn
        .prepare("SELECT file_path FROM attachments WHERE storage = 'file' AND detached = 0")
        .map_err(e)?;
    let set = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(e)?
        .filter_map(|x| x.ok())
        .filter(|p| !p.is_empty())
        .collect();
    Ok(set)
}

/// 孤儿文件（未引用 + mtime 超宽限期）绝对路径列表（只收集不删除，estimate/run 共用）
fn orphan_files(root: &Path, referenced: &HashSet<String>, now: i64) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let Ok(months) = std::fs::read_dir(root) else {
        return out;
    };
    let grace = std::time::Duration::from_secs(ORPHAN_GRACE_SECS);
    let stale_before = std::time::UNIX_EPOCH + std::time::Duration::from_secs(now as u64) - grace;
    for month in months.flatten() {
        if !month.path().is_dir() {
            continue;
        }
        let Ok(email_dirs) = std::fs::read_dir(month.path()) else {
            continue;
        };
        for edir in email_dirs.flatten() {
            if !edir.path().is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(edir.path()) else {
                continue;
            };
            for entry in files.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Ok(rel) = path.strip_prefix(root) else {
                    continue;
                };
                if referenced.contains(rel.to_string_lossy().as_ref()) {
                    continue;
                }
                // mtime 宽限：刚落盘的文件（可能是收件竞态窗口）本轮跳过
                let stale = std::fs::metadata(&path)
                    .and_then(|md| md.modified())
                    .map(|t| t < stale_before)
                    .unwrap_or(false);
                if !stale {
                    continue;
                }
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                out.push((path, size));
            }
        }
    }
    out
}

/// 预估将释放空间（只读；存储页"预计可释放"）
pub fn estimate(state: &AppState) -> Estimate {
    let p = policy(state);
    let now = now_epoch();
    let root = state.data_dir.join("attachments");
    let mut est = Estimate::default();
    let res = state.with_conn(|conn| {
        let targets = detach_targets(conn, p.clean_sent_bulk_days.map(|d| cutoff(now, d)))?;
        est.detach_files = targets.len() as u64;
        est.bytes += targets.iter().map(|t| t.2).filter(|s| *s > 0).sum::<i64>() as u64;

        let ids = expire_targets(
            conn,
            (
                p.keep_inbox_days.map(|d| cutoff(now, d)),
                p.keep_sent_days.map(|d| cutoff(now, d)),
            ),
        )?;
        est.expire_emails = ids.len() as u64;
        for id in ids {
            for rel in file_paths_of(conn, id)? {
                let sz = std::fs::metadata(root.join(&rel))
                    .map(|m| m.len())
                    .unwrap_or(0);
                est.bytes += sz;
            }
        }
        if let Ok(refs) = referenced_paths(conn) {
            for (_, size) in orphan_files(&root, &refs, now) {
                est.orphan_files += 1;
                est.bytes += size;
            }
        }
        Ok::<(), String>(())
    });
    if res.is_err() {
        // 预估失败不影响页面（显示 0；手动清理时仍会报错）
        return Estimate::default();
    }
    est
}

/// 执行清理（手动/定时入口；错误仅在 DB 异常时出现，文件系统错误不中断）
pub fn run(state: &AppState) -> Result<CleanReport, String> {
    let p = policy(state);
    let now = now_epoch();
    let root = state.data_dir.join("attachments");
    let mut report = CleanReport::default();
    // 步骤 1/2 的 DB 变更单事务提交；待删文件先记录，提交后统一执行
    let mut pending_files: Vec<String> = Vec::new();
    state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;

        // 步骤 1：大附件文件清理（置 detached；元数据保留）
        for (attach_id, rel, _) in
            detach_targets(&tx, p.clean_sent_bulk_days.map(|d| cutoff(now, d)))?
        {
            tx.execute(
                "UPDATE attachments SET detached = 1 WHERE id = ?1",
                [attach_id],
            )
            .map_err(e)?;
            report.detached_attachments += 1;
            if !rel.is_empty() {
                pending_files.push(rel);
            }
        }

        // 步骤 2：过期邮件硬删（草稿不在此列，永不自动删）
        for id in expire_targets(
            &tx,
            (
                p.keep_inbox_days.map(|d| cutoff(now, d)),
                p.keep_sent_days.map(|d| cutoff(now, d)),
            ),
        )? {
            let files = file_paths_of(&tx, id)?;
            let n = tx
                .execute("DELETE FROM emails WHERE id = ?1", [id])
                .map_err(e)?;
            if n == 1 {
                report.removed_emails += 1;
                pending_files.extend(files);
            }
        }

        tx.commit().map_err(e)?;
        Ok::<(), String>(())
    })?;

    // 提交后删文件（失败残留由孤儿回收兜底）
    for rel in &pending_files {
        if let Ok(md) = std::fs::metadata(root.join(rel))
            && std::fs::remove_file(root.join(rel)).is_ok()
        {
            report.freed_bytes += md.len();
        }
    }

    // 步骤 3：孤儿回收（引用集合读取失败即中止本步，宁可保留不可误删）
    let refs = state.with_conn(|conn| referenced_paths(conn))?;
    for (path, size) in orphan_files(&root, &refs, now) {
        if std::fs::remove_file(&path).is_ok() {
            report.orphan_files += 1;
            report.freed_bytes += size;
        }
    }
    crate::mail::prune_empty_dirs(&root);
    Ok(report)
}

// ---------- 单元测试 ----------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use std::path::PathBuf;

    fn temp_state(tag: &str) -> (AppState, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("mailbox-m5-clean-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (AppState::open(&dir), dir)
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 直接插一封邮件（绕过业务层；测试造过期数据）
    fn insert_email(
        state: &AppState,
        folder: &str,
        direction: &str,
        status: &str,
        time: i64,
    ) -> i64 {
        state
            .with_conn(|conn| {
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
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * ORPHAN_GRACE_SECS);
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
}
