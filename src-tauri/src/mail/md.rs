//! 邮件导出 Markdown（供 AI 分析处理）。
//!
//! 目标：把一封邮件转成**结构化、无噪音**的 Markdown——元数据进 YAML
//! front matter（AI 与文档工具都按头字段解析），正文转 MD 文本（HTML 邮件
//! 按块级结构重建段落/列表/表格的文本形态），附件列清单。
//!
//! - front matter 值用 `serde_json::to_string` 转义：JSON 字符串是合法 YAML
//!   （YAML 1.2 是 JSON 超集），主题/姓名中的换行、引号、冒号等任意字符
//!   都不会破坏结构，也无需手写 YAML 转义规则
//! - HTML→MD 是结构化转换（块级换行、li 前缀、h* 标题、表格转标准 GFM
//!   表格）：目的是让 AI 读到连贯分段与可解析的表格结构，不追求像素级
//!   等价原邮件版式
//! - 纯文本邮件原样导出；超大占位邮件按占位说明导出（提示未下载）

use crate::state::AppState;

/// 导出 .md 时可直接内嵌的附件大小上限（字节）：短文本附件直接进正文代码块，
/// AI 单次可读、MD 不致膨胀；超过则改为输出本地文件路径
const MD_EMBED_ATTACH_MAX: i64 = 64 * 1024;

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
    if !detail.attachments.is_empty() {
        md.push_str("\n## 附件\n\n");
        for a in &detail.attachments {
            write_attachment_section(state, email_id, a, &mut md);
        }
    }
    Ok(md)
}

/// 单附件小节：短文本附件直接内嵌内容（fenced 代码块）；大附件/二进制
/// 输出本地完整路径供后续处理；已清理的附件说明不可用
fn write_attachment_section(
    state: &AppState,
    email_id: i64,
    a: &crate::mail::AttachView,
    md: &mut String,
) {
    md.push_str(&format!(
        "### {}（{}）\n\n",
        a.filename,
        crate::templates::fmt_size(a.size_orig)
    ));
    if a.detached {
        md.push_str("文件已按清理策略移除，无法导出。\n\n");
        return;
    }
    if let Some(lang) = text_lang_of(&a.mime, &a.filename)
        && a.size_orig <= MD_EMBED_ATTACH_MAX
        && let Some((_, bytes)) = crate::mail::attachment_bytes(state, email_id, a.id)
    {
        // 短文本：内容直接进代码块（AI 单次可读）；内容含 ``` 时用 4 反引号防破坏
        let content = String::from_utf8_lossy(&bytes);
        let fence = if content.contains("```") {
            "````"
        } else {
            "```"
        };
        md.push_str(&format!("{fence}{lang}\n{content}\n{fence}\n\n"));
        return;
    }
    // 大附件/二进制：本地完整路径（storage=file 为磁盘真实文件，gzip 压缩存储；
    // inline BLOB 存于数据库无独立文件，如实说明）
    let location = state
        .with_conn(|conn| {
            conn.query_row(
                "SELECT storage, COALESCE(file_path, '') FROM attachments WHERE id = ?1",
                [a.id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
        })
        .ok();
    match location.as_ref().map(|(s, p)| (s.as_str(), p.as_str())) {
        Some(("file", p)) if !p.is_empty() => md.push_str(&format!(
            "本地文件：{}（gzip 压缩存储）\n\n",
            state.data_dir().join("attachments").join(p).display()
        )),
        _ => md.push_str(&format!(
            "附件内容存于本地数据库（无独立文件，{}）\n\n",
            crate::templates::fmt_size(a.size_orig)
        )),
    }
}

/// 文本类附件判定（返回 fenced 代码块语言标注；None = 二进制不适合内嵌）。
/// MIME 优先；octet-stream/缺失时按扩展名兜底（真实邮件附件头常不规范）
fn text_lang_of(mime: &str, filename: &str) -> Option<&'static str> {
    let m = mime.trim().to_ascii_lowercase();
    if let Some(lang) = match m.as_str() {
        "application/json" => Some("json"),
        "application/xml" => Some("xml"),
        _ if m.starts_with("text/") => Some("text"),
        _ => None,
    } {
        return Some(lang);
    }
    if !m.is_empty() && m != "application/octet-stream" {
        return None; // 其他显式 MIME（图片/PDF/zip 系）不靠扩展名猜
    }
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "txt" | "log" | "ini" | "conf" | "env" => "text",
        "md" | "markdown" => "markdown",
        "csv" | "tsv" => "csv",
        "json" => "json",
        "xml" => "xml",
        "yml" | "yaml" => "yaml",
        "html" | "htm" => "html",
        "js" => "javascript",
        "py" => "python",
        "rs" => "rust",
        "sql" => "sql",
        "sh" | "bat" => "bash",
        _ => return None,
    })
}

