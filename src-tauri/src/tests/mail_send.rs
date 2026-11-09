//! mail_send 模块单元测试（自 src/mail/send.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::mail::{DraftInput, add_attachments, get, save_draft, save_draft_with_html};
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
        bcc_addrs: String::new(),
        subject: "月度激励汇总".into(),
        body_text: "请查收汇总表。".into(),
        in_reply_to: None,
        scheduled_at: 0,
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
        bcc_addrs: String::new(),
        subject: subject.into(),
        body_text: body.into(),
        in_reply_to: None,
        scheduled_at: 0,
    }
}

#[test]
fn save_draft_with_html_stores_sanitized_html_and_plain_text() {
    let (state, dir) = temp_state("html-draft");
    let input = DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        bcc_addrs: String::new(),
        subject: "HTML 正文".into(),
        body_text: "你好".into(),
        in_reply_to: None,
        scheduled_at: 0,
    };
    let id = save_draft_with_html(
        &state,
        &input,
        r#"<p style="color:red">你好<script>alert(1)</script><a href="javascript:alert(1)">x</a></p>"#,
    )
    .unwrap();
    let d = get(&state, id).expect("草稿应可读");
    assert!(d.body_html.contains("color:red"), "内联样式应保留");
    assert!(!d.body_html.contains("<script"), "script 应被净化");
    assert!(!d.body_html.contains("javascript:"), "危险 URL 应被净化");
    assert_eq!(d.body_text, "你好", "纯文本版用于检索/预览");
    let kind: String = state
        .with_conn(|c| {
            c.query_row("SELECT body_kind FROM emails WHERE id=?1", [id], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(kind, "html", "body_kind 应标记为 html");
    cleanup(&dir);
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
        _mode: crate::net::TlsMode,
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

/// BCC 语义护栏（M1-3）：builder.bcc 后 lettre 的默认行为——
/// 信封 RCPT TO 含密送人（密送人能收到），MIME 头不含 Bcc（其他收件人看不到）。
/// 升级 lettre 若此测试变红，说明默认 drop_bcc 语义不再成立，
/// 真实外发将向全部收件人泄露密送名单——必须立即修复而非删断言。
#[test]
fn lettre_bcc_goes_to_envelope_not_mime_headers() {
    let msg = lettre::Message::builder()
        .from("demo@chinatelecom.cn".parse().unwrap())
        .to("peer@x.cn".parse().unwrap())
        .cc("cc@x.cn".parse().unwrap())
        .bcc("hidden@x.cn".parse().unwrap())
        .subject(String::from("密送语义护栏"))
        .body("体".to_string())
        .unwrap();
    assert!(
        msg.headers()
            .get::<lettre::message::header::Bcc>()
            .is_none(),
        "MIME 头不得含 Bcc（外发即泄露名单）"
    );
    let rcpts: Vec<String> = msg.envelope().to().iter().map(|a| a.to_string()).collect();
    for expect in ["peer@x.cn", "cc@x.cn", "hidden@x.cn"] {
        assert!(
            rcpts.iter().any(|r| r.eq_ignore_ascii_case(expect)),
            "信封 RCPT TO 应含 {expect}：{rcpts:?}"
        );
    }
}

/// 出站链路（M1-3）：库内 bcc_addrs → OutgoingMessage.bcc_addrs → 联系人采集；
/// 纯密送（To/Cc 空、仅 Bcc）是合法形态，不被"收件人为空"误拦；存档保留密送名单
#[test]
fn dispatch_carries_bcc_and_pure_bcc_mail_sends() {
    let (state, dir) = temp_state("m1-bcc-dispatch");
    state
        .set_batch(&[(keys::SMTP_HOST, "smtp.test.cn")])
        .unwrap();
    state
        .store_password("demo@chinatelecom.cn", "pw-bcc")
        .unwrap();
    let input = DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: String::new(),
        cc_addrs: String::new(),
        bcc_addrs: "hidden@x.cn, b2@x.cn".into(),
        subject: "纯密送".into(),
        body_text: "只给密送人看。".into(),
        in_reply_to: None,
        scheduled_at: 0,
    };
    let id = save_draft(&state, &input).unwrap();
    assert!(enqueue_draft(&state, id));
    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    let result = state.with_conn(|c| {
        dispatch_one(
            c,
            &sender,
            id,
            &smtp,
            &dir.join("attachments"),
            &state.key_file(),
        )
    });
    assert_eq!(result.unwrap(), "sent", "纯密送应正常发出");
    let seen = sender.seen.lock().unwrap();
    let msg = &seen[0].3;
    assert_eq!(
        msg.bcc_addrs,
        vec!["hidden@x.cn".to_string(), "b2@x.cn".to_string()],
        "OutgoingMessage 应携带密送名单"
    );
    assert!(msg.to_addrs.is_empty() && msg.cc_addrs.is_empty());
    // 存档保留：已发送行的 bcc_addrs 原样（提醒自己密送过谁）
    let d = get(&state, id).unwrap();
    assert_eq!(d.bcc_addrs, "hidden@x.cn, b2@x.cn");
    // 联系人采集：密送人也进联系人库
    let collected: i64 = state
        .with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM contacts WHERE addr = 'hidden@x.cn'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(collected, 1, "密送人应进联系人库");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn parse_mailbox_accepts_display_name_and_plain_addr() {
    // 带显示名（转发/通讯录选择常见）：整串曾当 Address 解析导致 Invalid email user
    let m = parse_mailbox("汤新苑 <tangxy1.gd@chinatelecom.cn>", "").unwrap();
    assert_eq!(m.email.to_string(), "tangxy1.gd@chinatelecom.cn");
    assert_eq!(m.name.as_deref(), Some("汤新苑"));
    // 纯地址 + 外部显示名（发件人场景）
    let f = parse_mailbox("demo@chinatelecom.cn", "张伟").unwrap();
    assert_eq!(f.email.to_string(), "demo@chinatelecom.cn");
    assert_eq!(f.name.as_deref(), Some("张伟"));
    // 纯地址无显示名
    let p = parse_mailbox("peer@x.cn", "").unwrap();
    assert_eq!(p.email.to_string(), "peer@x.cn");
    assert!(p.name.is_none());
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
    let result = state.with_conn(|c| {
        dispatch_one(
            c,
            &sender,
            id,
            &smtp,
            &dir.join("attachments"),
            &state.key_file(),
        )
    });
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
fn dispatch_reads_html_body_for_sending() {
    let (state, dir) = temp_state("m3-html-dispatch");
    state
        .set_batch(&[(keys::SMTP_HOST, "smtp.test.cn")])
        .unwrap();
    state
        .store_password("demo@chinatelecom.cn", "pw-m3")
        .unwrap();
    let input = DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        bcc_addrs: String::new(),
        subject: "HTML 发送".into(),
        body_text: "你好".into(),
        in_reply_to: None,
        scheduled_at: 0,
    };
    let id = save_draft_with_html(&state, &input, "<p>你好</p>").unwrap();
    assert!(enqueue_draft(&state, id));
    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    let result = state.with_conn(|c| {
        dispatch_one(
            c,
            &sender,
            id,
            &smtp,
            &dir.join("attachments"),
            &state.key_file(),
        )
    });
    assert_eq!(result.unwrap(), "sent");
    let seen = sender.seen.lock().unwrap();
    assert_eq!(
        seen[0].3.body_html.as_deref(),
        Some("<p>你好</p>"),
        "发送应携带净化后的 HTML 正文"
    );
    assert_eq!(seen[0].3.body_text, "你好");
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
    let result = state.with_conn(|c| {
        dispatch_one(
            c,
            &sender,
            id,
            &smtp,
            &dir.join("attachments"),
            &state.key_file(),
        )
    });
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
            .with_conn(|c| dispatch_one(
                c,
                &sender,
                id,
                &smtp,
                &dir.join("attachments"),
                &state.key_file()
            ))
            .is_err()
    );
    // 排队但收件人空白：转 failed（凭据存在仍先校验收件人）
    let blank_id = queued_email(&state, "   ");
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(
                c,
                &sender,
                blank_id,
                &smtp,
                &dir.join("attachments"),
                &state.key_file()
            ))
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
            .with_conn(|c| dispatch_one(
                c,
                &sender2,
                id2,
                &smtp,
                &dir.join("attachments"),
                &state.key_file()
            ))
            .unwrap(),
        "sent"
    );
    assert!(
        state
            .with_conn(|c| dispatch_one(
                c,
                &sender2,
                id2,
                &smtp,
                &dir.join("attachments"),
                &state.key_file()
            ))
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
            .with_conn(|c| dispatch_one(
                c,
                &sender,
                id,
                &smtp,
                &dir.join("attachments"),
                &state.key_file()
            ))
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
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp, &root, &state.key_file()))
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
        c.execute(
            "UPDATE attachments SET detached = 1 WHERE email_id = ?1",
            [id],
        )
        .unwrap();
    });

    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    let root = dir.join("attachments");
    assert_eq!(
        state
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp, &root, &state.key_file()))
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
    assert!(
        error.contains("已按清理策略删除"),
        "应指出附件不可用：{error}"
    );
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
            .with_conn(|c| dispatch_one(c, &sender, id, &smtp, &root, &state.key_file()))
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

