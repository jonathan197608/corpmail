//! 测试连接（设计 §8.1）：SMTP `test_connection` + IMAP `SELECT INBOX` 双通道
//!
//! 用途：首启向导"测试并保存"与设置·账号 tab"测试连接/测试收信"。
//! 两库均为纯同步 API（imap 3.0.0-alpha.15 实测为同步实现，无 async 依赖），
//! 与 `ui://` 协议回调的同步语义直接相配。
//! 防抖：前端按钮请求期禁用（§7），此处天然串行。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// 单通道测试结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct TestOutcome {
    pub ok: bool,
    pub elapsed_ms: u64,
    /// 失败原因（供页面展示；成功时为空）
    pub detail: String,
}

impl TestOutcome {
    fn ok(elapsed_ms: u64) -> Self {
        Self {
            ok: true,
            elapsed_ms,
            detail: String::new(),
        }
    }

    fn fail(elapsed_ms: u64, detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            elapsed_ms,
            detail: detail.into(),
        }
    }
}

/// 连接测试网络超时（秒）：imap 3.0.0-alpha 无内置网络超时（TLS 握手被内网设备
/// 黑洞时会永久阻塞，且本函数运行在 ui:// 协议回调线程——挂起即向导 UI 无响应）。
/// 搬到后台线程 + 带限期收结果；超时后回调立即返回，后台线程自主终了。
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 全局并发上限（R8 验收第 4 条）：超时线程无法 kill，也拿不到带读超时的
/// TcpStream 句柄，它会永久挂在 select() 上并持一个连接。旧实现每重试一次
/// 泄漏一个不可回收线程、无上界；这里用计数把「泄漏线程总数」钉死在 3。
const MAX_CONCURRENT_TESTS: usize = 3;

/// 槽位凭证：由**后台线程**持有，线程终了才释放。超时返回不释放，
/// 这样挂在黑洞连接上的线程会一直占着名额，第 4 次起直接失败。
struct TestSlot(Arc<AtomicUsize>);

impl Drop for TestSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn try_acquire(active: &AtomicUsize, max: usize) -> bool {
    active
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < max).then_some(n + 1)
        })
        .is_ok()
}

/// 带限期执行网络测试（后台线程 + recv_timeout）。
/// 参数化计数/上限/超时，便于在单测里用局部计数器与毫秒级超时做真实并发验证。
fn with_timeout_limited(
    f: impl FnOnce() -> TestOutcome + Send + 'static,
    active: Arc<AtomicUsize>,
    max: usize,
    timeout: Duration,
) -> TestOutcome {
    if !try_acquire(&active, max) {
        return TestOutcome::fail(
            0,
            format!("测试连接过于频繁（并发上限 {max}），请稍候再试"),
        );
    }
    let slot = TestSlot(active);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = f();
        let _ = tx.send(outcome);
        // 显式释放：槽位生命周期与后台线程一致，而非与调用方一致
        drop(slot);
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        TestOutcome::fail(
            timeout.as_millis() as u64,
            format!(
                "连接超时（{} 秒无响应，请检查网络或服务器地址）",
                timeout.as_secs()
            ),
        )
    })
}

fn with_timeout(f: impl FnOnce() -> TestOutcome + Send + 'static) -> TestOutcome {
    // 全局 static 无法直接装进 Arc，用 OnceLock 只初始化一次
    static GLOBAL: std::sync::OnceLock<Arc<AtomicUsize>> = std::sync::OnceLock::new();
    let active = GLOBAL
        .get_or_init(|| Arc::new(AtomicUsize::new(0)))
        .clone();
    with_timeout_limited(f, active, MAX_CONCURRENT_TESTS, TEST_TIMEOUT)
}

/// SMTP 通道测试：连接 + TLS（按模式）+ EHLO（lettre `test_connection`）
pub fn smtp_test(
    host: &str,
    port: u16,
    mode: crate::net::TlsMode,
    account: &str,
    password: &str,
) -> TestOutcome {
    let (host, account, password) = (host.to_string(), account.to_string(), password.to_string());
    with_timeout(move || {
        let t = Instant::now();
        let ms = || t.elapsed().as_millis() as u64;
        // 先做有界 TCP 探活：imap/lettre 自身无连接超时，探活把"不可达"从
        // 15 秒整体超时降为 10 秒内的明确原因（并避免后台线程长期挂起）
        if let Err(e) = crate::net::probe_tcp(&host, port) {
            return TestOutcome::fail(ms(), e);
        }
        let transport = match mode.smtp_transport(&host, port, &account, &password) {
            Ok(t) => t,
            Err(e) => return TestOutcome::fail(ms(), e),
        };
        // test_connection：TCP+TLS+EHLO（不做 AUTH——凭据正确性在发信时最终校验）
        match transport.test_connection() {
            Ok(true) => TestOutcome::ok(ms()),
            Ok(false) => TestOutcome::fail(ms(), "服务器无响应"),
            // 失败文案必须带端口与改法：587/143 填错模式的用户才能自己纠正
            Err(e) => TestOutcome::fail(ms(), format!("连接失败：{e}{}", mode.hint(port))),
        }
    })
}

/// IMAP 通道测试：按模式连接 + LOGIN + SELECT INBOX（设计 §8.1）
pub fn imap_test(
    host: &str,
    port: u16,
    mode: crate::net::TlsMode,
    account: &str,
    password: &str,
) -> TestOutcome {
    let (host, account, password) = (host.to_string(), account.to_string(), password.to_string());
    with_timeout(move || {
        let t = Instant::now();
        let ms = || t.elapsed().as_millis() as u64;
        // 同 smtp_test：先用有界探活给出明确原因（imap 无连接/读超时接口）
        if let Err(e) = crate::net::probe_tcp(&host, port) {
            return TestOutcome::fail(ms(), e);
        }
        let client = match imap::ClientBuilder::new(&host, port)
            .mode(mode.imap_mode())
            .connect()
        {
            Ok(c) => c,
            Err(e) => {
                return TestOutcome::fail(ms(), format!("连接失败：{e}{}", mode.hint(port)));
            }
        };
        let mut sess = match client.login(&account, &password) {
            Ok(s) => s,
            Err((e, _)) => return TestOutcome::fail(ms(), format!("登录失败：{e}")),
        };
        // SELECT INBOX 验证邮箱可访问（与首启后收件同路径）
        let outcome = match sess.select("INBOX") {
            Ok(_) => TestOutcome::ok(ms()),
            Err(e) => TestOutcome::fail(ms(), format!("无法打开收件箱：{e}")),
        };
        let _ = sess.logout();
        outcome
    })
}

#[cfg(test)]
#[path = "tests/conncheck.rs"]
mod tests;
