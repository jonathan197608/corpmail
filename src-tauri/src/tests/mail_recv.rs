//! mail_recv 模块单元测试（自 src/mail/recv.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::mail::{get, save_draft};
use crate::state::AppState;
use std::path::PathBuf;

/// 假收件器：记录批量回写调用参数（fetch 路径由真实 fetcher 单独覆盖，不测网络）
type SeenCall = (String, u16, String, u32, Vec<u32>);

struct FakeFetcher {
    seen_marks: std::sync::Mutex<Vec<SeenCall>>,
}

impl FakeFetcher {
    fn new() -> Self {
        Self {
            seen_marks: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl MailFetcher for FakeFetcher {
    fn fetch(
        &self,
        _host: &str,
        _port: u16,
        _mode: TlsMode,
        _account: &str,
        _password: &str,
        _first_days: i64,
        _last: LastCursor,
        _max_mail_bytes: u64,
    ) -> Result<FetchOutcome, String> {
        Ok(FetchOutcome {
            uidvalidity: 0,
            max_uid: 0,
            mails: Vec::new(),
        })
    }

    fn mark_seen_batch(
        &self,
        host: &str,
        port: u16,
        _mode: TlsMode,
        account: &str,
        _password: &str,
        uidvalidity: u32,
        uids: &[u32],
    ) -> Result<(), String> {
        self.seen_marks.lock().unwrap().push((
            host.into(),
            port,
            account.into(),
            uidvalidity,
            uids.to_vec(),
        ));
        Ok(())
    }
}

fn temp_state(tag: &str) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("corpmail-recv-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (AppState::open(&dir), dir)
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn incoming(uid: u32, seen: bool, subject: &str, message_id: &str) -> IncomingMail {
    IncomingMail {
        uidvalidity: 42,
        uid,
        seen,
        internal_date: Some(1_700_000_000),
        message_id: message_id.into(),
        subject: subject.into(),
        from_addr: "boss@x.cn".into(),
        from_name: "部门领导".into(),
        to_addrs: "demo@chinatelecom.cn".into(),
        body_text: "请查收本月激励清单。".into(),
        body_kind: "plain".into(),
        attachments: Vec::new(),
        too_large: false,
    }
}

#[test]
fn store_incoming_dedupes_and_sets_unread() {
    let (state, dir) = temp_state("m4-store");
    let mails = vec![
        incoming(1, false, "激励清单", "<m-1@x.cn>"),
        incoming(2, true, "已读通知", "<m-2@x.cn>"),
        // 同 uid（同 remote_uid）重复：应跳过
        incoming(1, false, "激励清单", "<m-1@x.cn>"),
        // 新 uid 但 message_id 已存在：应跳过
        incoming(9, false, "清单转发", "<m-1@x.cn>"),
    ];
    let added = store_incoming(&state, &mails).unwrap();
    assert_eq!(added, 2, "去重后仅入库两封");
    let rows: Vec<(String, bool, String)> = state.with_conn(|c| {
        c.prepare("SELECT remote_uid, unread = 1, message_id FROM emails ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    });
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "42:1");
    assert!(rows[0].1, "\\Seen 缺失应置未读");
    assert!(!rows[1].1, "含 \\Seen 应置已读");
    // FTS 索引同步（分词影子列由触发器搬运；列限定避免正文命中干扰断言）
    let hit: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM email_fts WHERE email_fts MATCH 'subject:激励'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(hit, 1, "收件应同步入 FTS 索引");
    // 附件入库（conn 版两级存储路径）
    let with_attach = IncomingMail {
        attachments: vec![IncomingAttach {
            filename: "清单.xlsx".into(),
            mime: "application/vnd.ms-excel".into(),
            data: b"xlsx-bytes".to_vec(),
        }],
        ..incoming(3, false, "带附件来件", "<m-3@x.cn>")
    };
    let added = store_incoming(&state, &[with_attach]).unwrap();
    assert_eq!(added, 1);
    let d = get(&state, 3).unwrap();
    assert_eq!(d.attach_cnt, 1);
    assert_eq!(d.attachments[0].filename, "清单.xlsx");
    cleanup(&dir);
}

#[test]
fn mark_seen_local_and_delegates_to_fetcher() {
    let (state, dir) = temp_state("m4-seen");
    state
        .set_batch(&[
            (keys::RECV_HOST, "imap.test.cn"),
            (keys::RECV_PORT, "993"),
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
        ])
        .unwrap();
    state
        .store_password("demo@chinatelecom.cn", "pw-m4")
        .unwrap();
    let mails = vec![incoming(7, false, "未读来件", "<m-7@x.cn>")];
    store_incoming(&state, &mails).unwrap();
    let fetcher = FakeFetcher::new();
    mark_seen(&state, &fetcher, 1).unwrap();
    let unread: i64 = state.with_conn(|c| {
        c.query_row("SELECT unread FROM emails WHERE id=1", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(unread, 0, "本地应置已读");
    let calls = fetcher.seen_marks.lock().unwrap();
    assert_eq!(calls.len(), 1, "应回写服务器一次");
    assert_eq!(
        (
            calls[0].0.as_str(),
            calls[0].1,
            calls[0].2.as_str(),
            calls[0].3,
            calls[0].4.clone()
        ),
        (
            "imap.test.cn",
            993,
            "demo@chinatelecom.cn",
            42,
            vec![7u32]
        ),
        "回写参数应来自设置与 remote_uid"
    );
    // 已读后再次调用：本地无变化、不再回写
    drop(calls);
    let fetcher2 = FakeFetcher::new();
    // mark_seen 幂等语义：显式再调仍会尝试回写（服务器对齐），本地不变
    mark_seen(&state, &fetcher2, 1).unwrap();
    assert_eq!(
        fetcher2.seen_marks.lock().unwrap().len(),
        1,
        "重复调用仍对齐服务器"
    );
    // 本地草稿（无 remote_uid）：仅本地，不回写
    let input = crate::mail::DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        subject: "本地草稿".into(),
        body_text: "体".into(),
        in_reply_to: None,
    };
    let draft_id = save_draft(&state, &input).unwrap();
    let fetcher3 = FakeFetcher::new();
    mark_seen(&state, &fetcher3, draft_id).unwrap();
    assert_eq!(
        fetcher3.seen_marks.lock().unwrap().len(),
        0,
        "本地邮件不应触发回写"
    );
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn strip_html_removes_tags_and_decodes_entities() {
    let out = strip_html("<p>月租<b>清单</b>&nbsp;&amp;&nbsp;报表</p>");
    assert_eq!(out, "月租清单 & 报表");
    assert_eq!(strip_html("无标签"), "无标签");
    assert_eq!(strip_html("a & lt"), "a & lt", "未闭合实体原样保留");
}

#[test]
fn imap_since_produces_imap_date_format() {
    let s = imap_since(7);
    // 形如 21-Oct-2026（DD-Mon-YYYY）
    let parts: Vec<&str> = s.split('-').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].len(), 2);
    assert_eq!(parts[1].len(), 3, "月份缩写三字母：{s}");
    assert_eq!(parts[2].len(), 4, "年份四位：{s}");
}

#[test]
fn extract_body_repairs_utf8_mislabeled_as_latin1() {
    // 用户实测场景：研发云平台邮件 UTF-8 正文被网关误标 iso-8859-1，
    // "原文"逐字节解码成 "åŽŸæ–‡"（0x8E→Ž/0x96→– 均为 cp1252 特有映射）
    let body = "欢迎使用研发云平台！·您已被加入项目组，请查收。";
    for cs in ["iso-8859-1", "windows-1252"] {
        let raw = format!("Content-Type: text/plain; charset={cs}\r\n\r\n{body}");
        let mail = mailparse::parse_mail(raw.as_bytes()).unwrap();
        if let Some(p) = find_inline_part(&mail, "text/plain") {
            let d = p.get_body().unwrap_or_default();
            let mangled = d.contains("åŽŸ") || !d.chars().any(is_cjk);
            assert!(mangled, "前提：{cs} 声明下确实产生乱码");
        }
        let (extracted, _) = extract_body(&mail);
        assert_eq!(
            extracted, body,
            "{cs} 误标 UTF-8 应被修复为原文：实际 {extracted}"
        );
    }
}

#[test]
fn extract_body_keeps_correct_utf8_and_gbk_labeled() {
    // 声明正确（utf-8）：回归保护，不改写
    let body = "正常中文正文，标点（），OK。";
    let raw = format!("Content-Type: text/plain; charset=utf-8\r\n\r\n{body}");
    let mail = mailparse::parse_mail(raw.as_bytes()).unwrap();
    let (extracted, _) = extract_body(&mail);
    assert_eq!(extracted, body);

    // 声明 gbk 且实为 GBK 字节：语义结果应是中文（mailparse 直解或兜底解均可）
    let (gbk_bytes, _, _) = encoding_rs::GBK.encode("已按GBK编码的中文正文。");
    let mut raw_gbk = b"Content-Type: text/plain; charset=gbk\r\n\r\n".to_vec();
    raw_gbk.extend_from_slice(&gbk_bytes);
    let mail2 = mailparse::parse_mail(&raw_gbk).unwrap();
    let (extracted2, _) = extract_body(&mail2);
    assert!(
        extracted2.chars().any(is_cjk),
        "GBK 语义正文应解出中文：{extracted2}"
    );
}

#[test]
fn extract_body_falls_back_to_gbk_when_charset_undeclared() {
    // 无 charset 头（mailparse 默认 us-ascii）+ GBK 字节：兜底解回中文
    let (gbk_bytes, _, _) = encoding_rs::GBK.encode("无声明场景：中文正文应能显示。");
    let mut raw = b"Content-Type: text/plain\r\n\r\n".to_vec();
    raw.extend_from_slice(&gbk_bytes);
    let mail = mailparse::parse_mail(&raw).unwrap();
    let (extracted, _) = extract_body(&mail);
    assert!(
        extracted.chars().any(is_cjk),
        "未声明 charset 的 GBK 正文应兜底解出中文：{extracted}"
    );
}

#[test]
fn fix_mojibake_does_not_touch_normal_western_text() {
    // 正常西文（cp1252 合法文本）：不含 CJK、逆推非合法 UTF-8 或无 CJK → 不改写
    let french = "Café déjà vu — très bien, ça va.";
    assert_eq!(fix_mojibake(french), None);
    // 纯 ASCII 不处理
    assert_eq!(fix_mojibake("plain ascii text"), None);
    // 已含正常中文不改写
    assert_eq!(fix_mojibake("正常中文 with é"), None);

    // 真 mojibake：手动构造（"原文测试"的 UTF-8 字节按 cp1252 解码后的文本）
    let mangled: String = "原文测试"
        .as_bytes()
        .iter()
        .map(|b| {
            let u = *b as u32;
            if (0x80..=0xFF).contains(&u) {
                // cp1252 逆推区：0x80-0x9F 特有映射 + 0xA0-0xFF 直通
                let bytes = [*b];
                encoding_rs::WINDOWS_1252
                    .decode(&bytes)
                    .0
                    .chars()
                    .next()
                    .unwrap()
            } else {
                char::from(*b)
            }
        })
        .collect();
    assert_eq!(fix_mojibake(&mangled).as_deref(), Some("原文测试"));
}

#[test]
fn extract_body_prefers_plain_and_falls_back_to_html() {
    let plain_mime = b"Content-Type: text/plain; charset=utf-8\r\n\r\nplain content";
    let mail = mailparse::parse_mail(plain_mime).unwrap();
    assert_eq!(extract_body(&mail).0, "plain content");
    let html_mime = b"Content-Type: text/html\r\n\r\n<h1>heading</h1>";
    let mail = mailparse::parse_mail(html_mime).unwrap();
    let (body, kind) = extract_body(&mail);
    assert_eq!((body.as_str(), kind), ("heading", "html"));
    // multipart/alternative：plain 优先
    let mp = b"Content-Type: multipart/alternative; boundary=BOUND\r\n\r\n--BOUND\r\nContent-Type: text/html\r\n\r\n<html>hi</html>\r\n--BOUND\r\nContent-Type: text/plain\r\n\r\nhi plain\r\n--BOUND--\r\n";
    let mail = mailparse::parse_mail(mp).unwrap();
    let (body, kind) = extract_body(&mail);
    assert_eq!((body.trim(), kind), ("hi plain", "plain"));
    // 附件识别
    let att = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nbody here\r\n--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"data.bin\"\r\n\r\nBINARY\r\n--B--\r\n";
    let mail = mailparse::parse_mail(att).unwrap();
    let attachments = collect_attachments(&mail);
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].filename, "data.bin");
    assert_eq!(attachments[0].data, b"BINARY");
}

#[test]
fn parse_incoming_extracts_headers_and_body() {
    let raw = b"From: =?utf-8?B?6YOo6Zeo6aKG5a+8?=<boss@x.cn>\r\n\
                To: demo@chinatelecom.cn\r\n\
                Subject: =?utf-8?B?5pyI5bqm6YWN5qGI?=\r\n\
                Message-Id: <abc@x.cn>\r\n\
                Content-Type: text/plain\r\n\r\nbody line";
    let mail = parse_incoming(7, 99, false, Some(1234), raw).unwrap();
    assert_eq!(mail.subject, "月度配案");
    assert_eq!(mail.from_addr, "boss@x.cn");
    assert_eq!(mail.body_text, "body line");
    assert_eq!(mail.message_id, "<abc@x.cn>");
    assert!(!mail.seen);
    assert_eq!(mail.uid, 99);
}

#[test]
fn parse_incoming_tolerates_malformed_mime() {
    // 模糊面探针：畸形输入不 panic、有界返回（真实邮件网关产物千奇百怪）
    let cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),                                                           // 空报文
        b"\r\n".to_vec(),                                                       // 纯换行
        b"From:".to_vec(),                                                      // 无值头
        b"From: \x80\x81\xff".to_vec(),                                         // 非法 UTF-8 字节
        b"Subject: =?utf-8?B?????=".to_vec(),                                   // 损坏 RFC2047
        b"Subject: =?bogus-charset?Q?x?=".to_vec(),                             // 未知 charset
        b"Content-Type: multipart/mixed; boundary=".to_vec(),                   // 空 boundary
        b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\n".to_vec(),   // 不闭合 multipart
        b"Content-Type: multipart/mixed\r\n\r\n[no parts]".to_vec(), // 无 boundary 的 multipart
        b"Subject: a\r\n\r\nb\xc3(b\xd1\xff".to_vec(),               // 正文非法 UTF-8
        format!("Subject: {}\r\n\r\nx", "\u{4e00}".repeat(50000)).into_bytes(), // 超长主题
    ];
    for (i, raw) in cases.iter().enumerate() {
        let out = parse_incoming(1, 1, false, Some(1), raw);
        // 允许 None（解析失败跳过）或 Some；断言主题有界
        if let Some(m) = out {
            assert!(m.subject.chars().count() < 100_000, "case {i} 主题超界");
        }
    }
}

#[test]
fn walk_attachments_tolerates_deep_nesting() {
    // 深嵌套 multipart：100 层（真实炸弹邮件结构）。递归 walk_attachments
    // 必须不栈溢出（100 层远低于触发线则通过；真正防线是 IMAP 拉取大小本身）
    let mut raw = String::from("Content-Type: multipart/mixed; boundary=B\r\n\r\n");
    for _ in 0..100 {
        raw.push_str("--B\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n");
    }
    raw.push_str("--B\r\nContent-Type: text/plain\r\n\r\nhi\r\n--B--\r\n");
    let parsed = mailparse::parse_mail(raw.as_bytes()).unwrap();
    let _ = collect_attachments(&parsed); // 不 panic 即过
}

/// 超大邮件占位：保留信头派生字段（主题/发件人/Message-Id），标记 too_large，
/// 正文为可操作的说明，附件为空
#[test]
fn placeholder_mail_keeps_headers_and_flags_too_large() {
    let header = b"From: =?utf-8?B?6YOo6Zeo6aKG5a+8?=<boss@x.cn>\r\n\
                   To: demo@chinatelecom.cn\r\n\
                   Subject: =?utf-8?B?5pyI5bqm6YWN5qGI?=\r\n\
                   Message-Id: <big@x.cn>\r\n\r\n";
    let m = placeholder_mail(
        42,
        5,
        false,
        Some(1234),
        header,
        120 * 1024 * 1024,
        50 * 1024 * 1024,
    );
    assert!(m.too_large, "超限邮件必须标记");
    assert_eq!(m.subject, "月度配案");
    assert_eq!(m.from_addr, "boss@x.cn");
    assert_eq!(m.message_id, "<big@x.cn>");
    assert!(m.attachments.is_empty(), "超限不下载附件");
    assert!(
        m.body_text.contains("120.0 MB") && m.body_text.contains("50 MB"),
        "正文应说明实际大小与上限：{}",
        m.body_text
    );
    assert!(m.body_text.contains("收件同步"), "正文应给出改法");
    assert_eq!((m.uidvalidity, m.uid), (42, 5));
}

/// 占位邮件入库：too_large 持久化，详情页据此显示提示条
#[test]
fn store_incoming_persists_too_large_flag() {
    let (state, dir) = temp_state("m4-toolarge");
    let mut m = incoming(11, false, "超大附件", "<big-11@x.cn>");
    m.too_large = true;
    m.body_text = "（该邮件约 120.0 MB，超过本地单封收件上限 50 MB…）".into();
    assert_eq!(store_incoming(&state, &[m]).unwrap(), 1);
    let id: i64 = state.with_conn(|c| {
        c.query_row("SELECT id FROM emails WHERE remote_uid = '42:11'", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    let d = get(&state, id).unwrap();
    assert!(d.too_large, "详情应带超限标记");
    assert_eq!(d.attach_cnt, 0);
    // 普通邮件不受影响
    let normal = incoming(12, false, "正常邮件", "<n-12@x.cn>");
    store_incoming(&state, &[normal]).unwrap();
    let normal_id: i64 = state.with_conn(|c| {
        c.query_row("SELECT id FROM emails WHERE remote_uid = '42:12'", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert!(!get(&state, normal_id).unwrap().too_large);
    cleanup(&dir);
}
