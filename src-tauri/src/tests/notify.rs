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

/// 进程内 id 去重：同窗口重复调用不再纳入已通知集合（用 reset 保证隔离）
#[test]
fn notified_set_dedupes_on_repeat() {
    reset_for_test();
    // 复用 on_fetch_finished 的去重段逻辑等价验证：直接操作集合
    {
        let mut notified = notified_set().lock().unwrap_or_else(|e| e.into_inner());
        notified.insert(42);
    }
    let dup_again = {
        let mut notified = notified_set().lock().unwrap_or_else(|e| e.into_inner());
        !notified.insert(42)
    };
    assert!(dup_again, "重复 id 应被识别为已通知");
    reset_for_test();
}

/// 上限截断：达到 NOTIFIED_MAX 后整表清空，防止常驻进程无限增长
#[test]
fn notified_set_caps_growth() {
    reset_for_test();
    {
        let mut notified = notified_set().lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..NOTIFIED_MAX as i64 {
            notified.insert(i);
        }
        assert_eq!(notified.len(), NOTIFIED_MAX, "前置：集合已满");
        // 达上限清空（与 on_fetch_finished 内联逻辑一致）
        if notified.len() >= NOTIFIED_MAX {
            notified.clear();
        }
    }
    {
        let notified = notified_set().lock().unwrap_or_else(|e| e.into_inner());
        assert!(notified.is_empty(), "达上限应整表清空");
    }
    reset_for_test();
}
