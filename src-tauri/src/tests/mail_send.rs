//! mail_send 模块单元测试（自 src/mail/send.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::mail::{DraftInput, save_draft};
use crate::state::AppState;
use std::path::PathBuf;

fn temp_state(tag: &str) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("mailbox-send-{tag}-{}", std::process::id()));
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
        msg: &OutgoingMessage,
    ) -> Result<(), String> {
        self.seen.lock().unwrap().push((
            host.into(),
            port,
            account.into(),
            OutgoingMessage {
                from_addr: msg.from_addr.clone(),
                from_name: msg.from_name.clone(),
                to_addrs: msg.to_addrs.clone(),
                cc_addrs: msg.cc_addrs.clone(),
                subject: msg.subject.clone(),
                body_text: msg.body_text.clone(),
            },
        ));
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
    // 发送参数快照：从 mailbox.toml 内存镜像生成（配置不落库）
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    assert_eq!(smtp.host, "smtp.test.cn");
    let result = state.with_conn(|c| dispatch_one(c, &sender, id, &smtp));
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
    let result = state.with_conn(|c| dispatch_one(c, &sender, id, &smtp));
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
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp))
            .is_err()
    );
    // 排队但收件人空白：转 failed（凭据存在仍先校验收件人）
    let blank_id = queued_email(&state, "   ");
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(c, &sender, blank_id, &smtp))
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
            .with_conn(|c| dispatch_one(c, &sender2, id2, &smtp))
            .unwrap(),
        "sent"
    );
    assert!(
        state
            .with_conn(|c| dispatch_one(c, &sender2, id2, &smtp))
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
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp))
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
