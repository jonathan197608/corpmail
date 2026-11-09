//! 邮件导出 Markdown（供 AI 分析处理）。
//!
//! 目标：把一封邮件转成**结构化、无噪音**的 Markdown——元数据进 YAML
//! front matter（AI 与文档工具都按头字段解析），正文转 MD 文本（HTML 邮件
//! 按块级结构重建段落/列表/表格的文本形态），附件列清单。
//!
//! - front matter 值用 `serde_json::to_string` 转义：JSON 字符串是合法 YAML
//!   （YAML 1.2 是 JSON 超集），主题/姓名中的换行、引号、冒号等任意字符
//!   都不会破坏结构，也无需手写 YAML 转义规则
//! - HTML→MD 是适度转换（块级换行、li 前缀、td 分隔、h* 标题）：目的是
//!   让 AI 读到连贯的分段文本，不追求视觉上等价原邮件版式
//! - 纯文本邮件原样导出；超大占位邮件按占位说明导出（提示未下载）

use crate::state::AppState;

/// 导出 .md 的推荐文件名：与 .eml 同源清洗，仅扩展名不同
pub fn suggested_filename(subject: &str, id: i64) -> String {
    match super::eml::suggested_filename(subject, id).strip_suffix(".eml") {
        Some(base) => format!("{base}.md"),
        None => format!("corpmail-{id}.md"),
    }
}

/// 组装单封邮件的 Markdown 文本。
pub fn build_md(state: &AppState, email_id: i64) -> Result<String, String> {
    let detail = crate::mail::get(state, email_id).ok_or("邮件不存在".to_string())?;
    // JSON 字符串转义 = 合法 YAML 值（见模块头注释）
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());

    let mut md = String::new();
    md.push_str("---\n");
    md.push_str(&format!("subject: {}\n", q(&detail.subject)));
    let from = if detail.from_name.is_empty() {
        detail.from_addr.clone()
    } else {
        format!("{} <{}>", detail.from_name, detail.from_addr)
    };
    md.push_str(&format!("from: {}\n", q(&from)));
    if !detail.to_addrs.trim().is_empty() {
        md.push_str(&format!("to: {}\n", q(&detail.to_addrs)));
    }
    if !detail.cc_addrs.trim().is_empty() {
        md.push_str(&format!("cc: {}\n", q(&detail.cc_addrs)));
    }
    if !detail.bcc_addrs.trim().is_empty() {
        md.push_str(&format!("bcc: {}\n", q(&detail.bcc_addrs)));
    }
    md.push_str(&format!("date: {}\n", q(&detail.time_full)));
    md.push_str(&format!("folder: {}\n", q(&detail.folder)));
    if !detail.attachments.is_empty() {
        md.push_str("attachments:\n");
        for a in &detail.attachments {
            md.push_str(&format!(
                "  - {} ({})\n",
                q(&a.filename),
                crate::templates::fmt_size(a.size_orig)
            ));
        }
    }
    md.push_str("---\n\n");

    md.push_str("## 正文\n\n");
    let body = if detail.body_html.trim().is_empty() {
        detail.body_text.clone()
    } else {
        html_to_md_text(&detail.body_html)
    };
    md.push_str(&body);
    md.push('\n');
    Ok(md)
}

/// HTML → Markdown 文本（纯函数，无 I/O）：
/// - 块级标签（p/div/tr/li/h1..h6/blockquote/table）结束补换行，段落不粘连
/// - `<li>` 开标签转 "- " 列表前缀；h1..h6 开标签转对应层级 "#"（正文标题
///   从 ## 起步，避免与文档级 "## 正文" 冲突）
/// - `td`/`th` 结束补 " | "，表格保持行×列结构（近似 MD 表格的可读形态）
/// - `<br>` 换行；内联标签（span/b 等）剥除；实体解码复用 strip_html 口径
/// - 未闭合 `<` 不吞后文；连续空行压缩为一段
pub fn html_to_md_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => match html[i..].find('>') {
                Some(rel) => {
                    let tag = &html[i + 1..i + rel];
                    let lower = tag.to_ascii_lowercase();
                    let closing = lower.starts_with('/');
                    let name: &str = lower
                        .trim_start_matches('/')
                        .split(|c: char| c.is_whitespace() || c == '>')
                        .next()
                        .unwrap_or("");
                    apply_tag(&mut out, name, closing);
                    i += rel + 1;
                }
                None => {
                    // 未闭合：余下按原文保留（畸形 HTML 不吞正文）
                    out.push_str(&html[i..]);
                    break;
                }
            },
            b'&' => super::decode_entity(html, &mut i, &mut out),
            _ => {
                let ch = html[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    collapse_blank_lines(&out)
}

/// 单个标签的 MD 结构映射（html_to_md_text 的标签分派）：
/// 块级开标签前确保空行（段落/标题/列表与表格整体与前后文空行分隔），
/// li/tr 开标签前确保单换行（列表项/表格行间紧凑），闭标签统一补换行。
fn apply_tag(out: &mut String, name: &str, closing: bool) {
    if !closing {
        match name {
            "p" | "div" | "blockquote" | "ul" | "ol" | "table" | "h1" | "h2" | "h3" | "h4"
            | "h5" | "h6" => ensure_blank(out),
            "li" | "tr" => ensure_newline(out),
            _ => {}
        }
    }
    match (name, closing) {
        ("br", false) => out.push('\n'),
        (
            "p" | "div" | "blockquote" | "tr" | "li" | "ul" | "ol" | "table" | "h1" | "h2" | "h3"
            | "h4" | "h5" | "h6",
            true,
        ) => out.push('\n'),
        ("li", false) => out.push_str("- "),
        ("h1", false) => out.push_str("## "),
        ("h2", false) => out.push_str("### "),
        ("h3" | "h4" | "h5" | "h6", false) => out.push_str("#### "),
        // 单元格结束补分隔；行首/行尾可能多出 "| "，结构可读即可
        ("td" | "th", true) => out.push_str(" | "),
        _ => {}
    }
}

/// 前置换行：out 已有内容且不以换行结尾时补单个 \n
fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// 前置空行：与前文以一个空行分隔（连续换行归一为 \n\n）
fn ensure_blank(out: &mut String) {
    if out.is_empty() || out.ends_with("\n\n") {
        return;
    }
    if out.ends_with('\n') {
        out.push('\n');
    } else {
        out.push_str("\n\n");
    }
}

/// 连续 3+ 换行压缩为 2（一个空行），并去掉首尾空白
fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut newlines = 0u32;
    for ch in s.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(ch);
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
#[path = "../tests/md.rs"]
mod tests;
