//! IMAP 收件子模块（M4，设计 §6.1/§6.2）
//!
//! RealFetcher：连接 → SELECT INBOX → 增量 UID 搜索（uid+1:* 续拉，空命中 SINCE 兜底）
//! → 逐封 FETCH → mailparse 解析（编码修复：latin1/cp1252 误标 mojibake 逆推 + GBK 兜底）。
//! 入库 store_incoming_conn：每封一事务（正文行 + 附件原子提交），remote_uid/message_id 双去重。

use super::{
    AppState, AttachCtx, attach_root_of, e, inline_limit_conn, insert_attachment, is_cjk,
    mark_read, month_dir_conn, now_epoch, safe_filename, sync_attach_stats, tokenize, zoned_of,
};
use crate::state::{defaults, full_address, keys};
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};

/// 收件附件（内存字节；来自 MIME 解析）
pub struct IncomingAttach {
    pub filename: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// 收件结果单封形态（fetcher 返回；入库走 store_incoming）
pub struct IncomingMail {
    pub uidvalidity: u32,
    pub uid: u32,
    /// 未含 \Seen 则置未读（§6.1）
    pub seen: bool,
    pub internal_date: Option<i64>,
    pub message_id: String,
    pub subject: String,
    pub from_addr: String,
    pub from_name: String,
    pub to_addrs: String,
    pub body_text: String,
    pub body_kind: String,
    pub attachments: Vec<IncomingAttach>,
}

/// 收件游标（settings 的 last_uidvalidity/last_uid 快照）
#[derive(Default, Clone, Copy)]
pub struct LastCursor {
    pub uidvalidity: u32,
    pub uid: u32,
}

/// 一轮收件结果（游标推进依据 = 搜索层 max_uid，解析失败/入库去重不阻塞游标）
#[allow(dead_code)] // 供单测 MockFetcher 实现 trait fetch；生产走 fetch_batched 分批路径
pub struct FetchOutcome {
    /// 服务器 INBOX 的 UIDVALIDITY（空轮也记录，防 uidvalidity 变化后误判）
    pub uidvalidity: u32,
    /// 本轮 UID SEARCH 命中的最大 UID（0 = 空轮，游标保持）
    pub max_uid: u32,
    /// 解析成功的待入库邮件
    pub mails: Vec<IncomingMail>,
}

/// IMAP 通道注入点（生产 imap crate + mailparse；单测注入假实现，与 MailSender 同模式）
pub trait MailFetcher {
    /// 增量拉取：last.uid > 0 且 uidvalidity 未变时从 uid+1 续拉；
    /// uidvalidity 变化或首次按 SINCE 范围（设计 §6.1：提示全量重拉由调用方清理语义）
    #[allow(dead_code)] // 生产 fetch_round 走 RealFetcher::fetch_batched；trait 保留供单测 MockFetcher 注入
    fn fetch(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        first_days: i64,
        last: LastCursor,
    ) -> Result<FetchOutcome, String>;

