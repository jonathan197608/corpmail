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

/// 头部值清洗：控制字符（含 CR/LF）替换为空格。库内字段来自收件解析或用户
/// 输入，若夹带 CRLF 会被拼进头部产生头注入（伪造 From/To/X-* 等）；
/// RFC 5322 头值域本就不允许裸控制字符，清洗无信息损失。
fn head_safe(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

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

/// base64 按 76 字符折行（RFC 2045 行宽上限）
fn wrap_b64(data: &[u8]) -> String {
    let encoded = B64.encode(data);
    encoded
        .as_bytes()
        .chunks(76)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// 文本正文部分（base64 传输）
fn text_part(mime: &str, content: &str) -> String {
    format!(
        "Content-Type: {mime}; charset=utf-8\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{}\r\n",
        wrap_b64(content.as_bytes())
    )
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
fn attach_part(boundary: &str, filename: &str, mime: &str, data: &[u8]) -> String {
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
    format!(
        "--{boundary}\r\n\
         Content-Type: {mime}; {name_param}\r\n\
         Content-Transfer-Encoding: base64\r\n\
         Content-Disposition: attachment; {file_param}\r\n\r\n\
         {body}\r\n",
        body = wrap_b64(data),
    )
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

/// 重建一封邮件的 .eml 字节（B4 导出 / B6 作为附件转发的共同实现）
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

    // 附件字节（detached/缺失的跳过：导出不因一个已清理附件整体失败）
    let mut attachments: Vec<(String, String, Vec<u8>)> = Vec::new();
    for a in &detail.attachments {
        if let Some((view, bytes)) = crate::mail::attachment_bytes(state, email_id, a.id) {
            attachments.push((view.filename, view.mime, bytes));
        }
    }

    let boundary = format!("=_corpmail_{email_id}_{:x}", super::now_epoch());
    let mut out = String::new();
    out.push_str(&format!(
        "From: {}\r\n",
        addr_header(&detail.from_name, &detail.from_addr)
    ));
    if !detail.to_addrs.trim().is_empty() {
        out.push_str(&format!("To: {}\r\n", addr_list_header(&detail.to_addrs)));
    }
    if !detail.cc_addrs.trim().is_empty() {
        out.push_str(&format!("Cc: {}\r\n", addr_list_header(&detail.cc_addrs)));
    }
    out.push_str(&format!("Subject: {}\r\n", rfc2047(&detail.subject)));
    out.push_str(&format!("Date: {date}\r\n"));
    out.push_str(&format!("Message-ID: {message_id}\r\n"));
    out.push_str("MIME-Version: 1.0\r\n");

    let has_html = !detail.body_html.trim().is_empty();
    let body = if has_html {
        let inner = format!("=_corpmail_alt_{email_id}");
        format!(
            "Content-Type: multipart/alternative; boundary=\"{inner}\"\r\n\r\n\
             --{inner}\r\n{}\
             --{inner}\r\n{}\
             --{inner}--\r\n",
            text_part("text/plain", &detail.body_text),
            text_part("text/html", &detail.body_html),
        )
    } else {
        text_part("text/plain", &detail.body_text)
    };

    if attachments.is_empty() {
        out.push_str(&body);
    } else {
        out.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n\
             --{boundary}\r\n{body}"
        ));
        for (filename, mime, bytes) in &attachments {
            out.push_str(&attach_part(
                &boundary,
                filename,
                if mime.trim().is_empty() {
                    "application/octet-stream"
                } else {
                    mime
                },
                bytes,
            ));
        }
        out.push_str(&format!("--{boundary}--\r\n"));
    }
    Ok(out.into_bytes())
}