/// HTML → Markdown 文本（纯函数，无 I/O）：
/// - 块级标签（p/div/tr/li/h1..h6/blockquote）结束补换行，段落不粘连
/// - `<li>` 开标签转 "- " 列表前缀；h1..h6 转对应层级 "#"（正文从 ## 起步）
/// - 表格转**标准 GFM 表格**：首行作表头 + `| --- |` 分隔行 + 逐行
///   `| a | b |`；列数不齐按最大列补齐；单元格内 `|` 转义为 `\|`、换行折叠
///   为空格；th 与 td 同等对待（GFM 表头固定取首行，无 thead 的表也按此
///   行业惯例降级）；嵌套表格退化为单元格文本（不破坏外层结构）
/// - `<br>` 换行；内联标签剥除；实体解码复用 strip_html 口径
/// - 未闭合 `<` 不吞后文；连续空行压缩为一段
pub fn html_to_md_text(html: &str) -> String {
    let mut b = MdBuilder::default();
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
                    b.on_tag(name, closing);
                    i += rel + 1;
                }
                None => {
                    // 未闭合：余下按原文保留（畸形 HTML 不吞正文）
                    let rest = &html[i..];
                    b.text_target().push_str(rest);
                    break;
                }
            },
            b'&' => {
                // i 为独立局部，target 从构建器借出（表格内落单元格）
                let target = b.text_target();
                super::decode_entity(html, &mut i, target);
            }
            _ => {
                let ch = html[i..].chars().next().unwrap_or(' ');
                b.text_target().push(ch);
                i += ch.len_utf8();
            }
        }
    }
    b.finish()
}

/// MD 构建器：普通内容写 out；进入表格后文本改写入当前单元格，
/// `</table>` 时一次性生成 GFM 表格（表头分隔行由结构保证存在）
#[derive(Default)]
struct MdBuilder {
    out: String,
    table: Option<TableBuf>,
}

/// 表格收集缓冲：已完成行 / 当前行单元格 / 当前单元格文本 / 嵌套深度
#[derive(Default)]
struct TableBuf {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    /// 嵌套 table 深度（内层结构忽略，文字进当前单元格；外层正常收尾）
    depth: u32,
}

impl MdBuilder {
    /// 当前文本落点：表格内的单元格 or 普通输出
    fn text_target(&mut self) -> &mut String {
        match &mut self.table {
            Some(t) => &mut t.cell,
            None => &mut self.out,
        }
    }

    fn on_tag(&mut self, name: &str, closing: bool) {
        // 表格边界：最外层进入/退出收集模式
        if name == "table" {
            match &mut self.table {
                None => {
                    self.table = if closing {
                        None
                    } else {
                        Some(TableBuf::default())
                    }
                }
                Some(t) => {
                    if closing {
                        if t.depth == 0 {
                            self.end_table();
                        } else {
                            t.depth -= 1;
                        }
                    } else {
                        t.depth += 1;
                    }
                }
            }
            return;
        }
        if self.table.is_some() {
            self.on_table_tag(name, closing);
            return;
        }
        apply_tag(&mut self.out, name, closing);
    }

