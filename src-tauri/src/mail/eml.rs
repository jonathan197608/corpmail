//! .eml 导出与转发为附件（B4/B6）：从库内已解析字段重建 RFC 5322 消息。
//!
//! 收件只保存解析结果与原始信头，不保存原始报文，因此导出时按标准结构重建：
//! - 头：From/To/Cc/Subject（RFC 2047）/Date（RFC 5322）/Message-ID/MIME-Version
//! - 正文：有 HTML → multipart/alternative（plain + html）；否则 text/plain
//! - 附件：multipart/mixed 包裹正文部分 + 每个附件（base64，76 字符折行）
//! - 换行统一 CRLF；正文/附件一律 base64，避免编码与行宽问题
//!
//! 生成结果可被主流客户端打开；B6「作为附件转发」直接以 message/rfc822 附加。

use crate::state::AppState;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// 头部值清洗复用 crate 级实现（与 SMTP 发信共用同一口径）
use super::head_safe;

/// RFC 2047 B 编码：纯 ASCII 原样返回（控制字符已清洗）；非 ASCII 按字符累积分段
/// （≈45 字节），段间用 CRLF + 空格折行，避免长主题头超 76 字符
fn rfc2047(s: &str) -> String {
    let s = head_safe(s);
    if s.is_ascii() {
        return s;
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        cur.push(ch);
        if cur.len() >= 45 {
            out.push(format!("=?UTF-8?B?{}?=", B64.encode(cur.as_bytes())));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(format!("=?UTF-8?B?{}?=", B64.encode(cur.as_bytes())));
    }
    out.join("\r\n ")
}

/// 简单地址头：`Name <addr>`（姓名按 RFC 2047 编码；两者都过控制字符清洗）
fn addr_header(name: &str, addr: &str) -> String {
    let name = head_safe(name.trim());
    let addr = head_safe(addr.trim());
    if name.is_empty() {
        return addr;
    }
    format!("{} <{}>", rfc2047(&name), addr)
}

/// 逗号分隔地址串 → 地址头（姓名未知，只拼纯地址；过滤空段；控制字符清洗）
fn addr_list_header(raw: &str) -> String {
    head_safe(raw)
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// base64 按 76 字符折行**直接追加**到目标缓冲（RFC 2045 行宽上限）。
///
/// 旧实现 `B64.encode` 生成整串 → `chunks(76).join` 再拼一份 → `format!` 又拷
/// 一份 → 最终缓冲再拷一份，单个大附件要同时占 4 份内存（审计 R61）。
/// 57 字节输入恰好编码成 76 字符且不留 padding，按 57 字节切块每行独立合法
fn push_b64_wrapped(out: &mut Vec<u8>, data: &[u8]) {
    const IN_CHUNK: usize = 57;
    let mut buf = [0u8; 76];
    for (i, chunk) in data.chunks(IN_CHUNK).enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        // 块大小固定 → 输出必为 76 字符，缓冲区刚好够；真失败就跳过该行
        // 而不是 panic（编码器无状态，不会影响后续行）
        if let Ok(n) = B64.encode_slice(chunk, &mut buf) {
            out.extend_from_slice(&buf[..n]);
        }
    }
}

/// 文本正文部分（base64 传输）
fn push_text_part(out: &mut Vec<u8>, mime: &str, content: &str) {
    out.extend_from_slice(
        format!(
            "Content-Type: {mime}; charset=utf-8\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n"
        )
        .as_bytes(),
    );
    push_b64_wrapped(out, content.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// 附件参数值的 ASCII 回退：非 ASCII/引号/反斜杠替换为 `_`，供老客户端读取
fn ascii_name(name: &str) -> String {
    let out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' && !c.is_control() {
                c
            } else {
                '_'
            }
        })
        .collect();
    let out = out.trim();
    if out.is_empty() {
        "attachment".to_string()
    } else {
        out.to_string()
    }
}

/// RFC 2231 扩展参数（仅非 ASCII 需要）：UTF-8 + 百分号编码，保留 attr-char
fn rfc2231_value(name: &str) -> Option<String> {
    if name.is_ascii() {
        return None;
    }
    let mut out = String::new();
    for b in name.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || "-._~".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Some(format!("UTF-8''{out}"))
}

/// 附件部分（base64 传输）。中文文件名用 RFC 2231 的 `filename*=`/`name*=`，
/// 同时给出 ASCII 回退值：老客户端读回退名，新客户端还原中文，避免整串乱码。
fn push_attach_part(out: &mut Vec<u8>, boundary: &str, filename: &str, mime: &str, data: &[u8]) {
    let fallback = ascii_name(filename);
    let (name_param, file_param) = match rfc2231_value(filename) {
        Some(enc) => (
            format!("name=\"{fallback}\"; name*={enc}"),
            format!("filename=\"{fallback}\"; filename*={enc}"),
        ),
        None => (
            format!("name=\"{fallback}\""),
            format!("filename=\"{fallback}\""),
        ),
    };
    out.extend_from_slice(
        format!(
            "--{boundary}\r\n\
             Content-Type: {mime}; {name_param}\r\n\
             Content-Transfer-Encoding: base64\r\n\
             Content-Disposition: attachment; {file_param}\r\n\r\n"
        )
        .as_bytes(),
    );
    push_b64_wrapped(out, data);
    out.extend_from_slice(b"\r\n");
}

