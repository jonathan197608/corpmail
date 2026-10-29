//! 测试连接（设计 §8.1）：SMTP `test_connection` + IMAP `SELECT INBOX` 双通道
//!
//! 用途：首启向导"测试并保存"与设置·账号 tab"测试连接/测试收信"。
//! 两库均为纯同步 API（imap 3.0.0-alpha.15 实测为同步实现，无 async 依赖），
//! 与 `ui://` 协议回调的同步语义直接相配。
//! 防抖：前端按钮请求期禁用（§7），此处天然串行。

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

/// 带限期执行网络测试（后台线程 + recv_timeout）
fn with_timeout(f: impl FnOnce() -> TestOutcome + Send + 'static) -> TestOutcome {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(TEST_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(_) => TestOutcome::fail(
            TEST_TIMEOUT.as_millis() as u64,
            "连接超时（15 秒无响应，请检查网络或服务器地址）",
        ),
    }
}

/// SMTP 通道测试：连接 + TLS + EHLO（lettre `test_connection`，隐式 TLS 465）
pub fn smtp_test(host: &str, port: u16, account: &str, password: &str) -> TestOutcome {
    let (host, account, password) = (host.to_string(), account.to_string(), password.to_string());
    with_timeout(move || {
        let t = Instant::now();
        let builder = match lettre::SmtpTransport::relay(&host) {
            Ok(b) => b,
            Err(e) => {
                return TestOutcome::fail(
                    t.elapsed().as_millis() as u64,
                    format!("服务器地址无效：{e}"),
                );
            }
        };
        let transport = builder
            .port(port)
            .credentials(lettre::transport::smtp::authentication::Credentials::new(
                account, password,
            ))
            .build();
        // test_connection：TCP+TLS+EHLO（不做 AUTH——凭据正确性在发信时最终校验）
        match transport.test_connection() {
            Ok(true) => TestOutcome::ok(t.elapsed().as_millis() as u64),
            Ok(false) => TestOutcome::fail(t.elapsed().as_millis() as u64, "服务器无响应"),
            Err(e) => TestOutcome::fail(t.elapsed().as_millis() as u64, format!("连接失败：{e}")),
        }
    })
}

/// IMAP 通道测试：隐式 TLS(993) + LOGIN + SELECT INBOX（设计 §8.1）
pub fn imap_test(host: &str, port: u16, account: &str, password: &str) -> TestOutcome {
    let (host, account, password) = (host.to_string(), account.to_string(), password.to_string());
    with_timeout(move || {
        let t = Instant::now();
        let ms = || t.elapsed().as_millis() as u64;

        let client = match imap::ClientBuilder::new(&host, port)
            .mode(imap::ConnectionMode::Tls)
            .connect()
        {
            Ok(c) => c,
            Err(e) => return TestOutcome::fail(ms(), format!("连接失败：{e}")),
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