    /// 服务器未读回写（SELECT 校验 uidvalidity 后 STORE +Flags \Seen）
    fn mark_seen(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        uidvalidity: u32,
        uid: u32,
    ) -> Result<(), String>;
}

/// 生产实现：imap 3.0.0-alpha.15（隐式 TLS）+ mailparse 0.18（MIME 解析，spike 已验证）
pub struct RealFetcher;
impl RealFetcher {
    /// 分批拉取（M6）：单会话内把 uids 按 BATCH 切块，逐批 uid_fetch → 解析 → 回调
    /// on_batch 入库。实现"边拉边入库边显示进度"，不再等整批拉完才落库。
    /// 首拉/增量/兜底搜索语义与 fetch 完全一致；返回 (uidvalidity, max_uid)。
    /// （fetch_round 走此方法；trait fetch 保留供 Mock 与批量一次性场景）
    #[allow(clippy::too_many_arguments)] // 收件参数集（host/port/account/password/range/游标/回调）本就完整
    pub fn fetch_batched(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        first_days: i64,
        last: LastCursor,
        mut on_batch: impl FnMut(&[IncomingMail], usize, usize) -> Result<(), String>,
    ) -> Result<(u32, u32), String> {
        const BATCH: usize = 25;
        let client = imap::ClientBuilder::new(host, port)
            .mode(imap::ConnectionMode::Tls)
            .connect()
            .map_err(|e| format!("连接失败：{e}"))?;
        let mut sess = client
            .login(account, password)
            .map_err(|(e, _)| format!("登录失败：{e}"))?;
        let mailbox = sess
            .select("INBOX")
            .map_err(|e| format!("无法打开收件箱：{e}"))?;
        let uidvalidity = mailbox.uid_validity.unwrap_or(0);
        // 首拉范围 / 增量 / 兜底（与 fetch 相同语义）
        let first_pull: Vec<u32> = if first_days <= 0 {
            sess.uid_search("ALL")
                .map_err(|e| format!("检索全部邮件失败：{e}"))?
                .into_iter()
                .collect()
        } else {
            let since = imap_since(first_days);
            sess.uid_search(format!("SINCE {since}"))
                .map_err(|e| format!("检索新邮件失败：{e}"))?
                .into_iter()
                .collect()
        };
        let uids: Vec<u32> = if last.uid > 0 && last.uidvalidity == uidvalidity {
            let inc: Vec<u32> = sess
                .uid_search(format!("{}:*", last.uid + 1))
                .map_err(|e| format!("检索新邮件失败：{e}"))?
                .into_iter()
                .filter(|u| *u > last.uid)
                .collect();
            if inc.is_empty() { first_pull } else { inc }
        } else {
            first_pull
        };
        // 分批拉取 + 回调入库（每批 BATCH 封；处理数 processed 作进度 done）
        let total = uids.len();
        let mut processed = 0usize;
        let mut batch: Vec<IncomingMail> = Vec::with_capacity(BATCH);
        let mut batch_uids: Vec<u32> = Vec::with_capacity(BATCH);
        for uid in uids.iter().copied() {
            let fetches = sess
                .uid_fetch(uid.to_string(), "(FLAGS INTERNALDATE BODY.PEEK[])")
                .map_err(|e| format!("拉取邮件 {uid} 失败：{e}"))?;
            for f in fetches.iter() {
                let seen = f
                    .flags()
                    .iter()
                    .any(|flag| flag.to_string().contains("\\Seen"));
                let internal_date = f.internal_date().map(|dt| dt.timestamp());
                let Some(body) = f.body() else {
                    eprintln!("[fetch] uid={uid} 无 BODY，跳过");
                    continue;
                };
                if let Some(mail) = parse_incoming(uidvalidity, uid, seen, internal_date, body) {
                    batch.push(mail);
                }
            }
            batch_uids.push(uid);
            processed += 1;
            if batch_uids.len() >= BATCH {
                on_batch(&batch, processed, total)?;
                batch.clear();
                batch_uids.clear();
            }
        }
        if !batch_uids.is_empty() {
            on_batch(&batch, processed, total)?;
        }
        let max_uid = uids.iter().copied().max().unwrap_or(0);
        let _ = sess.logout();
        Ok((uidvalidity, max_uid))
    }
}

impl MailFetcher for RealFetcher {
    fn fetch(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        first_days: i64,
        last: LastCursor,
    ) -> Result<FetchOutcome, String> {
        let client = imap::ClientBuilder::new(host, port)
            .mode(imap::ConnectionMode::Tls)
            .connect()
            .map_err(|e| format!("连接失败：{e}"))?;
        let mut sess = client
            .login(account, password)
            .map_err(|(e, _)| format!("登录失败：{e}"))?;
        let mailbox = sess
            .select("INBOX")
            .map_err(|e| format!("无法打开收件箱：{e}"))?;
        let uidvalidity = mailbox.uid_validity.unwrap_or(0);
        // 首拉范围：0 = 全部（UID SEARCH ALL，不限天数）；>0 = SINCE 天数。
        // 首拉只发生在无游标/uidvalidity 变化时；巨箱建议先用 7/30 天再补全。
        let first_pull: Vec<u32> = if first_days <= 0 {
            sess.uid_search("ALL")
                .map_err(|e| format!("检索全部邮件失败：{e}"))?
                .into_iter()
                .collect()
        } else {
            let since = imap_since(first_days);
            sess.uid_search(format!("SINCE {since}"))
                .map_err(|e| format!("检索新邮件失败：{e}"))?
                .into_iter()
                .collect()
        };
        // 增量范围：uidvalidity 未变且有游标 → uid+1 起续拉；否则用首拉范围（§6.1）。
        // 范围写法必须是 "N:*"（UID 范围）；曾误写 "N,*"（set 语义 = UID 恰为 N 或最大值），
        // 部分服务器（如 Coremail）下增量结果不可靠 —— M5 实测修复。
        // 增量命中 0 封时再用首拉范围兜底重查（个别服务器对 UID 范围的返回仍有 quirk；
        // 入库按 remote_uid/message_id 幂等去重，重复拉取无害）
        let uids: Vec<u32> = if last.uid > 0 && last.uidvalidity == uidvalidity {
            let inc: Vec<u32> = sess
                .uid_search(format!("{}:*", last.uid + 1))
                .map_err(|e| format!("检索新邮件失败：{e}"))?
                .into_iter()
                .filter(|u| *u > last.uid)
                .collect();
            if inc.is_empty() {
                eprintln!(
                    "[fetch] 增量命中 0 封，按首拉范围兜底重查（{} 封）",
                    first_pull.len()
                );
                first_pull
            } else {
                inc
            }
        } else {
            first_pull
        };
        eprintln!(
            "[fetch] INBOX uidvalidity={uidvalidity} 游标 uid={} 命中 {} 封",
            last.uid,
            uids.len()
        );
        let mut mails = Vec::new();
        for uid in uids.iter().copied() {
            let fetches = sess
                .uid_fetch(uid.to_string(), "(FLAGS INTERNALDATE BODY.PEEK[])")
                .map_err(|e| format!("拉取邮件 {uid} 失败：{e}"))?;
            for f in fetches.iter() {
                let seen = f
                    .flags()
                    .iter()
                    .any(|flag| flag.to_string().contains("\\Seen"));
                let internal_date = f.internal_date().map(|dt| dt.timestamp());
                let Some(body) = f.body() else {
                    eprintln!("[fetch] uid={uid} 无 BODY，跳过");
                    continue;
                };
                match parse_incoming(uidvalidity, uid, seen, internal_date, body) {
                    Some(mail) => mails.push(mail),
                    None => eprintln!("[fetch] uid={uid} MIME 解析失败，跳过"),
                }
            }
        }
        let _ = sess.logout();
        eprintln!(
            "[fetch] 拉取完成：命中 {} 封，解析成功 {} 封",
            uids.len(),
            mails.len()
        );
        Ok(FetchOutcome {
            uidvalidity,
            max_uid: uids.iter().copied().max().unwrap_or(0),
            mails,
        })
    }