/// 导出文件名：主题截断 60 字符，去掉路径分隔符与控制字符；空主题用 id
pub fn suggested_filename(subject: &str, id: i64) -> String {
    let cleaned: String = subject
        .chars()
        .take(60)
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        format!("corpmail-{id}.eml")
    } else {
        format!("{cleaned}.eml")
    }
}

/// 正文部分（有 HTML → multipart/alternative 双版本；否则纯文本）
fn push_body(out: &mut Vec<u8>, detail: &super::EmailDetail, email_id: i64) {
    if detail.body_html.trim().is_empty() {
        push_text_part(out, "text/plain", &detail.body_text);
        return;
    }
    let inner = format!("=_corpmail_alt_{email_id}");
    out.extend_from_slice(
        format!("Content-Type: multipart/alternative; boundary=\"{inner}\"\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(format!("--{inner}\r\n").as_bytes());
    push_text_part(out, "text/plain", &detail.body_text);
    out.extend_from_slice(format!("--{inner}\r\n").as_bytes());
    push_text_part(out, "text/html", &detail.body_html);
    out.extend_from_slice(format!("--{inner}--\r\n").as_bytes());
}

/// 重建一封邮件的 .eml 字节（B4 导出 / B6 作为附件转发的共同实现）。
///
/// 全程流式写一个 `Vec<u8>`（审计 R61）：附件逐个读、逐个编码、写完即丢弃，
/// 峰值内存 ≈ 成品大小 + 单个最大附件，而不是"全部附件 + 4 倍 base64 拷贝"。
/// 附件结构必须先定，故先用 `attachment_available` 做一次只查元数据/存在性的
/// 预检（不解压不读字节）
pub fn build_eml(state: &AppState, email_id: i64) -> Result<Vec<u8>, String> {
    let detail = crate::mail::get(state, email_id).ok_or("邮件不存在".to_string())?;
    // 时间与 Message-ID 需单独取（EmailDetail 只带展示文本）
    let (epoch, message_id): (i64, String) = state
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COALESCE(received_at, sent_at, created_at), COALESCE(message_id, '')
                 FROM emails WHERE id = ?1",
                [email_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .map_err(|e| format!("读取邮件信息失败：{e}"))?;
    let date = super::zoned_of(epoch)
        .map(|z| z.strftime("%a, %d %b %Y %H:%M:%S %z").to_string())
        .unwrap_or_else(|| "Thu, 01 Jan 1970 00:00:00 +0000".to_string());
    let message_id = if message_id.trim().is_empty() {
        format!("<corpmail-{email_id}@localhost>")
    } else {
        // Message-Id 来自收件头解析，可能夹带控制字符——清洗后再拼头
        head_safe(message_id.trim())
    };

    // 可导出的附件清单：detached/文件缺失的跳过（导出不因一个已清理附件整体失败）
    let parts: Vec<(String, String, i64)> = detail
        .attachments
        .iter()
        .filter(|a| crate::mail::attachment_available(state, email_id, a.id))
        .map(|a| (a.filename.clone(), a.mime.clone(), a.id))
        .collect();

    let boundary = format!("=_corpmail_{email_id}_{:x}", super::now_epoch());
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(
        format!(
            "From: {}\r\n",
            addr_header(&detail.from_name, &detail.from_addr)
        )
        .as_bytes(),
    );
    if !detail.to_addrs.trim().is_empty() {
        out.extend_from_slice(format!("To: {}\r\n", addr_list_header(&detail.to_addrs)).as_bytes());
    }
    if !detail.cc_addrs.trim().is_empty() {
        out.extend_from_slice(format!("Cc: {}\r\n", addr_list_header(&detail.cc_addrs)).as_bytes());
    }
    out.extend_from_slice(format!("Subject: {}\r\n", rfc2047(&detail.subject)).as_bytes());
    out.extend_from_slice(format!("Date: {date}\r\n").as_bytes());
    out.extend_from_slice(format!("Message-ID: {message_id}\r\n").as_bytes());
    out.extend_from_slice(b"MIME-Version: 1.0\r\n");

    if parts.is_empty() {
        push_body(&mut out, &detail, email_id);
        return Ok(out);
    }
    out.extend_from_slice(
        format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n")
            .as_bytes(),
    );
    push_body(&mut out, &detail, email_id);
    for (filename, mime, attach_id) in &parts {
        // 读取失败（预检后被删/被占用）只跳过该附件：正文与其余附件照常导出
        let Some((_, bytes)) = crate::mail::attachment_bytes(state, email_id, *attach_id) else {
            eprintln!("[eml] 附件「{filename}」读取失败，导出时跳过");
            continue;
        };
        let mime = if mime.trim().is_empty() {
            "application/octet-stream"
        } else {
            mime.as_str()
        };
        push_attach_part(&mut out, &boundary, filename, mime, &bytes);
        // bytes 在此丢弃：下一个附件重新读，不累积整封的附件字节
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(out)
}