    /// 表格内标签：结构标签归集行/单元格；内联标签剥除；br 折叠为空格
    fn on_table_tag(&mut self, name: &str, closing: bool) {
        let Some(t) = &mut self.table else { return };
        // 嵌套表格内层：所有结构标签忽略（内层文字自然留在当前单元格，
        // 尤其不能因内层 `<td>` 开标签清掉外层已收集的文本）
        if t.depth > 0 {
            return;
        }
        match (name, closing) {
            // 单元格开始：清空残留文本（如 caption，表格转换中不保留）
            ("td" | "th", false) => t.cell.clear(),
            // 单元格结束：原始文本收进当前行（清洗留到生成时统一做）
            ("td" | "th", true) => {
                let cell = std::mem::take(&mut t.cell);
                t.row.push(cell);
            }
            // 行结束：收进表格
            ("tr", true) => {
                if !t.row.is_empty() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                    t.cell.clear();
                }
            }
            // 表格结构标签自身不产生文本
            ("tr" | "thead" | "tbody" | "tfoot" | "colgroup" | "col" | "caption", _) => {}
            // 单元格内换行一律折叠为空格（MD 表格单元格不能跨行）
            ("br", false) => t.cell.push(' '),
            // 单元格内块级标签以空格分隔，避免文字粘连（嵌套 p/div/li）
            ("p" | "div" | "li" | "blockquote", _) => t.cell.push(' '),
            _ => {}
        }
    }

    /// 生成标准 GFM 表格：首行作表头 + 分隔行 + 数据行；列数不齐按最大列补齐
    fn end_table(&mut self) {
        let Some(t) = self.table.take() else { return };
        if t.rows.is_empty() {
            return;
        }
        let cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        ensure_blank(&mut self.out);
        write_table_row(&mut self.out, &t.rows[0], cols);
        // 表头分隔行（GFM 表格必需）
        self.out.push('|');
        for _ in 0..cols {
            self.out.push_str(" --- |");
        }
        self.out.push('\n');
        for row in &t.rows[1..] {
            write_table_row(&mut self.out, row, cols);
        }
    }

    /// 收尾：未闭合表格尽力输出（畸形 HTML），再压缩空行
    fn finish(mut self) -> String {
        if self.table.is_some() {
            self.end_table();
        }
        collapse_blank_lines(&self.out)
    }
}

/// 输出一行 MD 表格：`| a | b |`（缺列补空单元格）
fn write_table_row(out: &mut String, row: &[String], cols: usize) {
    out.push('|');
    for c in 0..cols {
        out.push(' ');
        if let Some(raw) = row.get(c) {
            out.push_str(&clean_cell(raw));
        }
        out.push_str(" |");
    }
    out.push('\n');
}

/// 单元格内容清洗：`|` 转义为 `\|`（不破坏表格语法）；换行/制表/连续空白
/// 折叠为单空格；首尾去空白
fn clean_cell(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        match ch {
            '|' => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push_str("\\|");
            }
            c if c.is_whitespace() => pending_space = !out.is_empty(),
            c => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push(c);
            }
        }
    }
    out
}

/// 单个标签的 MD 结构映射（表格由 MdBuilder 单独处理，不经此处）：
/// 块级开标签前确保空行（段落/标题/列表与前文空行分隔），
/// li/tr 开标签前确保单换行，闭标签统一补换行。
fn apply_tag(out: &mut String, name: &str, closing: bool) {
    if !closing {
        match name {
            "p" | "div" | "blockquote" | "ul" | "ol" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                ensure_blank(out);
            }
            "li" | "tr" => ensure_newline(out),
            _ => {}
        }
    }
    match (name, closing) {
        ("br", false) => out.push('\n'),
        (
            "p" | "div" | "blockquote" | "tr" | "li" | "ul" | "ol" | "h1" | "h2" | "h3" | "h4"
            | "h5" | "h6",
            true,
        ) => out.push('\n'),
        ("li", false) => out.push_str("- "),
        ("h1", false) => out.push_str("## "),
        ("h2", false) => out.push_str("### "),
        ("h3" | "h4" | "h5" | "h6", false) => out.push_str("#### "),
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
