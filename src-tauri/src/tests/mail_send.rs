//! mail_send 模块单元测试（自 src/mail/send.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::mail::{DraftInput, add_attachments, save_draft};
use crate::state::AppState;
use std::path::PathBuf;

fn temp_state(tag: &str) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("corpmail-send-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (AppState::open(&dir), dir)
}

fn cleanup(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// 排队态邮件（发件人固定单测账号，可带收件人覆盖）
fn queued_email(state: &AppState, to: &str) -> i64 {
    let input = DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: to.into(),
        cc_addrs: String::new(),
        subject: "月度激励汇总".into(),
        body_text: "请查收汇总表。".into(),
        in_reply_to: None,
    };
    let id = save_draft(state, &input).unwrap();
    assert!(enqueue_draft(state, id));
    id
}

fn draft(subject: &str, body: &str) -> DraftInput {
    DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "单测用户".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        subject: subject.into(),
        body_text: body.into(),
        in_reply_to: None,
    }
}

struct FakeSender {
    fail: Option<&'static str>,
    seen: std::sync::Mutex<Vec<(String, u16, String, OutgoingMessage)>>,
}

impl FakeSender {
    fn ok() -> Self {
        Self {
            fail: None,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn failing(reason: &'static str) -> Self {
        Self {
            fail: Some(reason),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl MailSender for FakeSender {
    fn send(
        &self,
        host: &str,
        port: u16,
        account: &str,
        _password: &str,
        msg: OutgoingMessage,
    ) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap()
            .push((host.into(), port, account.into(), msg));
        match self.fail {
            Some(r) => Err(r.into()),
            None => Ok(()),
        }
    }
}

#[test]
fn split_addrs_parses_comma_and_semicolon() {
    assert!(split_addrs("").is_empty());
    assert_eq!(
        split_addrs(" a@x.cn , b@x.cn ;; c@x.cn "),
        vec![
            "a@x.cn".to_string(),
            "b@x.cn".to_string(),
            "c@x.cn".to_string()
        ]
    );
}

#[test]
fn dispatch_success_transitions_to_sent() {
    let (state, dir) = temp_state("m3-ok");
    state
        .set_batch(&[(keys::SMTP_HOST, "smtp.test.cn")])
        .unwrap();
    state
        .store_password("demo@chinatelecom.cn", "pw-m3")
        .unwrap();
    let id = queued_email(&state, "peer@x.cn, ops@x.cn");
    let sender = FakeSender::ok();
    // 发送参数快照：从 corpmail.toml 内存镜像生成（配置不落库）
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    assert_eq!(smtp.host, "smtp.test.cn");
    let result = state.with_conn(|c| dispatch_one(c, &sender, id, &smtp, &dir.join("attachments")));
    assert_eq!(result.unwrap(), "sent");
    let (status, folder, sent_at, error): (String, String, Option<i64>, Option<String>) = state
        .with_conn(|c| {
            c.query_row(
                "SELECT status, folder, sent_at, error FROM emails WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        });
    assert_eq!((status.as_str(), folder.as_str()), ("sent", "sent"));
    assert!(sent_at.is_some(), "sent_at 应写入");
    assert!(error.unwrap_or_default().is_empty());
    // 发送参数与内容检视
    let seen = sender.seen.lock().unwrap();
    let (host, port, account, msg) = &seen[0];
    assert_eq!(host, "smtp.test.cn");
    assert_eq!(*port, 465, "端口默认 465");
    assert_eq!(account, "demo@chinatelecom.cn");
    assert_eq!(msg.subject, "月度激励汇总");
    assert_eq!(
        msg.to_addrs,
        vec!["peer@x.cn".to_string(), "ops@x.cn".to_string()]
    );
    assert_eq!(msg.body_text, "请查收汇总表。");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn dispatch_failure_marks_failed_with_reason() {
    let (state, dir) = temp_state("m3-fail");
    state
        .store_password("demo@chinatelecom.cn", "pw-m3")
        .unwrap();
    let id = queued_email(&state, "peer@x.cn");
    let sender = FakeSender::failing("连接超时");
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    let result = state.with_conn(|c| dispatch_one(c, &sender, id, &smtp, &dir.join("attachments")));
    assert_eq!(result.unwrap(), "failed");
    let (status, error): (String, String) = state.with_conn(|c| {
        c.query_row(
            "SELECT status, COALESCE(error,'') FROM emails WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(status, "failed");
    assert!(error.contains("连接超时"), "失败原因应入库：{error}");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn dispatch_rejects_non_queued_and_blank_recipients() {
    let (state, dir) = temp_state("m3-guard");
    state
        .store_password("demo@chinatelecom.cn", "pw-m3")
        .unwrap();
    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    // draft 态直接派发：CAS 抢占失败
    let id = save_draft(&state, &draft("未排队", "体")).unwrap();
    assert!(
        state
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp, &dir.join("attachments")))
            .is_err()
    );
    // 排队但收件人空白：转 failed（凭据存在仍先校验收件人）
    let blank_id = queued_email(&state, "   ");
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(c, &sender, blank_id, &smtp, &dir.join("attachments")))
            .unwrap(),
        "failed"
    );
    let (status, error): (String, String) = state.with_conn(|c| {
        c.query_row(
            "SELECT status, COALESCE(error,'') FROM emails WHERE id=?1",
            [blank_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(status, "failed");
    assert_eq!(error, "收件人为空");
    // 重复派发同一封 queued：第二次抢占失败（幂等）
    let id2 = queued_email(&state, "peer@x.cn");
    let sender2 = FakeSender::ok();
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(c, &sender2, id2, &smtp, &dir.join("attachments")))
            .unwrap(),
        "sent"
    );
    assert!(
        state
            .with_conn(|c| dispatch_one(c, &sender2, id2, &smtp, &dir.join("attachments")))
            .is_err(),
        "已发送邮件不可再派发"
    );
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn dispatch_reports_missing_credential() {
    let (state, dir) = temp_state("m3-nopw");
    let id = queued_email(&state, "ghost@x.cn");
    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp, &dir.join("attachments")))
            .unwrap(),
        "failed"
    );
    let (status, error): (String, String) = state.with_conn(|c| {
        c.query_row(
            "SELECT status, COALESCE(error,'') FROM emails WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(status, "failed");
    assert!(error.contains("凭据缺失"), "缺失凭据应有明确原因：{error}");
    cleanup(&dir);
}

#[test]
fn reopen_failed_returns_to_draft() {
    let (state, dir) = temp_state("m3-reopen");
    let id = queued_email(&state, "peer@x.cn");
    state.with_conn(|c| {
        c.execute(
            "UPDATE emails SET status='failed', folder='sent', error='服务器拒收' WHERE id=?1",
            [id],
        )
        .unwrap();
    });
    assert!(reopen_failed(&state, id), "failed 应可回草稿");
    let (status, folder, error): (String, String, Option<String>) = state.with_conn(|c| {
        c.query_row(
            "SELECT status, folder, error FROM emails WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    });
    assert_eq!((status.as_str(), folder.as_str()), ("draft", "draft"));
    assert!(error.is_none(), "重开应清失败原因");
    assert!(!reopen_failed(&state, id), "非 failed 不应可重开");
    cleanup(&dir);
}

#[test]
fn enqueue_is_cas_idempotent() {
    let (state, dir) = temp_state("m3-cas");
    let id = save_draft(&state, &draft("入队", "体")).unwrap();
    assert!(enqueue_draft(&state, id), "首次入队成功");
    assert!(!enqueue_draft(&state, id), "重复入队应被拒（幂等）");
    let status: String = state.with_conn(|c| {
        c.query_row("SELECT status FROM emails WHERE id=?1", [id], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(status, "queued");
    cleanup(&dir);
}

/// 附件必须随邮件真正发出（审计 P0-2：旧实现只发正文，界面仍显示"已发送"）
#[test]
fn attachments_travel_with_message() {
    let (state, dir) = temp_state("m3-att");
    state
        .store_password("demo@chinatelecom.cn", "pw-m3")
        .unwrap();
    let id = queued_email(&state, "peer@x.cn");
    let src = dir.join("report.txt");
    std::fs::write(&src, b"hello-attachment").unwrap();
    add_attachments(&state, id, &[src.to_string_lossy().into_owned()]).unwrap();

    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    let root = dir.join("attachments");
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp, &root))
            .unwrap(),
        "sent"
    );
    let seen = sender.seen.lock().unwrap();
    let msg = &seen[0].3;
    assert_eq!(msg.attachments.len(), 1, "附件必须进入出站消息");
    assert_eq!(msg.attachments[0].filename, "report.txt");
    assert_eq!(msg.attachments[0].bytes, b"hello-attachment");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 附件文件已被清理（detached）→ 明确失败，绝不"少发一个附件"静默成功
#[test]
fn detached_attachment_blocks_send() {
    let (state, dir) = temp_state("m3-att-detached");
    state
        .store_password("demo@chinatelecom.cn", "pw-m3")
        .unwrap();
    let id = queued_email(&state, "peer@x.cn");
    let src = dir.join("plan.pdf");
    std::fs::write(&src, b"%PDF-1.4").unwrap();
    add_attachments(&state, id, &[src.to_string_lossy().into_owned()]).unwrap();
    state.with_conn(|c| {
        c.execute("UPDATE attachments SET detached = 1 WHERE email_id = ?1", [id])
            .unwrap();
    });

    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    let root = dir.join("attachments");
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp, &root))
            .unwrap(),
        "failed"
    );
    let error: String = state.with_conn(|c| {
        c.query_row(
            "SELECT COALESCE(error,'') FROM emails WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert!(error.contains("已按清理策略删除"), "应指出附件不可用：{error}");
    assert!(sender.seen.lock().unwrap().is_empty(), "阻断时不得调用 MTA");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 附件总大小超过上限 → 提前失败（避免在 SMTP 传输阶段被服务器拒绝且文案不可读）
#[test]
fn oversized_attachments_block_send() {
    let (state, dir) = temp_state("m3-att-big");
    state
        .store_password("demo@chinatelecom.cn", "pw-m3")
        .unwrap();
    let id = queued_email(&state, "peer@x.cn");
    // 全零内容 gzip 后入库体积很小，解压后才计入上限
    let src = dir.join("big.bin");
    std::fs::write(&src, vec![0u8; 51 * 1024 * 1024]).unwrap();
    add_attachments(&state, id, &[src.to_string_lossy().into_owned()]).unwrap();
    let _ = std::fs::remove_file(&src);

    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    let root = dir.join("attachments");
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp, &root))
            .unwrap(),
        "failed"
    );
    let error: String = state.with_conn(|c| {
        c.query_row(
            "SELECT COALESCE(error,'') FROM emails WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert!(error.contains("超过 50 MB 上限"), "应给出上限提示：{error}");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 进程在发送中被杀 → 启动自愈把 sending 复位为 failed，用户可重发
#[test]
fn interrupted_sending_recovers_to_failed() {
    let (state, dir) = temp_state("m3-recover");
    let id = queued_email(&state, "peer@x.cn");
    state.with_conn(|c| {
        c.execute("UPDATE emails SET status='sending' WHERE id=?1", [id])
            .unwrap();
    });
    assert_eq!(state.with_conn(|c| recover_interrupted_sends(c)), 1);
    let (status, error): (String, String) = state.with_conn(|c| {
        c.query_row(
            "SELECT status, COALESCE(error,'') FROM emails WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(status, "failed");
    assert!(error.contains("上次发送被中断"), "应写明中断原因：{error}");
    assert!(reopen_failed(&state, id), "自愈后必须能重发");
    cleanup(&dir);
}
