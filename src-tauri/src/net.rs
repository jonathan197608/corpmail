//! 网络层公共件：传输加密模式选择 + 有界 TCP 探活
//!
//! 加密模式（审计批 1）：旧实现把 SMTP 固定成隐式 TLS（465）、IMAP 固定成隐式
//! TLS（993），端口填 587/143 时只得到一句"连接失败"，无从判断是地址错、端口错
//! 还是加密方式错。此处按端口给出默认模式（465/993 → 隐式 TLS，其余 → STARTTLS），
//! 并允许配置显式覆盖（auto/implicit/starttls/plain）；失败文案附带端口与改法。
//!
//! 探活（审计 P1-5）：`imap 3.0.0-alpha.15` 内部直接用 `TcpStream::connect`
//! （无 connect_timeout），且 `Session.conn` 私有、`SetReadTimeout` 仅对 IDLE 生效
//! ——应用层无法给常规 Session 设超时。内网设备丢包不回 RST 时，收件/测试连接
//! 线程会永久卡在握手或 fetch 上。这里提供**有界**连接探活：至少把「根本连不上」
//! 的场景在 10 秒内结束，而不是无限挂起。
//!
//! 局限：DNS 解析（`to_socket_addrs`）本身无超时接口，极端 DNS 故障时仍可能
//! 阻塞数十秒（由系统解析器上限决定）。探活只覆盖 TCP 层可达性，不代表协议可用。

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// 探活超时（连接每地址的上界）
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// 传输加密模式（SMTP/IMAP 共用语义）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMode {
    /// 连上即 TLS 握手（SMTPS 465 / IMAPS 993）
    Implicit,
    /// 先明文，再按服务器能力升级 TLS（587 / 143）
    StartTls,
    /// 全程明文（仅限可信内网中继；凭据与邮件不加密）
    Plain,
}

impl TlsMode {
    /// 配置串解析：auto/空/未知一律 None（交给端口推断），保证老配置零迁移
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "implicit" | "ssl" | "tls" => Some(Self::Implicit),
            "starttls" => Some(Self::StartTls),
            "plain" | "none" | "insecure" => Some(Self::Plain),
            _ => None,
        }
    }

    /// 端口 + 覆盖值解析。默认规则（SMTP/IMAP 同构）：
    /// 465（SMTPS）/ 993（IMAPS）→ 隐式 TLS，其余端口 → STARTTLS（可协商失败即报错，
    /// 不静默回落明文——Opportunistic 会掩盖中间人降级）
    pub fn resolve(port: u16, override_raw: &str) -> Self {
        Self::parse(override_raw).unwrap_or(if matches!(port, 465 | 993) {
            Self::Implicit
        } else {
            Self::StartTls
        })
    }

    /// 连接失败时的补充提示：说清"这个端口按什么方式连的"以及"该改成什么"，
    /// 让 587/143 填错模式的用户能自己纠正，而不是只看到"连接失败"
    pub fn hint(self, port: u16) -> String {
        match self {
            Self::Implicit => format!(
                "（本次按「隐式 TLS」连接端口 {port}；若服务器要求先明文再 STARTTLS，\
                 请把安全方式改为「STARTTLS」）"
            ),
            Self::StartTls => format!(
                "（本次按「STARTTLS」连接端口 {port}；若该端口本身即加密端口（如 465/993），\
                 请把安全方式改为「隐式 TLS」）"
            ),
            Self::Plain => format!("（本次按「明文」连接端口 {port}，凭据与邮件均未加密）"),
        }
    }

    /// imap crate 的连接模式
    pub fn imap_mode(self) -> imap::ConnectionMode {
        match self {
            Self::Implicit => imap::ConnectionMode::Tls,
            Self::StartTls => imap::ConnectionMode::StartTls,
            Self::Plain => imap::ConnectionMode::Plaintext,
        }
    }

    /// 构造 lettre SMTP 传输（含凭据）。三种模式共用同一出口，
    /// 避免测试连接与真实发信各写一套 TLS 配置而漂移
    pub fn smtp_transport(
        self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
    ) -> Result<lettre::SmtpTransport, String> {
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::transport::smtp::client::{Tls, TlsParameters};
        // 单独函数而非闭包：闭包对不同枚举构造子会固定首个推断类型
        fn params(host: &str) -> Result<TlsParameters, String> {
            TlsParameters::new(host.to_string()).map_err(|e| format!("TLS 参数初始化失败：{e}"))
        }
        let builder = match self {
            Self::Implicit => lettre::SmtpTransport::builder_dangerous(host)
                .port(port)
                .tls(Tls::Wrapper(params(host)?)),
            Self::StartTls => lettre::SmtpTransport::builder_dangerous(host)
                .port(port)
                .tls(Tls::Required(params(host)?)),
            Self::Plain => lettre::SmtpTransport::builder_dangerous(host)
                .port(port)
                .tls(Tls::None),
        };
        Ok(builder
            .credentials(Credentials::new(account.to_string(), password.to_string()))
            .build())
    }
}