    fn mark_seen(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        uidvalidity: u32,
        uid: u32,
    ) -> Result<(), String> {
        let client = imap::ClientBuilder::new(host, port)
            .mode(imap::ConnectionMode::Tls)
            .connect()
            .map_err(|e| format!("连接失败：{e}"))?;
        let mut sess = client
            .login(account, password)
            .map_err(|(e, _)| format!("登录失败：{e}"))?;
        let mailbox = sess
            .select("INBOX")
            .map_err(|e| format!("无法打开收件箱：{e}"))?;
        if mailbox.uid_validity.unwrap_or(0) != uidvalidity {
            return Err("邮箱 UID 已重排，跳过回写（下次收件自动对齐）".into());
        }
        sess.uid_store(uid.to_string(), "+FLAGS (\\Seen)")
            .map(|_| ())
            .map_err(|e| format!("回写已读失败：{e}"))?;
        let _ = sess.logout();
        Ok(())
    }
}

/// IMAP SINCE 日期（"DD-Mon-YYYY"，epoch 换算）
fn imap_since(days: i64) -> String {
    let cutoff = now_epoch() - days.clamp(1, 365) * 86_400;
    let Some(z) = zoned_of(cutoff) else {
        return "01-Jan-2020".into();
    };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(z.month() as usize - 1).min(11)];
    format!("{:02}-{month}-{}", z.day() as u32, z.year())
}

