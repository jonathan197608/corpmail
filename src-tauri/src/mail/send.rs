//! SMTP 发送子模块（M3，设计 §2 发送队列 / §7 幂等 / §9 compose/send）
//!
//! 状态机：`queued →（CAS 抢占）sending → sent / failed`；
//! sent 终态迁 folder='sent' 并记 sent_at。失败原因写 error 列。
//! 发送参数快照由 worker 每封从 mailbox.toml 镜像生成（不读库）。

use super::{AppState, e, exec_one, now_epoch};
use crate::state::{defaults, keys};

/// 出站消息（lettre Message 的平台无关形态；注入测试可检视内容）
pub struct OutgoingMessage {
    pub from_addr: String,
    pub from_name: String,
    pub to_addrs: Vec<String>,
    pub cc_addrs: Vec<String>,
    pub subject: String,
    pub body_text: String,
}

/// SMTP 发送注入点（生产 lettre SMTPS；单测注入假实现，避免网络依赖——与 ConnTester 同模式）
pub trait MailSender {
    fn send(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        msg: &OutgoingMessage,
    ) -> Result<(), String>;
}

/// 生产实现：与 conncheck::smtp_test 同 transport 配置（relay + port + credentials，465 隐式 TLS）
pub struct RealSender;

impl MailSender for RealSender {
    #[allow(clippy::too_many_lines)]
    fn send(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        msg: &OutgoingMessage,
    ) -> Result<(), String> {
        use lettre::Transport;
        use lettre::message::Mailbox;
        use lettre::transport::smtp::authentication::Credentials;
        let build = |raw: &str, name: &str| -> Result<Mailbox, String> {
            let addr = raw
                .trim()
                .parse()
                .map_err(|err| format!("邮箱地址无效 {raw}：{err}"))?;
            let name = name.trim();
            Ok(if name.is_empty() {
                Mailbox::new(None, addr)
            } else {
                Mailbox::new(Some(name.to_string()), addr)
            })
        };
        let mut builder = lettre::Message::builder().from(build(&msg.from_addr, &msg.from_name)?);
        for t in &msg.to_addrs {
            builder = builder.to(build(t, "")?);
        }
        for c in &msg.cc_addrs {
            builder = builder.cc(build(c, "")?);
        }
        let message = builder
            .subject(msg.subject.clone())
            .body(msg.body_text.clone())
            .map_err(|err| format!("构建邮件失败：{err}"))?;
        let transport = lettre::SmtpTransport::relay(host)
            .map_err(|err| format!("服务器地址无效：{err}"))?
            .port(port)
            .credentials(Credentials::new(account.to_string(), password.to_string()))
            .build();
        transport
            .send(&message)
            .map_err(|err| format!("发送失败：{err}"))
            .map(|_| ())
    }
}

/// 收件人/抄送串拆分（逗号或分号分隔，去空白）
fn split_addrs(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// 草稿 → 排队（原子 CAS：仅 status='draft' 可入队；重复提交 0 行返回 false，幂等 §7）
pub fn enqueue_draft(state: &AppState, id: i64) -> bool {
    state.with_conn(|conn| {
        exec_one(
            conn,
            "UPDATE emails SET status = 'queued', error = NULL WHERE id = ?1 AND status = 'draft'",
            id,
        )
    })
}

/// failed → draft 回编辑（§9 /emails/{id}/resend 重发入口；仅 failed 可转）
pub fn reopen_failed(state: &AppState, id: i64) -> bool {
    state.with_conn(|conn| {
        exec_one(
            conn,
            "UPDATE emails SET status = 'draft', folder = 'draft', error = NULL
             WHERE id = ?1 AND status = 'failed'",
            id,
        )
    })
}

/// 单封发送核心（发送 worker 与单测共用；基于独立连接，不持 AppState）。
///
/// 状态机（§3）：`queued →（CAS 抢占）sending → sent / failed`；
/// sent 终态同步迁 `folder='sent'` 并记 sent_at。失败原因写 error 列。
/// 返回 Ok(终态)（"sent"/"failed"）；未抢占到排队状态或邮件缺失返回 Err。
/// 发送参数快照（worker 每封派发前从配置生成；配置源 mailbox.toml，
/// 不再读库——验收拍板：DB 不保存 mailbox.toml 已有配置）
#[derive(Clone, Debug)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
}