/// 加密方式解析：端口推断（587 STARTTLS / 465 隐式）+ 配置覆盖 + 缺省回退
#[test]
fn smtp_config_resolves_security_from_port_and_override() {
    use crate::net::TlsMode;
    use crate::state::keys;
    let mut cfg = crate::conf::ConfigMap::new();
    cfg.insert(keys::SMTP_HOST.into(), "smtp.test.cn".into());
    cfg.insert(keys::SMTP_PORT.into(), "587".into());
    let c = SmtpConfig::from_config(&cfg);
    assert_eq!(c.security, TlsMode::StartTls, "587 应走 STARTTLS");

    cfg.insert(keys::SMTP_PORT.into(), "465".into());
    assert_eq!(
        SmtpConfig::from_config(&cfg).security,
        TlsMode::Implicit,
        "465 应走隐式 TLS"
    );

    cfg.insert(keys::SMTP_PORT.into(), "2525".into());
    cfg.insert(keys::SMTP_SECURITY.into(), "implicit".into());
    assert_eq!(
        SmtpConfig::from_config(&cfg).security,
        TlsMode::Implicit,
        "显式覆盖优先于端口推断"
    );

    // 端口缺省 → 465 隐式 TLS（与历史行为一致，老配置零迁移）
    cfg.remove(keys::SMTP_PORT);
    let c = SmtpConfig::from_config(&cfg);
    assert_eq!((c.port, c.security), (465, TlsMode::Implicit));
    assert_eq!(c.host, "smtp.test.cn");
}

