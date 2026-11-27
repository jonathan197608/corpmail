//! notify 模块单元测试（摘要组稿逻辑；系统通知本身需 GUI 环境不测）

use super::*;

fn mails(items: &[(i64, &str)]) -> Vec<(i64, String)> {
    items.iter().map(|(i, s)| (*i, s.to_string())).collect()
}

#[test]
fn summary_single_mail_directly_outputs_subject() {
    let p = mails(&[(1, "配案通知")]);
    assert_eq!(build_summary(&p), "配案通知");
}

#[test]
fn summary_empty_subject_uses_placeholder() {
    let p = mails(&[(7, "  ")]);
    assert_eq!(build_summary(&p), "（无主题 · #7）");
}

#[test]
fn summary_multiple_mails_join_with_semicolon() {
    let p = mails(&[(1, "A"), (2, "B")]);
    assert_eq!(build_summary(&p), "A；B");
}

#[test]
fn summary_over_five_mails_appends_count() {
    let p = mails(&[
        (1, "一"),
        (2, "二"),
        (3, "三"),
        (4, "四"),
        (5, "五"),
        (6, "六"),
    ]);
    assert_eq!(build_summary(&p), "一；二；三；四；五 等 6 封");
}

#[test]
fn summary_truncates_to_max_chars() {
    let long = "长".repeat(200);
    let p = mails(&[(1, long.as_str())]);
    let out = build_summary(&p);
    assert!(
        out.chars().count() <= MAX_BODY_CHARS + 1,
        "应截断：{}",
        out.chars().count()
    );
    assert!(out.ends_with('…'));
}

/// 进程内 id 去重：走生产同一份 `select_pending`/`commit_notified`，
/// 不再复刻镜像逻辑（第七轮 P3-6）。R14：筛选不消费，提交才消费。
#[test]
fn notified_set_dedupes_on_repeat() {
    let mut notified = std::collections::HashSet::new();
    let first = select_pending(&notified, vec![(42, "主题".into())]);
    assert_eq!(first.len(), 1, "首次通知须纳入");
    assert!(notified.is_empty(), "筛选阶段不得消费 id（失败要能重试）");
    commit_notified(&mut notified, &first);
    assert!(notified.contains(&42), "成功后才提交");
    let again = select_pending(&notified, vec![(42, "主题".into())]);
    assert!(again.is_empty(), "重复 id 应被识别为已通知");
    // 不触碰全局集合，天然无并行串味
}

/// R14：通知失败不提交 → 下一轮仍能筛到同一批（旧实现永久吞掉）
#[test]
fn notified_set_keeps_ids_when_notification_fails() {
    let mut notified = std::collections::HashSet::new();
    let batch = vec![(7, "主题".into()), (8, "另一个".into())];
    let pending = select_pending(&notified, batch.clone());
    assert_eq!(pending.len(), 2);
    // 模拟 .show() 失败：不调用 commit_notified
    let retry = select_pending(&notified, batch);
    assert_eq!(retry.len(), 2, "失败后重试须仍拿到同一批 id");
    commit_notified(&mut notified, &pending);
    assert!(select_pending(&notified, vec![(7, "x".into())]).is_empty());
}

/// 上限截断：达到 NOTIFIED_MAX 后整表清空，防止常驻进程无限增长
#[test]
fn notified_set_caps_growth() {
    let mut notified: std::collections::HashSet<i64> = (0..NOTIFIED_MAX as i64).collect();
    assert_eq!(notified.len(), NOTIFIED_MAX, "前置：集合已满");
    // 生产函数：满表时先清空，再纳入本批候选
    let pending = select_pending(
        &notified,
        vec![(99_999, "新件".into()), (0, "旧 id".into())],
    );
    assert_eq!(pending.len(), 1, "满表中的旧 id 应被筛除");
    commit_notified(&mut notified, &pending);
    assert_eq!(notified.len(), 1, "集合须已清空只剩本批 id");
    assert!(notified.contains(&99_999));
}

/// 安全：主题中的控制字符（CR/LF）在通知正文里折叠为空格，不打乱排版
#[test]
fn summary_collapses_control_chars_in_subject() {
    let p = mails(&[(1, "正常\r\n主题")]);
    assert_eq!(build_summary(&p), "正常 主题");
    let p = mails(&[(1, "a\n\nb"), (2, "c\r\rd")]);
    let out = build_summary(&p);
    assert!(
        !out.contains('\r') && !out.contains('\n'),
        "通知正文不得含控制字符：{out:?}"
    );
}

#[test]
fn p52_notify_hook_resolves_db_path_inside_callback() {
    // 复查五 P2-2：首启向导会在同一会话内直切数据目录（不重启）。若通知钩子
    // 在注册时捕获 db_path，切库后仍指向旧锚点库 → 幽灵空库 + 永不通知。
    // 护栏要求回调闭包内动态 try_state::<AppState>() 取当前 data_dir。
    const LIB: &str = include_str!("../lib.rs");
    let hook_at = LIB
        .find("state.set_notify_hook")
        .expect("通知钩子注册点存在");
    let body = &LIB[hook_at..];
    let body = &body[..body.find("state.set_tray_hook").unwrap_or(body.len())];
    assert!(
        body.contains("try_state::<state::AppState>()"),
        "通知钩子必须回调内动态取 AppState（首设直切数据目录场景）"
    );
    assert!(
        body.contains("st.data_dir().join(\"corpmail.db\")"),
        "通知查询的库路径必须来自回调时的当前 data_dir，不得注册时捕获"
    );
    assert!(
        !body.contains("let db_path = state.data_dir()"),
        "注册点仍存在静态 db_path 捕获"
    );
}

/// R14：生产路径必须「通知成功才提交去重」——`.show()` 失败时提交语句
/// 不得执行。纯函数签名已是只读，这条护栏钉死 on_fetch_finished 的编排。
#[test]
fn r14_notify_commits_only_after_show_success() {
    // 运行时读取：include_str! 会编译期缓存，变异验证恢复后可能读到陈旧
    // 内容导致假红（实测 include_str 在变异恢复后仍返回变异版源码）
    let src_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/notify.rs");
    let src = std::fs::read_to_string(src_path).expect("读取 notify.rs");
    let at = src.find("pub fn on_fetch_finished").expect("入口存在");
    let body = &src[at..];
    let commit = body.find("commit_notified(").expect("成功提交点存在");
    let show = body.find(".show()").expect("通知调用存在");
    assert!(show < commit, "提交必须在 .show() 之后：{body}");
    let between = &body[show..commit];
    assert!(
        between.contains("is_ok()") && between.contains("if shown"),
        "必须按 .show() 结果分支，失败不提交：{between}"
    );

    // 仅「commit 在 .show() 之后」+「between 含 is_ok()/if shown」不足以挡住
    // 把 commit 挪出成功块（留空壳 if shown 仍绿）。按花括号计数定位
    // `if shown {` 的配对闭合括号，钉死 commit 真正落在成功分支块内。
    let branch = body.find("if shown {").expect("成功分支存在");
    let open = branch + "if shown".len(); // 指向 `{` 前的空格，让计数从 0 开始
    let mut depth = 0i32;
    let mut close = None;
    for (off, ch) in body[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + off);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close.expect("if shown 成功分支块须闭合");
    assert!(
        commit > open && commit < close,
        "commit_notified 必须位于 if shown 成功块内：{body}"
    );

    assert!(
        !body[..commit].contains("select_pending(&mut"),
        "筛选阶段不得消费去重集合"
    );
}