impl SmtpConfig {
    /// 从配置快照生成（含默认值回退；端口非法回退 465）
    pub fn from_config(cfg: &crate::conf::ConfigMap) -> Self {
        let d = defaults();
        let get = |k: &str| -> String {
            cfg.get(k)
                .cloned()
                .or_else(|| {
                    d.iter()
                        .find(|(dk, _)| *dk == k)
                        .map(|(_, v)| (*v).to_string())
                })
                .unwrap_or_default()
        };
        Self {
            host: get(keys::SMTP_HOST),
            port: get(keys::SMTP_PORT).parse().unwrap_or(465),
        }
    }
}

pub fn dispatch_one(
    conn: &rusqlite::Connection,
    sender: &dyn MailSender,
    id: i64,
    smtp: &SmtpConfig,
) -> Result<&'static str, String> {
    // CAS 抢占：重复派发同一封（双击/崩溃重放）时 0 行，天然幂等
    let n = conn
        .execute(
            "UPDATE emails SET status = 'sending' WHERE id = ?1 AND status = 'queued'",
            [id],
        )
        .map_err(e)?;
    if n != 1 {
        return Err("邮件不在排队状态（可能已在发送）".into());
    }
    let mark = |status: &str, err: Option<&str>| -> Result<(), String> {
        let now = now_epoch();
        let err_v = err.unwrap_or("");
        conn.execute(
            "UPDATE emails SET status = ?2, error = ?3,
                    sent_at = CASE WHEN ?2 = 'sent' THEN ?4 ELSE sent_at END,
                    folder  = CASE WHEN ?2 = 'sent' THEN 'sent' ELSE folder END
             WHERE id = ?1",
            rusqlite::params![id, status, err_v, now],
        )
        .map(|_| ())
        .map_err(e)
    };
    let row = conn.query_row(
        "SELECT from_addr, COALESCE(from_name,''), to_addrs, COALESCE(cc_addrs,''),
                subject, body_text
         FROM emails WHERE id = ?1",
        [id],
        |r| {
            Ok(OutgoingMessage {
                from_addr: r.get(0)?,
                from_name: r.get(1)?,
                to_addrs: split_addrs(&r.get::<_, String>(2)?),
                cc_addrs: split_addrs(&r.get::<_, String>(3)?),
                subject: r.get(4)?,
                body_text: r.get(5)?,
            })
        },
    );
    // 读取失败（行被删/库异常）也必须把已抢占的 sending 落定：
    // worker 侧吞错，若状态滞留 sending 则永不重试（启动补发只查 queued）；
    // 落定失败本身也不再向上传错（此处已无重试机会），仅记日志
    let fail_or_log = |ctx: &str| {
        if let Err(err) = mark("failed", Some(ctx)) {
            eprintln!("[send] 邮件 {id} 落定 failed 失败：{err}");
        }
    };
    let msg = match row {
        Ok(m) => m,
        Err(err) => {
            fail_or_log(&format!("读取待发邮件失败：{err}"));
            return Ok("failed");
        }
    };
    if msg.to_addrs.is_empty() {
        fail_or_log("收件人为空");
        return Ok("failed");
    }
    // 发信参数：调用方快照（mailbox.toml）；密码仍走 conn（凭据与配置分离）
    let password = match crate::secrets::retrieve(conn, &msg.from_addr) {
        crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => p,
        crate::secrets::GetResult::Missing => {
            fail_or_log("凭据缺失，请在设置中重新保存密码");
            return Ok("failed");
        }
    };
    match sender.send(&smtp.host, smtp.port, &msg.from_addr, &password, &msg) {
        Ok(()) => {
            if let Err(err) = mark("sent", None) {
                eprintln!("[send] 邮件 {id} 落定 sent 失败：{err}");
            }
            Ok("sent")
        }
        Err(err) => {
            fail_or_log(&err);
            Ok("failed")
        }
    }
}

#[cfg(test)]
#[path = "../tests/mail_send.rs"]
mod tests;
