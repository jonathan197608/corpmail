//! conncheck 模块单测（R8 验收第 4 条 / C11）：并发上限与超时槽位语义

use super::*;

#[test]
fn test_slot_caps_concurrent_checks() {
    let active = AtomicUsize::new(0);
    assert!(try_acquire(&active, 3));
    assert!(try_acquire(&active, 3));
    assert!(try_acquire(&active, 3));
    assert_eq!(active.load(Ordering::SeqCst), 3);
    assert!(
        !try_acquire(&active, 3),
        "第 4 次必须立即失败，不许再 spawn 一个不可回收线程"
    );
    // 槽位释放后可再占（RAII guard 的 Drop 路径）
    let active = Arc::new(active);
    let slot = TestSlot(Arc::clone(&active));
    drop(slot);
    assert_eq!(active.load(Ordering::SeqCst), 2);
    assert!(try_acquire(&active, 3), "释放后名额可复用");
}

#[test]
fn with_timeout_rejects_when_limit_reached() {
    let active = Arc::new(AtomicUsize::new(0));
    for _ in 0..3 {
        assert!(try_acquire(&active, 3));
    }
    let out = with_timeout_limited(
        || TestOutcome::ok(0),
        Arc::clone(&active),
        3,
        Duration::from_millis(50),
    );
    assert!(!out.ok, "超上限不得启动测试");
    assert!(out.detail.contains("并发上限"), "要给用户明确原因");
    assert_eq!(active.load(Ordering::SeqCst), 3, "被拒的请求不该改变计数");
}

#[test]
fn timeout_does_not_release_the_leaked_threads_slot() {
    // 超时后线程仍挂在黑洞连接上：槽位必须继续被它占着，否则无限累积
    let active = Arc::new(AtomicUsize::new(0));
    let out = with_timeout_limited(
        || {
            std::thread::sleep(Duration::from_millis(300));
            TestOutcome::ok(0)
        },
        Arc::clone(&active),
        3,
        Duration::from_millis(30),
    );
    assert!(!out.ok);
    assert!(out.detail.contains("连接超时"));
    assert_eq!(
        active.load(Ordering::SeqCst),
        1,
        "超时返回不释放槽位；泄漏线程总数因此被钉死在上限"
    );
    // 后台线程自主终了后应释放（避免测试进程残留 global 状态）
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

/// A7：后台线程 panic 不得伪装成「连接超时 15 秒」。
/// 旧实现 unwrap_or_else 把 Disconnected 与 Timeout 混报，elapsed_ms 恒 15000
#[test]
fn panicked_test_thread_reports_abort_not_timeout() {
    let active = Arc::new(AtomicUsize::new(0));
    let out = with_timeout_limited(
        || panic!("boom"),
        Arc::clone(&active),
        3,
        Duration::from_millis(100),
    );
    assert!(!out.ok, "panic 必须判失败");
    assert!(
        out.detail.contains("异常终止"),
        "报异常终止而非超时：{}",
        out.detail
    );
    assert!(
        !out.detail.contains("连接超时") && !out.detail.contains("15"),
        "不得伪装网络超时或恒定 15 秒：{}",
        out.detail
    );
    assert!(
        out.elapsed_ms < 100,
        "elapsed_ms 应为实测值（panic 即刻返回），而非恒 15000：{}",
        out.elapsed_ms
    );
    // 槽位随线程终了释放，不因 panic 泄漏
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(active.load(Ordering::SeqCst), 0, "panic 后槽位应释放");
}

/// 正常超时分支不受 A7 改动影响：文案与计口径保持（实测 ≈100ms，不再恒 15000）
#[test]
fn normal_timeout_keeps_original_wording() {
    let active = Arc::new(AtomicUsize::new(0));
    let out = with_timeout_limited(
        || {
            std::thread::sleep(Duration::from_millis(300));
            TestOutcome::ok(0)
        },
        Arc::clone(&active),
        3,
        Duration::from_millis(100),
    );
    assert!(!out.ok);
    assert!(out.detail.contains("连接超时"), "{}", out.detail);
    assert!(
        out.elapsed_ms >= 100 && out.elapsed_ms < 300,
        "{}",
        out.elapsed_ms
    );
    std::thread::sleep(Duration::from_millis(250));
}
