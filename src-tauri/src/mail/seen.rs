//! 已读回写批处理（审计批 3）
//!
//! 旧实现每点开一封未读邮件就 spawn 一个线程：完整 IMAP 连接 + LOGIN + SELECT +
//! STORE 各一次。连点 20 封 = 20 次登录，企业服务器普遍按"每分钟登录次数"
//! 限流，触发后表现为长时间收不到新邮件。
//!
//! 现方案：
//! - 本地置已读时同事务写 `seen_pending = 1`（业务状态与"待回写"意向一次落盘，
//!   崩溃后标志仍在，重启由 worker 补发）
//! - worker 单线程：1.5s 合并窗口把连点合成一批，单次连接 + 单条 STORE 覆盖多个 UID
//! - uidvalidity 不匹配 → 清标志放弃（UID 空间已重排，旧 UID 可能指向别的邮件，
//!   绝不写入）；网络等其他失败 → 保留标志，指数退避重试，5 次后放弃并记日志

use super::{MailFetcher, seen_params_of};
use crate::mail::recv::ERR_UIDVALIDITY_CHANGED;
use crate::state::AppState;
use std::time::Duration;

/// 合并窗口：最后一次点击后等这么久再发（连点 N 封 = 1 次登录）
pub const MERGE_WINDOW: Duration = Duration::from_millis(1500);

/// 单轮最多回写的 UID 数（再多的部分留给下一轮，避免单条命令过长）
const MAX_UIDS_PER_ROUND: usize = 500;

/// 失败重试上限（连续失败达到即清标志放弃，避免永久重试）
pub const MAX_ATTEMPTS: u32 = 5;

/// 第 attempt 次失败后的退避时长（60s 起翻倍，上限 10 分钟；attempt 从 1 计）。
/// 旧实现 `60 << n` 首次失败即 120s，与「60s 起」的文档语义不符
pub fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    let secs = 60u64.saturating_mul(1u64 << shift);
    Duration::from_secs(secs.min(600))
}

/// 待回写行（id 用于清标志，uidvalidity+uid 用于 STORE）
struct Pending {
    id: i64,
    uidvalidity: u32,
    uid: u32,
}

/// 读待回写行（按 id 升序；单轮上限 MAX_UIDS_PER_ROUND）
fn pending_rows(state: &AppState) -> Result<Vec<Pending>, String> {
    let rows: Vec<(i64, String)> = state.with_conn(|conn| -> Result<Vec<_>, String> {
        let mut stmt = conn
            .prepare(
                "SELECT id, remote_uid FROM emails
                 WHERE seen_pending = 1 AND direction = 'in' AND remote_uid IS NOT NULL
                 ORDER BY id LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let out = stmt
            .query_map([MAX_UIDS_PER_ROUND as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();
        Ok(out)
    })?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, remote)| {
            let (v, u) = remote.split_once(':')?;
            Some(Pending {
                id,
                uidvalidity: v.parse().ok()?,
                uid: u.parse().ok()?,
            })
        })
        .collect())
}

/// 清待回写标志（成功/放弃时调用；本地 unread 保持已读不受影响）
fn clear_pending(state: &AppState, ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    state.with_conn(|conn| {
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[seen] 清标志事务开启失败：{e}");
                return;
            }
        };
        for id in ids {
            if let Err(e) = tx.execute("UPDATE emails SET seen_pending = 0 WHERE id = ?1", [id]) {
                eprintln!("[seen] 邮件 {id} 清标志失败：{e}");
                return;
            }
        }
        if let Err(e) = tx.commit() {
            eprintln!("[seen] 清标志提交失败：{e}");
        }
    });
}

/// 放弃全部待回写（重试上限后用；下次点击已读会重新置标志，不影响本地语义）
pub fn abandon(state: &AppState) {
    let n = state.with_conn(|conn| {
        conn.execute("UPDATE emails SET seen_pending = 0 WHERE seen_pending = 1", [])
            .unwrap_or(0)
    });
    if n > 0 {
        eprintln!("[seen] 回写连续失败达上限，放弃 {n} 封的服务器已读同步（本地已读不受影响）");
    }
}

/// 执行一轮回写：返回本轮成功同步的封数。
///
/// 按 uidvalidity 分组（同一账号下 UIDVALIDITY 变化意味着 UID 空间重置，
/// 不同组的 UID 不能混在一条 STORE 里）；每组一次连接。
pub fn flush(state: &AppState, fetcher: &dyn MailFetcher) -> Result<usize, String> {
    let rows = pending_rows(state)?;
    if rows.is_empty() {
        return Ok(0);
    }
    // 账号/服务器参数取任一待回写行即可（同一设置源）；取不到说明凭据缺失：
    // 清标志放弃（本地已读已生效，用户重存密码后再次点击会重新置标志）
    let Some(p) = seen_params_of(state, rows[0].id) else {
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        eprintln!("[seen] 凭据缺失，跳过 {} 封的服务器已读回写", ids.len());
        clear_pending(state, &ids);
        return Ok(0);
    };
    // 分组保序：BTreeMap 让日志与请求顺序稳定（便于排查）
    let mut groups: std::collections::BTreeMap<u32, Vec<&Pending>> =
        std::collections::BTreeMap::new();
    for r in &rows {
        groups.entry(r.uidvalidity).or_default().push(r);
    }
    let mut done = 0usize;
    for (uidvalidity, group) in groups {
        let uids: Vec<u32> = group.iter().map(|r| r.uid).collect();
        let ids: Vec<i64> = group.iter().map(|r| r.id).collect();
        match fetcher.mark_seen_batch(
            &p.host,
            p.port,
            p.security,
            &p.account,
            &p.password,
            uidvalidity,
            &uids,
        ) {
            Ok(()) => {
                clear_pending(state, &ids);
                done += ids.len();
            }
            Err(err) if err.starts_with(ERR_UIDVALIDITY_CHANGED) => {
                // UID 已重排：旧 UID 可能指向别的邮件，写入会误标他人邮件为已读
                eprintln!("[seen] {err}（uidvalidity={uidvalidity}）");
                clear_pending(state, &ids);
            }
            Err(err) => return Err(err), // 保留标志，调用方退避重试
        }
    }
    // 单轮上限之外的积压：再自唤醒一次，避免剩余标志一直等到用户下次点击
    if done > 0 && pending_rows(state).map(|r| !r.is_empty()).unwrap_or(false) {
        state.notify_seen();
    }
    Ok(done)
}

#[cfg(test)]
#[path = "../tests/mail_seen.rs"]
mod tests;
