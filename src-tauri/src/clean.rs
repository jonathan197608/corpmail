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

use crate::err as e;
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
    /// 部分成功警告（步骤 3 读引用集合失败时非空：前两步已提交，
    /// 不能整体报"清理失败"误导用户，见 run 的降级逻辑）
    pub warning: String,
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
    crate::db::collect_rows(
        conn,
        "SELECT a.id, COALESCE(a.file_path, ''), a.size_gz
         FROM attachments a JOIN emails e ON e.id = a.email_id
         WHERE a.storage = 'file' AND a.detached = 0
           AND e.direction = 'out' AND e.status = 'sent'
           AND e.sent_at IS NOT NULL AND e.sent_at < ?1",
        &[&cutoff],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .map_err(e)
}

/// 步骤 2 目标：过期邮件 id（inbox 按 created_at、sent 按 sent_at）
fn expire_targets(
    conn: &rusqlite::Connection,
    (inbox_cut, sent_cut): (Option<i64>, Option<i64>),
) -> Result<Vec<i64>, String> {
    let mut ids: Vec<i64> = Vec::new();
    if let Some(cut) = inbox_cut {
        ids.extend(
            crate::db::collect_rows(
                conn,
                "SELECT id FROM emails WHERE folder = 'inbox' AND created_at < ?1",
                &[&cut],
                |r| r.get::<_, i64>(0),
            )
            .map_err(e)?,
        );
    }
    if let Some(cut) = sent_cut {
        ids.extend(
            crate::db::collect_rows(
                conn,
                "SELECT id FROM emails
                 WHERE folder = 'sent' AND sent_at IS NOT NULL AND sent_at < ?1",
                &[&cut],
                |r| r.get::<_, i64>(0),
            )
            .map_err(e)?,
        );
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// 邮件的独立附件相对路径（删行前收集；文件删除在提交后）
fn file_paths_of(conn: &rusqlite::Connection, email_id: i64) -> Result<Vec<String>, String> {
    Ok(crate::db::collect_rows(
        conn,
        "SELECT file_path FROM attachments WHERE email_id = ?1 AND storage = 'file'",
        &[&email_id],
        |r| r.get::<_, String>(0),
    )
    .map_err(e)?
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect())
}

/// 引用集合（storage='file' 且未 detached；detached 行的残留文件由孤儿回收清）
fn referenced_paths(conn: &rusqlite::Connection) -> Result<HashSet<String>, String> {
    Ok(crate::db::collect_rows(
        conn,
        "SELECT file_path FROM attachments WHERE storage = 'file' AND detached = 0",
        &[],
        |r| r.get::<_, String>(0),
    )
    .map_err(e)?
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect())
}

/// 孤儿文件（未引用 + mtime 超宽限期）绝对路径列表（只收集不删除，estimate/run 共用）
fn orphan_files(root: &Path, referenced: &HashSet<String>, now: i64) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let Ok(months) = std::fs::read_dir(root) else {
        return out;
    };
    let grace = std::time::Duration::from_secs(ORPHAN_GRACE_SECS);
    // 时钟早于 1970+1h（极端测试/系统时钟错拨）时判不出孤儿：全部跳过，防 Duration 下溢 panic
    let stale_before = std::time::Duration::from_secs(now.max(0) as u64)
        .checked_sub(grace)
        .map(|d| std::time::UNIX_EPOCH + d);
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
                // mtime 宽限：刚落盘的文件（可能是收件竞态窗口）本轮跳过；
                // stale_before 为 None（系统时钟异常）时按不超期处理
                let stale = stale_before.is_some_and(|cut| {
                    std::fs::metadata(&path)
                        .and_then(|md| md.modified())
                        .map(|t| t < cut)
                        .unwrap_or(false)
                });
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
    let root = state.data_dir().join("attachments");
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
    let root = state.data_dir().join("attachments");
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

    // 步骤 3：孤儿回收（引用集合读取失败即跳过本步，宁可保留不可误删）。
    // 步骤 1/2 此时已提交，绝不能把失败向上抛——那会让 UI 报"清理失败"，
    // 而实际已删掉邮件与附件，属误导；降级为警告，下轮清理自动重试（审计 P2-8）
    match state.with_conn(|conn| referenced_paths(conn)) {
        Ok(refs) => {
            for (path, size) in orphan_files(&root, &refs, now) {
                if std::fs::remove_file(&path).is_ok() {
                    report.orphan_files += 1;
                    report.freed_bytes += size;
                }
            }
            crate::mail::prune_empty_dirs(&root);
        }
        Err(err) => {
            eprintln!("[clean] 孤儿回收跳过（引用集合读取失败）：{err}");
            report.warning =
                format!("孤儿文件回收已跳过（读取附件引用失败：{err}），下次清理会自动重试");
        }
    }
    Ok(report)
}

// ---------- 单元测试 ----------

#[cfg(test)]
#[path = "tests/clean.rs"]
mod tests;