/// MIME 解析为 IncomingMail（mailparse：主题/地址自动解码 RFC2047，spike 已验证）
fn parse_incoming(
    uidvalidity: u32,
    uid: u32,
    seen: bool,
    internal_date: Option<i64>,
    raw: &[u8],
) -> Option<IncomingMail> {
    use mailparse::MailHeaderMap;
    let mail = mailparse::parse_mail(raw).ok()?;
    let headers = mail.get_headers();
    // RFC2047 解码需 get_first_header + get_value（get_first_value 返回原始值不解码）
    let header_value = |key: &str| -> String {
        headers
            .get_first_header(key)
            .map(|h| h.get_value())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let subject = header_value("Subject");
    // From/To：addrparse 解析出地址与显示名
    let (from_addr, from_name) = headers
        .get_first_header("From")
        .and_then(|h| mailparse::addrparse_header(h).ok())
        .and_then(|list| {
            list.extract_single_info().map(|i| {
                (
                    i.addr.to_string(),
                    i.display_name.clone().unwrap_or_default(),
                )
            })
        })
        .unwrap_or_default();
    let to_addrs = headers
        .get_first_header("To")
        .and_then(|h| mailparse::addrparse_header(h).ok())
        .map(|list| {
            let mut out: Vec<String> = Vec::new();
            for a in list.iter() {
                match a {
                    mailparse::MailAddr::Single(s) => out.push(s.addr.clone()),
                    mailparse::MailAddr::Group(g) => {
                        for s in &g.addrs {
                            out.push(s.addr.clone());
                        }
                    }
                }
            }
            out.join(",")
        })
        .unwrap_or_default();
    let message_id = header_value("Message-Id");
    let (body_text, body_kind) = extract_body(&mail);
    let attachments = collect_attachments(&mail);
    Some(IncomingMail {
        uidvalidity,
        uid,
        seen,
        internal_date,
        message_id,
        subject,
        from_addr,
        from_name,
        to_addrs,
        body_text,
        body_kind: body_kind.to_string(),
        attachments,
    })
}

/// cp1252 高位区（0x80-0x9F 的 Unicode 映射）反查表：mojibake 修复用
fn cp1252_high_inverse(c: char) -> Option<u8> {
    Some(match c {
        '\u{20AC}' => 0x80,
        '\u{201A}' => 0x82,
        '\u{0192}' => 0x83,
        '\u{201E}' => 0x84,
        '\u{2026}' => 0x85,
        '\u{2020}' => 0x86,
        '\u{2021}' => 0x87,
        '\u{02C6}' => 0x88,
        '\u{2030}' => 0x89,
        '\u{0160}' => 0x8A,
        '\u{2039}' => 0x8B,
        '\u{0152}' => 0x8C,
        '\u{017D}' => 0x8E,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201C}' => 0x93,
        '\u{201D}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{02DC}' => 0x98,
        '\u{2122}' => 0x99,
        '\u{0161}' => 0x9A,
        '\u{203A}' => 0x9B,
        '\u{0153}' => 0x9C,
        '\u{017E}' => 0x9E,
        '\u{0178}' => 0x9F,
        _ => return None,
    })
}

