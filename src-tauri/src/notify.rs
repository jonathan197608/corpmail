//! 收件完成通知（系统通知中心）
//!
//! 一轮收件结束后，把本轮新增的未读邮件汇总为一条摘要通知：
//! - 只通知本轮新入库且仍未读的邮件（已读的不打扰）
//! - 同一封邮件只通知一次：按“本轮窗口内入库”划定范围，且进程内记录
//!   已通知 id 防同轮/多轮重复；重启后首轮增量拉取不会拉到旧邮件
//! - 通知失败静默忽略（增值体验，不阻断收件主流程）

use std::path::Path;
/// 单条通知最多列出的标题数（超出合并为“等 N 封”）。
/// 截断长度按“字符数”而非 UTF-8 字节数——通知中心按显示宽度截断，
/// 字节截断会切碎多字节字符成乱码。
const MAX_TITLES: usize = 5;

/// 通知正文最长字符数（超长截断 + 省略号）
const MAX_BODY_CHARS: usize = 120;

/// 已通知过的邮件 id（进程内去重；重启清零属预期——首轮增量拉不到旧件）。
/// 上限截断：常驻进程的极端长会话下（如长期不关机 + 海量收件）无界增长，
/// 达上限整表清空——代价是极端清空后同轮可能重复通知一次，属可接受的
/// 稀有退化（通知本就是尽力而为的增值体验）
const NOTIFIED_MAX: usize = 50_000;
type NotifiedSet = std::sync::Mutex<std::collections::HashSet<i64>>;

/// HashSet::new 非 const fn（RandomState），沿用 OnceLock 惰性初始化
fn notified_set() -> &'static NotifiedSet {
    static SET: std::sync::OnceLock<NotifiedSet> = std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// 进程内去重**筛选**：只剔除已通知过的 id，不消费（R14）。
/// 旧实现在这里就 `insert`，而系统通知 `.show()` 可能失败（首次未授权、
/// 通知中心不可用）；失败后本批 id 已被记为「已通知」，下一轮窗口前移后
/// 这几封邮件在进程生命周期内再无通知机会——瞬时失败被放大成永久丢失。
/// 提交推到通知成功后的 `commit_notified`。
fn select_pending(
    notified: &std::collections::HashSet<i64>,
    candidates: Vec<(i64, String)>,
) -> Vec<(i64, String)> {
    candidates
        .into_iter()
        .filter(|(id, _)| !notified.contains(id))
        .collect()
}

/// 通知成功后提交去重：达上限整表清空（第七轮 P3-6），再纳入本批 id。
/// 与 `select_pending` 拆开，保证「失败可重试、成功不重复」。
fn commit_notified(notified: &mut std::collections::HashSet<i64>, ids: &[(i64, String)]) {
    if notified.len() >= NOTIFIED_MAX {
        notified.clear();
    }
    for (id, _) in ids {
        notified.insert(*id);
    }
}

/// 一轮收件完成后调用：发一条新邮件摘要系统通知。
///
/// `db_path`：主库路径（自建独立连接查询，WAL 下与收件/主连接不互阻）；
/// `since` = 本轮开始时刻（epoch 秒），窗口内新入库且未读的邮件即候选。
/// 查询/通知任何失败均静默忽略（增值体验，不阻断收件主流程）。
pub fn on_fetch_finished<R: tauri::Runtime>(app: &tauri::AppHandle<R>, db_path: &Path, since: i64) {
    let Ok(conn) = crate::db::open(db_path) else {
        eprintln!("[notify] 打开数据库失败（通知跳过）");
        return;
    };
    // 窗口内未读新件（inbox 方向；发件箱同步是自己发的信，不通知）。
    // 窗口上界取当前时刻（审计 L4：原 since+3600 在单轮收件超 1 小时后
    // 会漏掉窗口外入库的邮件，且 id 去重不补漏）；重叠部分由 id 去重兜住
    let candidates: Vec<(i64, String)> = conn
        .prepare(
            "SELECT id, COALESCE(subject, '') FROM emails
              WHERE direction = 'in' AND folder = 'inbox' AND unread = 1
                AND created_at >= ?1 AND created_at <= ?2
              ORDER BY id",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([since, crate::mail::now_epoch()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            Ok(rows.filter_map(Result::ok).collect::<Vec<_>>())
        })
        .unwrap_or_else(|err| {
            eprintln!("[notify] 查询新邮件失败（不影响收件）：{err}");
            Vec::new()
        });
    if candidates.is_empty() {
        return;
    }
    // 进程内去重筛选：已通知过的 id 剔除（同轮窗口重叠/多轮重复拉取都拦在此处）
    // R14：此处只读不消费，通知成功后再 commit（失败保留重试机会）
    let pending: Vec<(i64, String)> = {
        let notified = notified_set().lock().unwrap_or_else(|e| e.into_inner());
        select_pending(&notified, candidates)
    };
    if pending.is_empty() {
        return;
    }
    let count = pending.len();
    let body = build_summary(&pending);
    let title = if count == 1 {
        "新邮件".to_string()
    } else {
        format!("收到 {count} 封新邮件")
    };
    use tauri_plugin_notification::NotificationExt;
    let shown = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .is_ok();
    if shown {
        let mut notified = notified_set().lock().unwrap_or_else(|e| e.into_inner());
        commit_notified(&mut notified, &pending);
    } else {
        eprintln!("[notify] 系统通知发送失败（不影响收件，本批保留重试机会）");
    }
}

/// 摘要正文：单封直出主题；多封前 5 条主题合并，超出收“等 N 封”
fn build_summary(pending: &[(i64, String)]) -> String {
    let count = pending.len();
    let empty = |s: &str| s.trim().is_empty();
    let pick = |(id, s): &(i64, String)| {
        if empty(s) {
            format!("（无主题 · #{id}）")
        } else {
            // 主题来自收件头解析，可能含控制字符（CR/LF）——通知正文折叠为
            // 单空格，避免系统通知里出现换行/不可见控制符打乱排版
            crate::mail::collapse_control(s)
        }
    };
    let mut body = if count == 1 {
        pick(&pending[0])
    } else if count <= MAX_TITLES {
        pending.iter().map(pick).collect::<Vec<_>>().join("；")
    } else {
        format!(
            "{} 等 {count} 封",
            pending
                .iter()
                .take(MAX_TITLES)
                .map(pick)
                .collect::<Vec<_>>()
                .join("；")
        )
    };
    if body.chars().count() > MAX_BODY_CHARS {
        body = format!("{}…", body.chars().take(MAX_BODY_CHARS).collect::<String>());
    }
    body
}

/// 测试辅助：清空已通知集合
#[cfg(test)]
#[allow(dead_code)]
pub fn reset_for_test() {
    notified_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

#[cfg(test)]
#[path = "tests/notify.rs"]
mod tests;
