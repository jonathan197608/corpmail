//! mail_recv 模块单元测试（自 src/mail/recv.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::mail::{get, save_draft};
use crate::state::AppState;
use crate::state::keys;
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

/// 主题解码（GBK 兜底）：未编码的原始 GBK/GB2312 主题头经 mailparse 会退
/// Latin-1 变乱码，decode_subject 应以 GB18030 重解码；RFC2047/UTF-8/西文
/// latin1 主题不受影响
#[test]
fn decode_subject_falls_back_to_gbk_for_raw_header() {
    // 原始 GBK 主题字节（非 RFC2047 编码）
    let (gbk, _, _) = encoding_rs::GBK.encode("十月佣金结算");
    let raw = [
        b"Subject: ".as_slice(),
        gbk.as_ref(),
        b"\r\n\r\n".as_slice(),
    ]
    .concat();
    let parsed = mailparse::parse_mail(&raw).unwrap();
    assert_eq!(
        header_fields(&parsed).subject,
        "十月佣金结算",
        "原始 GBK 主题应被解码为中文"
    );
    // RFC2047（=?utf-8?B?SGVsbG8=?= = "Hello"）：get_value 正常解码，不受兜底影响
    let raw2 = b"Subject: =?utf-8?B?SGVsbG8=?=\r\n\r\n".to_vec();
    assert_eq!(
        header_fields(&mailparse::parse_mail(&raw2).unwrap()).subject,
        "Hello"
    );
    // 纯 UTF-8 直接主题：原样
    let raw3 = "Subject: 直发中文\r\n\r\n".as_bytes().to_vec();
    assert_eq!(
        header_fields(&mailparse::parse_mail(&raw3).unwrap()).subject,
        "直发中文"
    );
    // 西文 latin1（é = 0xE9 单字节）：GB18030 单前导字节不成 CJK，不误改
    let raw4 = b"Subject: caf\xE9\r\n\r\n".to_vec();
    assert_eq!(
        header_fields(&mailparse::parse_mail(&raw4).unwrap()).subject,
        "caf\u{e9}"
    );
}