/// 正文 mojibake 修复（M5 验收）：部分邮件网关把 UTF-8 正文误标为
/// iso-8859-1/windows-1252，mailparse 按声明逐字节解码产生乱码（如"原文"→"åŽŸæ–‡"）。
/// 探测：文本全部字符可逆映射回 cp1252 字节、且逆推字节恰为合法 UTF-8、含 CJK——
/// 判定原文实为 UTF-8，返回重解码结果。正常西文邮件逆推后 UTF-8 解码几乎必失败；
/// 含正常中文的文本提前退出，双重防误伤。
fn fix_mojibake(body: &str) -> Option<String> {
    let mut has_high = false;
    for c in body.chars() {
        let u = c as u32;
        if u >= 0x80 {
            has_high = true;
            if is_cjk(c) {
                return None; // 已含正常中文：解码正确（mojibake 不可能恰含 CJK）
            }
        }
    }
    if !has_high {
        return None; // 纯 ASCII 无需修复
    }
    let mut bytes = Vec::with_capacity(body.len());
    for c in body.chars() {
        let u = c as u32;
        // Latin-1 直通（iso-8859-1 解码路径：0x80-0x9F 为 C1 控制字符直通）
        // + cp1252 特有映射（windows-1252 解码路径：如 U+017D→0x8E）——两条路径都可逆
        let b = if u <= 0xFF {
            u as u8
        } else {
            cp1252_high_inverse(c)?
        };
        bytes.push(b);
    }
    let fixed = String::from_utf8(bytes).ok()?;
    (fixed != body && fixed.chars().any(is_cjk)).then_some(fixed)
}

/// 未声明/错误声明 charset 的 GBK 正文兜底：按声明解码丢字（U+FFFD）或落成无 CJK
/// 高位乱码时，取原始字节（已解传输编码）按 GBK 重解码；无错且含中文才采纳
/// （正常西文邮件 GBK 解码必然报错，不会误伤）
fn fix_gbk_fallback(part: &mailparse::ParsedMail, decoded: &str) -> String {
    let suspect = decoded.contains('\u{FFFD}')
        || (!decoded.chars().any(is_cjk) && decoded.chars().any(|c| (c as u32) >= 0x80));
    if !suspect {
        return decoded.to_string();
    }
    let Ok(raw) = part.get_body_raw() else {
        return decoded.to_string();
    };
    // 原始字节本就是合法 UTF-8：丢字另有原因（如 us-ascii 声明截断），不猜测
    if std::str::from_utf8(&raw).is_ok() {
        return decoded.to_string();
    }
    let (cow, _, had_errors) = encoding_rs::GBK.decode(&raw);
    if !had_errors && cow.chars().any(is_cjk) {
        cow.into_owned()
    } else {
        decoded.to_string()
    }
}

/// 取正文文本并修复编码问题：mojibake（UTF-8 被误标 latin1/1252）优先，GBK 兜底其次
fn body_text_repaired(part: &mailparse::ParsedMail) -> String {
    let decoded = part.get_body().unwrap_or_default();
    if let Some(fixed) = fix_mojibake(&decoded) {
        return fixed;
    }
    fix_gbk_fallback(part, &decoded)
}

/// 正文抽取（§6.1）：multipart 递归，text/plain 优先；仅 text/html 时剥标签存纯文本；
/// 编码修复（M5）：mis-declared charset 修复 + GBK 兜底
fn extract_body(mail: &mailparse::ParsedMail) -> (String, &'static str) {
    if let Some(p) = find_inline_part(mail, "text/plain") {
        return (body_text_repaired(p), "plain");
    }
    if let Some(p) = find_inline_part(mail, "text/html") {
        return (strip_html(&body_text_repaired(p)), "html");
    }
    (String::new(), "plain")
}

fn find_inline_part<'a>(
    mail: &'a mailparse::ParsedMail<'a>,
    mime: &str,
) -> Option<&'a mailparse::ParsedMail<'a>> {
    if mail.ctype.mimetype == mime
        && mail.get_content_disposition().disposition != mailparse::DispositionType::Attachment
    {
        return Some(mail);
    }
    for sub in &mail.subparts {
        if let Some(p) = find_inline_part(sub, mime) {
            return Some(p);
        }
    }
    None
}