/// 定时发送调度（M4-2）：只派发到点草稿（folder/status=draft 且
/// scheduled_at>0 且已到）；入队即清 scheduled_at；未到点不动；
/// 非草稿行（已发送）带 scheduled_at 不受影响
#[test]
fn dispatch_due_scheduled_picks_only_due_drafts() {
    let (state, dir) = temp_state("sched-dispatch");
    let now = now_epoch();
    // 到点草稿（60s 前应发）
    let due = save_draft(
        &state,
        &DraftInput {
            scheduled_at: now - 60,
            ..draft("到点", "内容")
        },
    )
    .unwrap();
    // 未到点草稿（1h 后）
    let future = save_draft(
        &state,
        &DraftInput {
            scheduled_at: now + 3600,
            ..draft("未到", "内容")
        },
    )
    .unwrap();
    // 非草稿：已发送行带 scheduled_at（异常残留，调度不应触碰）
    let sent_row = save_draft(&state, &draft("已发", "内容")).unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "UPDATE emails SET folder='sent', status='sent', scheduled_at=?2 WHERE id=?1",
                rusqlite::params![sent_row, now - 3600],
            )
            .unwrap();
            Ok::<(), ()>(())
        })
        .unwrap();

    let dispatched = dispatch_due_scheduled(&state);

    assert_eq!(dispatched, 1, "只派发到点的那封");
    let st = |id| {
        state.with_conn(|c| {
            c.query_row(
                "SELECT status, scheduled_at FROM emails WHERE id=?1",
                [id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .unwrap()
        })
    };
    assert_eq!(st(due), ("queued".into(), 0), "到点入队并清定时标记");
    assert_eq!(
        st(future),
        ("draft".into(), now + 3600),
        "未到点保持草稿与计划"
    );
    assert_eq!(
        st(sent_row).1,
        now - 3600,
        "非草稿行的残留 scheduled_at 不被清"
    );

    // 幂等：再扫一轮（due 已 queued）不再派发
    assert_eq!(dispatch_due_scheduled(&state), 0);
    cleanup(&dir);
}

/// enqueue 路径清定时（M4-2）：立即发送一个定时草稿 → queued 且 scheduled_at 归零
#[test]
fn enqueue_draft_clears_scheduled_at() {
    let (state, dir) = temp_state("sched-enqueue");
    let now = now_epoch();
    let id = save_draft(
        &state,
        &DraftInput {
            scheduled_at: now + 7200,
            ..draft("定时转立即", "内容")
        },
    )
    .unwrap();
    assert!(enqueue_draft(&state, id));
    let (status, at) = state.with_conn(|c| {
        c.query_row(
            "SELECT status, scheduled_at FROM emails WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap()
    });
    assert_eq!(status, "queued");
    assert_eq!(at, 0, "入队即清 scheduled_at（取消定时语义）");
    cleanup(&dir);
}

/// 定时时刻 ↔ datetime-local 字符串互转（M4-2）：往返一致、空串/非法返回 None
#[test]
fn sched_helpers_roundtrip_and_reject_invalid() {
    let now = now_epoch();
    // 未来整点：取下一小时边界，保证分钟对齐（datetime-local 精度即分钟）
    let target = now + 3600;
    let local = crate::mail::sched_local_value(target);
    assert_eq!(local.len(), 16, "YYYY-MM-DDTHH:MM 形态：{local}");
    assert!(local.contains('T'));
    let back = crate::mail::sched_epoch_of(&local).expect("合法格式应可解析");
    assert!(
        (back - target).abs() <= 60,
        "往返误差应在分钟粒度内：{back} vs {target}"
    );
    // 空串/空白/非法 → None
    assert_eq!(crate::mail::sched_epoch_of(""), None);
    assert_eq!(crate::mail::sched_epoch_of("   "), None);
    assert_eq!(crate::mail::sched_epoch_of("not-a-date"), None);
    assert_eq!(crate::mail::sched_epoch_of("2026-13-40T99:99"), None);
    // epoch 0 → 空串（未定时不渲染控件值）
    assert_eq!(crate::mail::sched_local_value(0), "");
}

/// 安全：发信前主题/显示名的 CR/LF 必须被清洗（防头注入）。
/// 库内字段可能来自收件解析或用户输入，交 lettre 组包前不能带裸控制字符。
#[test]
fn dispatch_strips_control_chars_from_headers() {
    let (state, dir) = temp_state("send-head-safe");
    let input = DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟\r\nX-Injected: yes".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        bcc_addrs: String::new(),
        subject: "正常主题\r\nBcc: evil@attacker.example".into(),
        body_text: "正文".into(),
        in_reply_to: None,
        scheduled_at: 0,
    };
    state
        .store_password("demo@chinatelecom.cn", "pw-head-safe")
        .unwrap();
    let id = save_draft(&state, &input).unwrap();
    assert!(enqueue_draft(&state, id));
    let sender = FakeSender::ok();
    let smtp = SmtpConfig::from_config(&state.config_snapshot());
    let result = state.with_conn(|c| {
        dispatch_one(
            c,
            &sender,
            id,
            &smtp,
            &dir.join("attachments"),
            &state.key_file(),
        )
    });
    assert_eq!(result.unwrap(), "sent");
    let seen = sender.seen.lock().unwrap();
    let msg = &seen[0].3;
    assert!(
        !msg.subject.contains('\r') && !msg.subject.contains('\n'),
        "主题不得含控制字符：{:?}",
        msg.subject
    );
    assert!(msg.subject.contains("正常主题"), "{:?}", msg.subject);
    assert!(
        !msg.from_name.contains('\r') && !msg.from_name.contains('\n'),
        "显示名不得含控制字符：{:?}",
        msg.from_name
    );
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}