/// 尝试与 (host, port) 建立一次带超时的 TCP 连接；任一地址成功即视为可达。
///
/// 仅在"发现不可达"时返回 Err，错误文案面向用户（含具体地址与原因）。
pub fn probe_tcp(host: &str, port: u16) -> Result<(), String> {
    let addrs: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("无法解析服务器地址 {host}：{e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("无法解析服务器地址 {host}"));
    }
    let mut last = format!("无法连接 {host}:{port}");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
            Ok(_) => return Ok(()),
            Err(e) => last = format!("无法连接 {addr}：{e}"),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    /// 未监听端口应快速失败（本机拒绝连接，不做网络依赖）
    #[test]
    fn probe_reports_closed_port() {
        assert!(super::probe_tcp("127.0.0.1", 1).is_err());
    }

    /// 端口 → 模式推断：465/993 隐式，其余 STARTTLS（587/143/2525 都是 STARTTLS 场景）
    #[test]
    fn resolve_prefers_implicit_on_known_ports() {
        use super::TlsMode;
        assert_eq!(TlsMode::resolve(465, "auto"), TlsMode::Implicit);
        assert_eq!(TlsMode::resolve(993, "auto"), TlsMode::Implicit);
        assert_eq!(TlsMode::resolve(587, ""), TlsMode::StartTls);
        assert_eq!(TlsMode::resolve(143, "auto"), TlsMode::StartTls);
        assert_eq!(TlsMode::resolve(25, "auto"), TlsMode::StartTls);
    }

    /// 显式覆盖优先于端口推断（含大小写/空白/未知值）
    #[test]
    fn resolve_honours_override() {
        use super::TlsMode;
        assert_eq!(TlsMode::resolve(465, "starttls"), TlsMode::StartTls);
        assert_eq!(TlsMode::resolve(587, "implicit"), TlsMode::Implicit);
        assert_eq!(TlsMode::resolve(993, " Plain "), TlsMode::Plain);
        assert_eq!(TlsMode::resolve(465, "bogus"), TlsMode::Implicit, "未知值回落端口推断");
        assert_eq!(TlsMode::parse("SSL"), Some(TlsMode::Implicit));
        assert_eq!(TlsMode::parse("auto"), None);
    }

    /// 失败提示必须含端口与建议（验收项：不再只报"连接失败"）
    #[test]
    fn hint_mentions_port_and_alternative() {
        use super::TlsMode;
        let h = TlsMode::StartTls.hint(587);
        assert!(h.contains("587"), "{h}");
        assert!(h.contains("隐式 TLS"), "{h}");
        let h2 = TlsMode::Implicit.hint(465);
        assert!(h2.contains("465") && h2.contains("STARTTLS"), "{h2}");
    }
}