/// HTML 剥标签（无 regex 依赖的手工实现）+ 基础实体解码。
/// 索引扫描而非吞并式迭代：未闭合 `<` 不应吞掉后文（畸形 HTML 常见）
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        match html.as_bytes()[i] {
            b'<' => match html[i..].find('>') {
                Some(rel) => i += rel + 1, // 整段标签丢弃
                None => {
                    // 未闭合：保留 '<' 与后文（不解析）
                    out.push_str(&html[i..]);
                    break;
                }
            },
            b'&' => {
                // 实体：扫描到 ';'（最多 8 字符）；未闭合则原样输出
                let rest = &html[i + 1..];
                match rest.find(';').filter(|pos| *pos <= 8) {
                    Some(pos) => {
                        match &rest[..pos] {
                            "nbsp" => out.push(' '),
                            "lt" => out.push('<'),
                            "gt" => out.push('>'),
                            "amp" => out.push('&'),
                            "quot" => out.push('"'),
                            other => out.push_str(other),
                        }
                        i += 1 + pos + 1;
                    }
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            _ => {
                let ch = html[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// 附件收集（§6.1：Content-Disposition attachment 或带 filename 的 part）
fn collect_attachments(mail: &mailparse::ParsedMail) -> Vec<IncomingAttach> {
    let mut out = Vec::new();
    walk_attachments(mail, &mut out);
    out
}

fn walk_attachments(mail: &mailparse::ParsedMail, out: &mut Vec<IncomingAttach>) {
    let cd = mail.get_content_disposition();
    let named = cd.params.contains_key("filename") || mail.ctype.params.contains_key("name");
    if cd.disposition == mailparse::DispositionType::Attachment || named {
        let filename = cd
            .params
            .get("filename")
            .or_else(|| mail.ctype.params.get("name"))
            .cloned()
            .unwrap_or_else(|| "attachment".into());
        if let Ok(data) = mail.get_body_raw() {
            out.push(IncomingAttach {
                filename,
                mime: mail.ctype.mimetype.clone(),
                data,
            });
        }
        return; // 附件 part 不再递归
    }
    for sub in &mail.subparts {
        walk_attachments(sub, out);
    }
}

/// 收件入库（M4 §6.1）：remote_uid（UNIQUE）+ message_id 双重去重；分词影子列；
/// unread 按 \Seen；附件走两级存储。返回新增封数。
/// （生产走收件线程的 conn 版；本 AppState 薄壳服务于单测与同步调用）
#[cfg_attr(not(test), allow(dead_code))]
pub fn store_incoming(state: &AppState, mails: &[IncomingMail]) -> Result<usize, String> {
    let data_dir = state.data_dir.clone();
    state.with_conn(|conn| store_incoming_conn(conn, &data_dir, mails))
}

/// 收件入库核心（独立连接版：收件线程使用，不依赖 AppState）。
/// 每封邮件一个事务：正文行 + 附件行原子提交（避免"正文已入库、附件缺失"的半状态）；
/// 单封失败回滚该封并中止整批（已提交的前序邮件保留，与 §6.1 取舍一致）。
pub fn store_incoming_conn(
    conn: &mut rusqlite::Connection,
    data_dir: &Path,
    mails: &[IncomingMail],
) -> Result<usize, String> {
    let root = attach_root_of(data_dir);
    let limit = inline_limit_conn(conn);
    let mut added = 0usize;
    for m in mails {
        let remote = format!("{}:{}", m.uidvalidity, m.uid);
        // 去重：remote_uid 唯一索引 + message_id 双保险（为空串时不按 message_id 去重）
        let dup: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM emails WHERE remote_uid = ?1)
                     OR (?2 <> '' AND EXISTS(SELECT 1 FROM emails WHERE message_id = ?2))",
                rusqlite::params![remote, m.message_id],
                |r| r.get(0),
            )
            .map_err(e)?;
        if dup {
            continue;
        }
        let st = tokenize(&m.subject);
        let bt = tokenize(&m.body_text);
        let now = now_epoch();
        let attach_bytes: i64 = m.attachments.iter().map(|a| a.data.len() as i64).sum();
        let unread = !m.seen;

        let tx = conn.transaction().map_err(e)?;
        tx.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, body_kind, subject_tokens, body_tokens,
                                attach_cnt, attach_bytes, unread, remote_uid, message_id,
                                created_at, received_at)
             VALUES ('in', 'inbox', 'received', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                     ?11, ?12, ?13, ?14, ?15)",
            rusqlite::params![
                m.from_addr,
                m.from_name,
                m.to_addrs,
                m.subject,
                m.body_text,
                m.body_kind,
                st,
                bt,
                m.attachments.len() as i64,
                attach_bytes,
                unread,
                remote,
                m.message_id,
                now,
                m.internal_date.unwrap_or(now),
            ],
        )
        .map_err(e)?;
        let id = tx.last_insert_rowid();
        let month = month_dir_conn(&tx, id);
        let mut written: Vec<PathBuf> = Vec::new();
        let stored = (|| -> Result<(), String> {
            for a in &m.attachments {
                insert_attachment(
                    &AttachCtx {
                        tx: &tx,
                        root: &root,
                        month: &month,
                        email_id: id,
                        limit,
                    },
                    &safe_filename(&a.filename),
                    &a.mime,
                    &a.data,
                    &mut written,
                )?;
            }
            sync_attach_stats(&tx, id)
        })();
        match stored {
            Ok(()) => {
                tx.commit().map_err(e)?;
                added += 1;
            }
            Err(err) => {
                // 事务 drop 即回滚；清理本封已写盘的独立附件文件
                drop(tx);
                for f in &written {
                    let _ = std::fs::remove_file(f);
                }
                return Err(err);
            }
        }
    }
    Ok(added)
}

/// 本地已读 + 服务器 \Seen 回写（§6.1）：
/// 参数快照在调用线程（协议回调内，纯本地查询）完成后，
/// 网络回写交由调用方 spawn 的线程执行（fetcher 不依赖 AppState）。
/// 本地已读 + 服务器 \Seen 回写（§6.1）：同步语义版（单测验证回写参数）；
/// 生产路径见路由 mark_read_and_sync_seen（参数快照 + 异步 spawn）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn mark_seen(state: &AppState, fetcher: &dyn MailFetcher, id: i64) -> Result<(), String> {
    // 本地置已读（幂等；无 remote_uid 时仅本地）
    mark_read(state, id);
    let Some(p) = seen_params_of(state, id) else {
        return Ok(()); // 本地邮件（草稿/已发送）或凭据缺失：仅本地已读
    };
    fetcher.mark_seen(
        &p.host,
        p.port,
        &p.account,
        &p.password,
        p.uidvalidity,
        p.uid,
    )
}

