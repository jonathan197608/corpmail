//! 网络探活
//!
//! 动机（审计 P1-5）：`imap 3.0.0-alpha.15` 内部直接用 `TcpStream::connect`
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
}