/// 存量主题 GBK 乱码修复：库内 latin1-mojibake（每字节一 U+00xx 字符）应还原，
/// 且 subject_tokens 同步重算（FTS 检索一致）；西文 latin1 主题不误改
#[test]
fn repair_gbk_subjects_restores_mojibake() {
    let (state, dir) = temp_state("repair-gbk");
    // 手工构造 mojibake：GBK 字节按 latin1 逐字节映射成 String
    let (gbk, _, _) = encoding_rs::GBK.encode("配案清单");
    let mojibake: String = gbk.iter().map(|&b| b as char).collect();
    // 一封真实乱码邮件（旧路径存库的形态）
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                subject_tokens, body_tokens, created_at)
                 VALUES ('in','inbox','received','a@x.cn','b@x.cn',?1,'','',1)",
                [&mojibake],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 二次连接跑修复（state 连接在 open 时已跑过一次，但污染行插入在之后）
    let mut conn = crate::db::open(&dir.join("corpmail.db")).unwrap();
    crate::mail::repair_gbk_subjects(&mut conn).unwrap();
    let (subj, toks): (String, String) = state
        .with_conn(|c| {
            c.query_row(
                "SELECT subject, subject_tokens FROM emails WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(subj, "配案清单", "mojibake 应还原为中文主题");
    assert!(toks.contains("配案"), "FTS 分词列应同步重算");
    // 西文 latin1 主题不误改
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                subject_tokens, body_tokens, created_at)
                 VALUES ('in','inbox','received','a@x.cn','b@x.cn','caf\u{e9}','cafe',0,2)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let mut c2 = crate::db::open(&dir.join("corpmail.db")).unwrap();
    crate::mail::repair_gbk_subjects(&mut c2).unwrap();
    let s: String = state
        .with_conn(|c| c.query_row("SELECT subject FROM emails WHERE id = 2", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(s, "caf\u{e9}", "西文 latin1 主题不应被误改");
    cleanup(&dir);
}

/// 存量正文/HTML 乱码修复（UTF-8 被邮件头误标 gb2312 解码入库的“璇—□”类）：
/// 库内文本按 GB18030 逆推回字节恰为合法 UTF-8 → 还原原文并重算 body_tokens；
/// 正常中文/西文正文不误改；幂等（二次修复空扫）
#[test]
fn repair_mojibake_bodies_restores_misdecoded_utf8() {
    let (state, dir) = temp_state("repair-body");
    // 构造乱码：UTF-8 原文被按 GBK 解码的形态（无丢字，可完整逆推）
    let orig = "申诉已收到，正在核实";
    let (cow, _, had_errors) = encoding_rs::GBK.decode(orig.as_bytes());
    assert!(!had_errors, "测试样本须为无丢字乱码");
    let mangled = cow.into_owned();
    assert_ne!(&mangled, orig);
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                subject_tokens, body_text, body_tokens, body_html, created_at)
                 VALUES ('in','inbox','received','a@x.cn','b@x.cn','误判申诉','',
                        ?1,'',NULL,1),
                        ('in','inbox','received','a@x.cn','b@x.cn','正常主题','',
                        '正常中文正文，标点（），OK。','',NULL,2),
                        ('in','inbox','received','a@x.cn','b@x.cn','西文主题','',
                        'plain ascii body','',?2,3)",
                rusqlite::params![mangled, format!("<p>{mangled}</p>")],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 独立连接跑修复（state 连接 open 时已跑过，污染行插入在之后）
    let mut conn = crate::db::open(&dir.join("corpmail.db")).unwrap();
    crate::mail::repair_mojibake_bodies(&mut conn).unwrap();
    let rows: Vec<(i64, String, String)> = state.with_conn(|c| {
        c.prepare("SELECT id, body_text, body_tokens FROM emails ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    });
    assert_eq!(rows[0].1, orig, "乱码正文应还原为 UTF-8 原文");
    assert!(rows[0].2.contains("核实"), "body_tokens 应同步重算");
    assert_eq!(
        rows[1].1, "正常中文正文，标点（），OK。",
        "正常中文不得误改"
    );
    assert_eq!(rows[2].1, "plain ascii body", "西文正文不得误改");
    let html: Option<String> = state
        .with_conn(|c| {
            c.query_row("SELECT body_html FROM emails WHERE id = 3", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(
        html.as_deref(),
        Some("<p>申诉已收到，正在核实</p>"),
        "乱码 HTML 应同步还原"
    );
    // 幂等：二次修复不再改写
    let mut conn2 = crate::db::open(&dir.join("corpmail.db")).unwrap();
    crate::mail::repair_mojibake_bodies(&mut conn2).unwrap();
    let again: String = state
        .with_conn(|c| {
            c.query_row("SELECT body_text FROM emails WHERE id = 1", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(again, orig, "修复后文本应保持稳定（幂等）");
    cleanup(&dir);
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
        cc_addrs: String::new(),
        to_addrs: "demo@chinatelecom.cn".into(),
        body_text: "请查收本月激励清单。".into(),
        body_kind: "plain".into(),
        body_html: None,
        raw_headers: String::new(),
        attachments: Vec::new(),
        too_large: false,
    }
}

/// 联系人「最近联系」取邮件实际时间（INTERNALDATE），不是入库时刻：
/// 首次收取历史邮件时不得把联系人时间全部写成今天
#[test]
fn store_incoming_uses_mail_date_for_contact_time() {
    let (state, dir) = temp_state("m4-contact-time");
    let mails = vec![incoming(1, false, "时间口径", "<m-time@x.cn>")];
    assert_eq!(store_incoming(&state, &mails).unwrap(), 1);
    let mil = 1_700_000_000_i64;
    let (from_at, to_at): (i64, i64) = state.with_conn(|c| {
        (
            c.query_row(
                "SELECT updated_at FROM contacts WHERE addr = 'boss@x.cn'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            c.query_row(
                "SELECT updated_at FROM contacts WHERE addr = 'demo@chinatelecom.cn'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
        )
    });
    assert_eq!(from_at, mil, "发件人联系时间应为邮件实际时间");
    assert_eq!(to_at, mil, "收件人联系时间应为邮件实际时间");
    assert_ne!(from_at, now_epoch(), "不得退化为入库时间");
    cleanup(&dir);
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
        ("imap.test.cn", 993, "demo@chinatelecom.cn", 42, vec![7u32]),
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
        bcc_addrs: String::new(),
        subject: "本地草稿".into(),
        body_text: "体".into(),
        in_reply_to: None,
        scheduled_at: 0,
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

/// 用户实测场景（2026-11-09 截图）：正文实为 UTF-8 却被声明为 gb2312/gbk，
/// mailparse 按声明解码出“璇—□”类乱码；应按原始字节直接取 UTF-8 原文
#[test]
fn extract_body_repairs_utf8_mislabeled_as_gbk() {
    let body = "您的申诉已收到，我们正在核实考试记录。";
    for cs in ["gb2312", "gbk", "gb18030"] {
        let raw = format!("Content-Type: text/plain; charset={cs}\r\n\r\n{body}");
        let mail = mailparse::parse_mail(raw.as_bytes()).unwrap();
        // 前提：按声明解码确实产生乱码（不应恰好等于原文）
        let d = find_inline_part(&mail, "text/plain")
            .unwrap()
            .get_body()
            .unwrap_or_default();
        assert_ne!(d, body, "{cs} 声明下原文不应被直接解出：{d}");
        let (extracted, _) = extract_body(&mail);
        assert_eq!(
            extracted, body,
            "{cs} 误标 UTF-8 应按原始字节还原：实际 {extracted}"
        );
    }
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
    let (body, html) = extract_body(&mail);
    assert_eq!(body, "heading");
    assert_eq!(html.as_deref(), Some("<h1>heading</h1>"));
    // multipart/alternative：plain 优先
    let mp = b"Content-Type: multipart/alternative; boundary=BOUND\r\n\r\n--BOUND\r\nContent-Type: text/html\r\n\r\n<html>hi</html>\r\n--BOUND\r\nContent-Type: text/plain\r\n\r\nhi plain\r\n--BOUND--\r\n";
    let mail = mailparse::parse_mail(mp).unwrap();
    let (body, html) = extract_body(&mail);
    assert_eq!(body.trim(), "hi plain");
    assert!(
        html.as_deref().unwrap_or("").contains("<html>hi</html>"),
        "HTML 原文应保留（可净化渲染）：{html:?}"
    );
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
    // A5：原始信头块完整保存（含未解码的 RFC2047 原值），且不污染正文
    assert!(
        mail.raw_headers
            .contains("Subject: =?utf-8?B?5pyI5bqm6YWN5qGI?="),
        "应保存原始信头：{}",
        mail.raw_headers
    );
    assert!(
        !mail.raw_headers.contains("body line"),
        "信头块不应包含正文"
    );
}

/// A5 原始信头提取：CRLF / LF-only / 无空行 / 超长截断
#[test]
fn raw_header_block_handles_line_endings_and_bounds() {
    assert_eq!(
        raw_header_block(b"From: a@x.cn\r\nSubject: hi\r\n\r\nbody"),
        "From: a@x.cn\r\nSubject: hi"
    );
    assert_eq!(
        raw_header_block(b"From: a@x.cn\nSubject: hi\n\nbody"),
        "From: a@x.cn\nSubject: hi"
    );
    // 无空行：整段视为信头
    assert_eq!(raw_header_block(b"From: a@x.cn"), "From: a@x.cn");
    // 超长信头截断到 64KB（防恶意巨型头占用内存/撑爆页面）
    let huge = format!("X-Junk: {}\r\n\r\nbody", "a".repeat(100_000));
    let out = raw_header_block(huge.as_bytes());
    assert_eq!(out.len(), 64 * 1024, "超长信头应截断到 64KB");
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
        c.query_row(
            "SELECT id FROM emails WHERE remote_uid = '42:11'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    let d = get(&state, id).unwrap();
    assert!(d.too_large, "详情应带超限标记");
    assert_eq!(d.attach_cnt, 0);
    // 普通邮件不受影响
    let normal = incoming(12, false, "正常邮件", "<n-12@x.cn>");
    store_incoming(&state, &[normal]).unwrap();
    let normal_id: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT id FROM emails WHERE remote_uid = '42:12'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert!(!get(&state, normal_id).unwrap().too_large);
    cleanup(&dir);
}

/// 空信头占位（服务器对超限邮件未返回 HEADER / 返回空）：
/// 主题必须有可见兜底文案而非空串（mailparse 对空输入返回 Ok 空头，不可依赖 Err 分支）
#[test]
fn placeholder_mail_empty_header_gets_subject_fallback() {
    let m = placeholder_mail(7, 8, false, None, &[], 60 * 1024 * 1024, 50 * 1024 * 1024);
    assert!(
        !m.subject.trim().is_empty(),
        "空信头占位主题不得为空串：{:?}",
        m.subject
    );
    assert!(m.too_large);
    assert!(m.body_text.contains("60.0 MB"));
}

/// run 计划：连续同类归并 + 升序不变量（红线：绝不按类别重排，否则崩溃时
/// 游标越过未拉邮件 = 静默漏信）+ 双阈值断 run + 超限块上限
#[test]
fn plan_runs_merges_same_class_and_preserves_order() {
    use super::{BATCH, FetchRun, OVER_CHUNK, plan_runs};
    let mb = 1024u64 * 1024;
    let max = 50 * mb;

    // 空集
    assert!(plan_runs(&[], &HashMap::new(), max).is_empty());

    // 正常/超限交替：按类别断 run 且全局升序
    let uids = [1u32, 2, 3];
    let mut sizes = HashMap::new();
    sizes.insert(2u32, (60 * mb) as u32);
    let runs = plan_runs(&uids, &sizes, max);
    assert_eq!(
        runs,
        vec![
            FetchRun {
                uids: vec![1],
                over: false
            },
            FetchRun {
                uids: vec![2],
                over: true
            },
            FetchRun {
                uids: vec![3],
                over: false
            },
        ],
        "类别翻转即断 run，顺序不重排"
    );

    // 无大小信息（预检失败）：全部按正常拉取，仅 BATCH 封数兜底
    let uids: Vec<u32> = (1..=60).collect();
    let runs = plan_runs(&uids, &HashMap::new(), max);
    assert_eq!(runs.len(), 3);
    assert!(runs.iter().all(|r| !r.over));
    assert_eq!(runs[0].uids.len(), BATCH);
    assert_eq!(runs[2].uids.len(), 10);

    // 字节预算：已知大小累计超 32MB 即断 run（与 flush 预算同源）
    let uids = [1u32, 2];
    let mut sizes = HashMap::new();
    sizes.insert(1u32, (20 * mb) as u32);
    sizes.insert(2u32, (20 * mb) as u32);
    let runs = plan_runs(&uids, &sizes, 200 * mb);
    assert_eq!(runs.len(), 2, "20+20MB 超 32MB 预算应断开");
    assert!(runs.iter().all(|r| !r.over));

    // 单封大但未超限（max=200MB）：自成一个 run，不把后续邮件拖进同一响应
    let uids = [1u32, 2];
    let mut sizes = HashMap::new();
    sizes.insert(1u32, (100 * mb) as u32);
    sizes.insert(2u32, mb as u32);
    let runs = plan_runs(&uids, &sizes, 200 * mb);
    assert_eq!(runs.len(), 2, "100MB 单封独占一个响应，峰值受控");

    // 全超限：OVER_CHUNK 封一块，不受字节预算约束（只拉信头，代价低）
    let uids: Vec<u32> = (1..=250).collect();
    let mut sizes = HashMap::new();
    for u in &uids {
        sizes.insert(*u, (60 * mb) as u32);
    }
    let runs = plan_runs(&uids, &sizes, max);
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.over));
    assert_eq!(runs[0].uids.len(), OVER_CHUNK);
    assert_eq!(runs[1].uids.len(), 50);

    // 升序不变量：任意混合输入，拼接全部 run 的 uids == 输入（无重排、无丢失）
    let uids: Vec<u32> = [3, 9, 9, 1, 1, 1, 2, 5].iter().map(|&x| x + 10).collect();
    let mut sizes = HashMap::new();
    for (i, u) in uids.iter().enumerate() {
        if i % 3 == 0 {
            sizes.insert(*u, (60 * mb) as u32);
        }
    }
    let runs = plan_runs(&uids, &sizes, max);
    let flattened: Vec<u32> = runs.iter().flat_map(|r| r.uids.iter().copied()).collect();
    assert_eq!(flattened, uids, "run 划分必须保序保量");
}

#[test]
fn store_incoming_sent_uses_out_folder_and_prefixed_remote_uid() {
    let (state, dir) = temp_state("m4-sent");
    // 发件箱与收件箱的 UIDVALIDITY 独立，v/u 可能完全相同：
    // 前缀 + 独立 folder/status 保证两箱并存不冲突
    let inbox = incoming(1, false, "收件甲", "<a@x.cn>");
    let sent = IncomingMail {
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        ..incoming(1, true, "发件甲", "<b@x.cn>")
    };
    let added = state.with_conn(|conn| {
        let mut n = store_incoming_conn(conn, &state.data_dir(), &[inbox], MailDest::Inbox)?;
        n += store_incoming_conn(conn, &state.data_dir(), &[sent], MailDest::Sent)?;
        Ok::<usize, String>(n)
    });
    assert_eq!(added.unwrap(), 2, "同 v/u 的收件与发件应各自入库");
    let rows: Vec<(String, String, String, String, bool, bool)> = state.with_conn(|c| {
        c.prepare(
            "SELECT remote_uid, direction, folder, status, unread = 1,
                    sent_at IS NOT NULL AND received_at IS NULL
             FROM emails ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    });
    assert_eq!(rows[0].0, "42:1", "收件 remote_uid 不带前缀");
    assert_eq!(
        (rows[0].1.as_str(), rows[0].2.as_str(), rows[0].3.as_str()),
        ("in", "inbox", "received")
    );
    assert!(rows[0].4, "\\Seen 缺失应未读");
    assert!(!rows[0].5, "收件只写 received_at");
    assert_eq!(rows[1].0, "sent:42:1", "发件 remote_uid 必须带 sent: 前缀");
    assert_eq!(
        (rows[1].1.as_str(), rows[1].2.as_str(), rows[1].3.as_str()),
        ("out", "sent", "sent")
    );
    assert!(!rows[1].4, "发件箱恒已读");
    assert!(rows[1].5, "发件只写 sent_at");
    cleanup(&dir);
}

#[test]
fn store_incoming_dedupes_message_id_within_direction_only() {
    let (state, dir) = temp_state("m4-sent-dedup");
    // 自己发给自己：同一 message_id 会同时落在 Sent 与 Inbox，跨方向不得互吞
    let msg = "<self@chinatelecom.cn>";
    let inbox = IncomingMail {
        from_addr: "demo@chinatelecom.cn".into(),
        to_addrs: "demo@chinatelecom.cn".into(),
        ..incoming(7, false, "自留副本", msg)
    };
    let sent = IncomingMail {
        from_addr: "demo@chinatelecom.cn".into(),
        to_addrs: "demo@chinatelecom.cn".into(),
        ..incoming(7, true, "自留副本", msg)
    };
    let added = state.with_conn(|conn| {
        let mut n = store_incoming_conn(conn, &state.data_dir(), &[inbox], MailDest::Inbox)?;
        n += store_incoming_conn(conn, &state.data_dir(), &[sent], MailDest::Sent)?;
        // 同方向重复：应被去重（收件箱靠消息 id / 发件箱靠前缀 remote_uid）
        let again_in = IncomingMail {
            uid: 8,
            ..incoming(8, false, "自留副本", msg)
        };
        let again_sent = IncomingMail {
            uid: 9,
            ..incoming(9, true, "自留副本", msg)
        };
        n += store_incoming_conn(conn, &state.data_dir(), &[again_in], MailDest::Inbox)?;
        n += store_incoming_conn(conn, &state.data_dir(), &[again_sent], MailDest::Sent)?;
        Ok::<usize, String>(n)
    });
    assert_eq!(added.unwrap(), 2, "跨方向各存一份，同方向重复被去重");
    let dirs: Vec<String> = state.with_conn(|c| {
        c.prepare("SELECT direction FROM emails ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    });
    assert_eq!(dirs, vec!["in".to_string(), "out".to_string()]);
    cleanup(&dir);
}

#[test]
fn decode_mutf7_handles_rfc3501_mailbox_names() {
    // 回归（发件箱同步拉不到）：中文服务器的 LIST 返回 modified UTF-7 名称，
    // 旧实现直接在原始串上找「已发送」永远匹配不上，发件箱被静默跳过
    assert_eq!(decode_mutf7("INBOX"), "INBOX");
    assert_eq!(decode_mutf7("Sent Items"), "Sent Items");
    // 「已发送」的 mUTF-7 编码（RFC 3501 §5.1.3 字母表：, 代替 /）
    assert_eq!(decode_mutf7("&XfJT0ZAB-"), "已发送");
    // 维基已知样例：日本語
    assert_eq!(decode_mutf7("&ZeVnLIqe-"), "日本語");
    // 增补平面字符（代理对）：U+20BB9 → UTF-16 D842 DFB9（32 bit）→
    // 6bit 分组 110110|000100|001011|011111|101110|01+0000 → 2ELfuQ（末组补零）
    assert_eq!(decode_mutf7("&2ELfuQ-"), "\u{20BB9}");
    // &- 转义 = 字面 &
    assert_eq!(decode_mutf7("A&-B"), "A&B");
    // 纯 ASCII 混合
    assert_eq!(
        decode_mutf7("Prefix.&XfJT0ZAB-.Suffix"),
        "Prefix.已发送.Suffix"
    );
    // 非法输入：无结束符的段原样保留，绝不 panic/吞串
    assert_eq!(decode_mutf7("&XfJT0ZAB"), "&XfJT0ZAB");
    assert_eq!(decode_mutf7("尾随&"), "尾随&");
}