/// 已读回写的参数快照（无网络操作；路由层取快照后可自由移交线程）
pub struct SeenParams {
    pub host: String,
    pub port: u16,
    pub account: String,
    pub password: String,
    pub uidvalidity: u32,
    pub uid: u32,
}

/// 取回写参数（本地已读由 mark_read 负责；此处读 remote/服务器参数/凭据）。
/// 无 remote_uid（本地草稿/out 已发送）或凭据缺失返回 None（静默跳过回写）。
pub fn seen_params_of(state: &AppState, id: i64) -> Option<SeenParams> {
    // 单层 ?：闭包返回 Option<String>（None = 非 in 邮件/已删除），None 即跳过回写
    let remote = state.with_conn(|conn| {
        conn.query_row(
            "SELECT remote_uid FROM emails WHERE id = ?1 AND direction = 'in'",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .unwrap_or(None)
    })?;
    let (v, u) = remote.split_once(':')?;
    let uidvalidity = v.parse().ok()?;
    let uid = u.parse().ok()?;
    let d = defaults();
    let host = state.get_or(keys::RECV_HOST, &d).unwrap_or_default();
    let port: u16 = state
        .get_or(keys::RECV_PORT, &d)
        .and_then(|p| p.parse().ok())
        .unwrap_or(993);
    let prefix = state.get_or(keys::ACCOUNT_PREFIX, &d).unwrap_or_default();
    let domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    let account = full_address(&prefix, &domain);
    let password = match state.retrieve_password(&account) {
        crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => p,
        crate::secrets::GetResult::Missing => return None,
    };
    Some(SeenParams {
        host,
        port,
        account,
        password,
        uidvalidity,
        uid,
    })
}

#[cfg(test)]
#[path = "../tests/mail_recv.rs"]
mod tests;
