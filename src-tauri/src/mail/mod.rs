//! 邮件存储层（M2，设计 §3/§4.2/§5.1/§5.3）
//!
//! - emails/attachments 表业务读写；FTS 一致性由 AI/AD/AU 触发器保证，
//!   写入方职责仅是先分词落影子列（subject_tokens/body_tokens，§4.1）
//! - 附件两级存储（§5.1）：`size_orig ≤ inline_attach_max_mb`（默认 20MB）→ gzip 入库 BLOB；
//!   超限 → gzip 独立文件 `attachments/{YYYY-MM}/{email_id}/{n}_{safe}.gz`（按月分目录）
//! - 删除为硬删无回收站（§5.3 确认项 D）：emails 行（触发器连带 FTS 行、外键级联附件行）
//!   + 独立附件文件（应用层清理，空目录一并回收）
//! - 列表分页 50/页（§8.3）；时间展示 label：今天 HH:MM / 同年 M月D日 / 跨年 YYYY-M-D
//!
//! 子模块：`send`（SMTP 发送队列状态机）、`recv`（IMAP 收件与 MIME 解析入库）。

pub mod contacts;
/// .eml 导出与「作为附件转发」（B4/B6）
pub mod eml;
/// 导出 Markdown（供 AI 分析处理）
pub mod md;
/// Office 附件预览安全预处理与文本兜底（B8）
pub mod office;
mod recv;
pub(crate) mod seen;
mod send;
pub mod templates;

pub use recv::{
    IncomingMail, LastCursor, MailDest, MailFetcher, RealFetcher, seen_params_of,
    store_incoming_conn,
};
// 仅单测使用的入口：生产收件走 fetch_batched + store_incoming_conn（避免为测试放宽生产可见性）
#[cfg(test)]
pub use recv::{FetchOutcome, store_incoming};
pub use send::{
    RealSender, SmtpConfig, cancel_scheduled_send, dispatch_due_scheduled, dispatch_one,
    recover_interrupted_sends, reopen_failed, schedule_send_in,
};

use crate::err as e;
use crate::state::AppState;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::OptionalExtension;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

// ---------- 分词（FTS 索引源，§4.1） ----------

static JIEBA: std::sync::LazyLock<jieba_rs::Jieba> = std::sync::LazyLock::new(jieba_rs::Jieba::new);

/// 停用词（高频虚词，整词全等过滤；"等级"等含单字词不受影响）
const STOPWORDS: &[&str] = &[
    "的", "了", "在", "是", "我", "有", "和", "就", "不", "人", "都", "一", "上", "也", "很", "到",
    "说", "要", "去", "你", "会", "着", "看", "好", "这", "那", "他", "她", "它", "个", "们", "么",
    "什", "吗", "呢", "吧", "啊", "哦", "呀", "嗯", "把", "被", "让", "与", "及", "或", "等", "最",
    "再", "又", "才", "只", "没", "还", "如", "啥", "于", "以", "从", "向", "之", "其", "为", "地",
    "得", "而", "所", "请", "对", "中", "the", "a", "an", "of", "and", "or", "to", "in", "on",
    "for", "is", "are", "be", "with", "by", "as", "at", "from", "this", "that", "it",
];

fn is_ascii_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

/// 检索分词（索引影子列与查询语句同源，§4.1）：
/// - ASCII 连字串（字母数字 + 夹在字母数字间的 `-` `.` `_`）整词保留并小写化，
///   编号/日期/版本号不被切碎；词尾句点、省略号不并入
/// - 其余文本 jieba 精确模式 + 停用词过滤 + ASCII 小写化
/// - 丢弃纯标点/空白词（须含 CJK 或字母数字）
pub fn tokenize_words(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    let mut rest_start = 0; // 当前未结算的非 ASCII 连字串段起点（交 jieba）
    while i < bytes.len() {
        if is_ascii_word_byte(bytes[i]) {
            if i > rest_start {
                words.extend(jieba_words(&text[rest_start..i]));
            }
            let start = i;
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if is_ascii_word_byte(c) {
                    i += 1;
                } else if (c == b'-' || c == b'.' || c == b'_')
                    && i + 1 < bytes.len()
                    && is_ascii_word_byte(bytes[i + 1])
                {
                    i += 2;
                } else {
                    break;
                }
            }
            let word = text[start..i].to_ascii_lowercase();
            if !STOPWORDS.contains(&word.as_str()) {
                words.push(word);
            }
            rest_start = i;
        } else {
            i += 1;
        }
    }
    if rest_start < bytes.len() {
        words.extend(jieba_words(&text[rest_start..]));
    }
    words
}

/// jieba 精确模式分词 + 停用词过滤 + 小写化 + 有效性过滤
fn jieba_words(text: &str) -> Vec<String> {
    JIEBA
        .cut(text, true)
        .iter()
        .map(|t| t.word.to_ascii_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .filter(|w| w.chars().any(|c| is_cjk(c) || c.is_ascii_alphanumeric()))
        .collect()
}

/// jieba 分词结果空格拼接（FTS 影子列写入格式；空文本返回空串，FTS 无 term 亦合法）
pub fn tokenize(text: &str) -> String {
    tokenize_words(text).join(" ")
}

/// 会话视图归组键（M2-2，方案A）：剥离回复/转发前缀链 + ASCII 小写折叠 + 去空白。
/// 同一主题的往复邮件归一组；空主题/剥空 → 空串（归组查询排除空串行，不串组）。
/// 例：`Re: 会议` → `会议`；`回复：明天开会` → `明天开会`；`Re[2]: Budget` → `budget`。
pub fn thread_key_of(subject: &str) -> String {
    let mut cur = subject.trim();
    loop {
        let t = cur.trim();
        match strip_thread_prefix(t) {
            Some(rest) if rest != t => cur = rest,
            _ => break,
        }
    }
    cur.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 尝试剥离单个回复/转发前缀（大小写不敏感），返回剥离后剩余；不匹配返回 None。
/// 支持英文 re/fwd/fw/reply/r/aw（可带序号如 Re[2]、Re2）与中文 回复/答复/转发。
fn strip_thread_prefix(subject: &str) -> Option<&str> {
    let t = subject.trim_start();
    // 词边界：到空白、英文冒号或中文冒号（：）为止
    let end = t
        .find(|c: char| c.is_whitespace() || c == ':' || c == '\u{FF1A}')
        .unwrap_or(t.len());
    let (word, rest) = t.split_at(end);
    // 英文前缀：提取 ASCII 字母段并小写（容忍 Re[2]、Re(2) 等序号后缀）
    let ascii: String = word
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let is_en = matches!(ascii.as_str(), "re" | "fwd" | "fw" | "reply" | "r" | "aw");
    // 中文前缀：word 直接命中（reply/转发等无冒号场景）
    let is_cn = ["回复", "答复", "转发"].iter().any(|k| word.starts_with(k));
    if is_en || is_cn {
        Some(rest.trim_start_matches(|c: char| c == ':' || c == '\u{FF1A}' || c.is_whitespace()))
    } else {
        None
    }
}

/// 当前 epoch 秒
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// HTML 转义（展示层通用；router.rs 复用本实现，避免双份漂移）
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 邮件 HTML 正文净化（XSS 防护）：白名单过滤，移除 script/iframe/object/embed/form、
/// `on*` 事件属性、`javascript:` 等危险 URL；保留常规排版标签、表格与内联样式。
/// 用于收件入库前净化——库内不存可执行内容，详情页可直接安全输出。
pub fn sanitize_html(raw: &str) -> String {
    static SANITIZER: std::sync::LazyLock<ammonia::Builder<'static>> =
        std::sync::LazyLock::new(|| {
            let mut b = ammonia::Builder::default();
            // 邮件常见但默认白名单外的排版标签/属性
            b.add_tags(["font", "center", "colgroup", "col", "strike"]);
            b.add_generic_attributes([
                "style", "align", "valign", "width", "height", "bgcolor", "color",
            ]);
            // 表格边框：企业邮件（Outlook/Foxmail 生成）普遍用 border/cellpadding/
            // cellspacing 属性而非 CSS；不在白名单会被剥掉——收件箱看是"无边框
            // 表格"，转发经编辑器往返后差异更明显。放行 table 系标签的这三个属性
            //（纯视觉历史属性，无安全面；现代 style 边框本就放行）
            b.add_tag_attributes("table", ["border", "cellpadding", "cellspacing", "style"]);
            b.add_tag_attributes("td", ["border", "cellpadding", "cellspacing", "style"]);
            b.add_tag_attributes("th", ["border", "cellpadding", "cellspacing", "style"]);
            b.link_rel(Some("noopener noreferrer"));
            // 外部图片统一改写到本机代理端点（见 ui::router::proxy_image）：
            // WebView 不直接连第三方，避免暴露 Referer/指纹；代理侧再做 SSRF 校验。
            // 改写结果是相对路径，需允许相对 URL（默认 Deny 会把它过滤掉）。
            b.url_relative(ammonia::UrlRelative::PassThrough);
            // 撰写页富文本编辑器插入的本地图片为 data: URI（无网络端点可上传）。
            // ammonia 的 scheme 白名单按整体串放行，故此处放行 "data" 后必须由
            // 下面的 attribute_filter 收紧：仅 img src 且 data:image/* 前缀放行，
            // 其余元素（a href 等）的任何 data: 一律移除（防 data:text/html 等）。
            b.add_url_schemes(["data"]);
            b.attribute_filter(|element, attribute, value| {
                let v = value.trim();
                let lower = v.to_ascii_lowercase();
                match (element, attribute) {
                    // 图片：远程 → 代理改写；内嵌 data:image/* 放行；其余 data: 移除
                    ("img", "src") => {
                        if is_remote_http_url(v) {
                            let enc: String =
                                url::form_urlencoded::byte_serialize(v.as_bytes()).collect();
                            Some(std::borrow::Cow::Owned(format!("/proxy/image?url={enc}")))
                        } else if lower.starts_with("data:image/") {
                            Some(std::borrow::Cow::Borrowed(value))
                        } else if lower.starts_with("data:") {
                            None
                        } else {
                            Some(std::borrow::Cow::Borrowed(value))
                        }
                    }
                    // 链接等：data: 属危险协议（data:text/html 可执行），一律移除
                    (_, "href") if lower.starts_with("data:") => None,
                    // CSS url()：background 等样式内的远程资源与 img src 同口径改写代理
                    //（审计 L3：否则转发外发后跟踪像素仍可加载）
                    (_, "style") if lower.contains("url(") => {
                        Some(std::borrow::Cow::Owned(rewrite_css_urls(value)))
                    }
                    _ => Some(std::borrow::Cow::Borrowed(value)),
                }
            });
            b
        });
    SANITIZER.clean(raw).to_string()
}

/// 是否 http(s) 远程 URL（用于图片代理改写）
fn is_remote_http_url(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// CSS `url(...)` 远程资源改写（审计 L3）：
/// style 属性里的 `url(http://…)`（背景图等）与 img src 同口径改写到本机
/// 代理——应用内不直连第三方（代理侧做 SSRF 校验）；HTML 正文被转发到
/// 外部收件人后为相对路径，外部客户端解析不了 → 远程图不加载，
/// 阻断跟踪像素随转发泄露（此前 img 已改写而 style 未改写，恰是缺口）。
/// `data:image/*` 内嵌保留；相对路径与其余 URL 原样；解析不了的整段原样。
fn rewrite_css_urls(css: &str) -> String {
    let chars: Vec<char> = css.chars().collect();
    // 是否在 i 处出现 url( （关键字大小写不敏感，'(' 无大小写）
    let eq_ci = |idx: usize, want: char| {
        chars
            .get(idx)
            .is_some_and(|c| c.eq_ignore_ascii_case(&want))
    };
    let is_url_open = |i: usize| {
        eq_ci(i, 'u') && eq_ci(i + 1, 'r') && eq_ci(i + 2, 'l') && chars.get(i + 3) == Some(&'(')
    };
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    while i < chars.len() {
        if eq_ci(i, 'u') && is_url_open(i) {
            // 解析参数：可选空白 + 可选引号 + 内容 + 闭引号 + 可选空白 + ')'
            let mut j = i + 4;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let quote = match chars.get(j) {
                Some('\'') => Some('\''),
                Some('"') => Some('"'),
                _ => None,
            };
            let start = if quote.is_some() { j + 1 } else { j };
            let mut end = start;
            match quote {
                Some(q) => {
                    while end < chars.len() && chars[end] != q {
                        end += 1;
                    }
                }
                None => {
                    while end < chars.len() && !chars[end].is_whitespace() && chars[end] != ')' {
                        end += 1;
                    }
                }
            }
            let mut close = end;
            if quote.is_some() && close < chars.len() {
                close += 1; // 越过闭引号
            }
            while close < chars.len() && chars[close].is_whitespace() {
                close += 1;
            }
            if close < chars.len() && chars[close] == ')' {
                let url: String = chars[start..end].iter().collect();
                let trimmed = url.trim();
                if is_remote_http_url(trimmed) {
                    let enc: String =
                        url::form_urlencoded::byte_serialize(trimmed.as_bytes()).collect();
                    out.push_str(&format!("url(\"/proxy/image?url={enc}\")"));
                } else {
                    // 非远程（data:、相对、cid 等）：整段原样
                    for c in &chars[i..=close] {
                        out.push(*c);
                    }
                }
                i = close + 1;
                continue;
            }
            // 闭括号缺失（畸形 CSS）：剩余原文吐出，不再解析
            for c in &chars[i..] {
                out.push(*c);
            }
            return out;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 剥掉顶层单一包裹 div（企业 HTML 邮件常把全文包在一层 `<div style=…>` 里）。
///
/// 富文本编辑器（wangEditor）按顶层元素解析：p/table/h2 等顶层节点均保真，
/// 但顶层未知容器 div 会被整体打平成纯文本（配案表格变一行字，实测 2026-11-03）。
/// 仅当整串恰好被一个 div 包裹（首个标签是 `<div…>` 且其配对 `</div>` 在末尾）
/// 才展开内部内容；嵌套子 div 或多顶层节点混排原样返回。循环剥壳处理多重包裹。
/// 只用于编辑器注入链路（回复/转发/草稿回填），展示与收件存储仍保留原 div 版式。
pub fn unwrap_top_div(html: &str) -> String {
    let mut cur = html.trim().to_string();
    while let Some(inner) = single_top_div(&cur) {
        cur = inner.trim().to_string();
    }
    cur
}

/// 若 `s` 恰好被一个顶层 div 包裹，返回其内部内容；否则 None
fn single_top_div(s: &str) -> Option<String> {
    let after_div = s.strip_prefix("<div")?;
    // 开标签结束位置（ammonia 输出的属性值中 > 已转义为 &gt;，首个 > 即标签边界）
    // 注意：tag_end 相对 after_div，换算回 s 的绝对下标需 +4（前缀 "<div" 长度）
    let tag_end = after_div.find('>')?;
    let tag_body = &after_div[..tag_end];
    // 自闭合 <div/> 不是包裹容器
    if tag_body.ends_with('/') {
        return None;
    }
    let content_start = 4 + tag_end + 1;
    // 扫描 div 深度找配对闭合
    let mut depth = 1i32;
    let mut i = content_start;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &s[i..];
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[1..gt];
        let lower = tag.to_ascii_lowercase();
        if lower.starts_with("div") || lower.starts_with("/div") {
            if lower.starts_with('/') {
                depth -= 1;
            } else if !lower.rsplit('/').next().is_some_and(str::is_empty) {
                depth += 1;
            } else {
                return None; // <div/> 自闭合：按非包裹处理
            }
            if depth == 0 {
                // 闭合标签后必须到末尾（否则该 div 只是多个顶层节点之一）
                return if s[i + gt + 1..].trim().is_empty() {
                    Some(s[content_start..i].to_string())
                } else {
                    None
                };
            }
        }
        i += gt + 1;
    }
    None
}

/// HTML 剥标签为纯文本（基础实体解码）：用于 HTML 正文的检索/列表预览/转发引用。
/// 索引扫描而非吞并式迭代：未闭合 `<` 不应吞掉后文（畸形 HTML 常见）。
/// script/style/head/title 的**内容**一并跳过（邮件 HTML 常带大段 CSS/条件注释，
/// 剥成纯文本时会变成看不懂的"乱码"），HTML 注释整段丢弃。
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        match html.as_bytes()[i] {
            b'<' if html[i..].starts_with("<!--") => {
                // 注释：找到 --> 整段跳过；未闭合则丢弃余下内容
                match html[i + 4..].find("-->") {
                    Some(end) => i += 4 + end + 3,
                    None => break,
                }
            }
            b'<' => match html[i..].find('>') {
                Some(rel) => {
                    let tag = &html[i + 1..i + rel];
                    let name = tag
                        .trim_start_matches('/')
                        .split(|c: char| c.is_whitespace() || c == '/')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    i += rel + 1;
                    if matches!(name.as_str(), "script" | "style" | "head" | "title") {
                        // 非自闭合的隐藏块：跳到对应闭合标签之后
                        if tag.starts_with('/') {
                            continue;
                        }
                        let lower = html[i..].to_ascii_lowercase();
                        match lower.find(&format!("</{name}")) {
                            Some(close) => match html[i + close..].find('>') {
                                Some(gt) => i += close + gt + 1,
                                None => break,
                            },
                            None => break, // 无闭合标签：余下全是隐藏块内容
                        }
                    }
                }
                None => {
                    // 未闭合：保留 '<' 与后文（不解析）
                    out.push_str(&html[i..]);
                    break;
                }
            },
            b'&' => decode_entity(html, &mut i, &mut out),
            _ => {
                let ch = html[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// HTML 实体解码一步（strip_html 与 MD 导出共用）：从 `&` 起扫描到 `;`
/// （最多 8 字符），命中已知实体解码；未闭合/未知原样输出。
/// `i` 指向 `&` 字节，返回时推进到实体之后。
fn decode_entity(html: &str, i: &mut usize, out: &mut String) {
    let rest = &html[*i + 1..];
    match rest.find(';').filter(|pos| *pos <= 8) {
        Some(pos) => {
            match &rest[..pos] {
                "nbsp" => out.push(' '),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "amp" => out.push('&'),
                "quot" => out.push('"'),
                // 数字字符引用：&#20320;（十进制）/ &#x4f60;（十六进制）。
                // 中文邮件常把正文编码成数字实体（GB2312 网关/老客户端
                // 常见）；此前走 other 分支原样输出残串（"#20320"），
                // 列表预览即"乱码"而详情页（浏览器解码）正常。
                // 非法码位（0/越大/代理区）原样保留，不 panic
                other if other.starts_with('#') => {
                    let num = other[1..].strip_prefix(['x', 'X']);
                    let parsed = match num {
                        Some(hex) => u32::from_str_radix(hex, 16),
                        None => other[1..].parse::<u32>(),
                    };
                    match parsed.ok().and_then(char::from_u32) {
                        // 控制字符（含 \0）对预览/检索文本无意义，原样保留标记
                        Some(c) if !c.is_control() => out.push(c),
                        // 非法码位原样保留（含 & 与 ;），避免留下 "#20320" 残串
                        _ => {
                            out.push('&');
                            out.push_str(other);
                            out.push(';');
                        }
                    }
                }
                other => match named_entity(other) {
                    Some(c) => out.push(c),
                    // 未知实体原样保留（含 & 与 ;），不能只留下名称残串
                    None => {
                        out.push('&');
                        out.push_str(other);
                        out.push(';');
                    }
                },
            }
            *i += 1 + pos + 1;
        }
        None => {
            out.push('&');
            *i += 1;
        }
    }
}

/// 常见命名实体（非数字）：中文 HTML 邮件大量使用弯引号/省略号/破折号，
/// 不解码时列表预览会把 `&hellip;`、`&ldquo;` 等原样显示成“乱码”
/// （详情页由浏览器解码，故只在预览侧出现）。名称按 ASCII 不区分大小写。
fn named_entity(name: &str) -> Option<char> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "ldquo" => '“',
        "rdquo" => '”',
        "lsquo" => '‘',
        "rsquo" => '’',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "times" => '×',
        "divide" => '÷',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "sect" => '§',
        "para" => '¶',
        "plusmn" => '±',
        "frac12" => '½',
        "frac14" => '¼',
        "frac34" => '¾',
        "sup2" => '²',
        "sup3" => '³',
        "micro" => 'µ',
        "ensp" | "emsp" | "thinsp" => ' ',
        _ => return None,
    })
}

/// 仅解码文本中的 HTML 实体（不动标签/结构）；用于存量 body_text 的显示侧
/// 兜底（入库时未识别的命名实体会原样留在纯文本列）。
pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'&' {
            decode_entity(text, &mut i, &mut out);
        } else {
            let ch = text[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(bytes)?;
    enc.finish()
}

fn gunzip(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut dec = GzDecoder::new(bytes);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).ok()?;
    Some(out)
}

// ---------- 时间展示（epoch 秒 → 本地时区 label） ----------

/// 列表行时间：今天 `HH:MM` / 同年 `M月D日` / 跨年 `YYYY-M-D`
pub fn time_label(epoch: i64) -> String {
    let Some(z) = zoned_of(epoch) else {
        return String::new();
    };
    let now = jiff::Zoned::now();
    if (z.year(), z.month(), z.day()) == (now.year(), now.month(), now.day()) {
        format!("{:02}:{:02}", z.hour(), z.minute())
    } else if z.year() == now.year() {
        format!("{}月{}日", z.month() as u32, z.day() as u32)
    } else {
        format!("{}-{}-{}", z.year(), z.month() as u32, z.day() as u32)
    }
}

/// 详情页完整时间 `YYYY-MM-DD HH:MM`
pub fn time_full(epoch: i64) -> String {
    zoned_of(epoch)
        .map(|z| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                z.year(),
                z.month() as u32,
                z.day() as u32,
                z.hour(),
                z.minute()
            )
        })
        .unwrap_or_default()
}

/// 定时发送时刻（M4-2）：epoch 秒 → datetime-local 控件值 `YYYY-MM-DDTHH:MM`
/// （compose 页定时选择器回显用；本地时区与控件语义一致）。
/// 0/负值视为"未定时"返回空串（控件不回显任何值）
pub fn sched_local_value(epoch: i64) -> String {
    if epoch <= 0 {
        return String::new();
    }
    zoned_of(epoch)
        .map(|z| {
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}",
                z.year(),
                z.month() as u32,
                z.day() as u32,
                z.hour(),
                z.minute()
            )
        })
        .unwrap_or_default()
}

/// 定时发送时刻（M4-2）：datetime-local 控件值 `YYYY-MM-DDTHH:MM` → epoch 秒。
/// 按本地时区解析（浏览器 datetime-local 不带时区，含义即用户系统时区；
/// 服务端 jiff 用同一系统时区，两端一致）。空串/非法格式返回 None
pub fn sched_epoch_of(local: &str) -> Option<i64> {
    let t = local.trim();
    if t.is_empty() {
        return None;
    }
    let dt: jiff::civil::DateTime = t.parse().ok()?;
    let z = dt.to_zoned(local_tz()).ok()?;
    Some(z.timestamp().as_second())
}

/// 高级检索日期（B1）：`YYYY-MM-DD` → 本地时区 epoch 秒。
/// end_of_day=true 时取 23:59:59（结束日期包含当天全部邮件）。
pub fn date_epoch_of(date: &str, end_of_day: bool) -> Option<i64> {
    let d: jiff::civil::Date = date.trim().parse().ok()?;
    let time = if end_of_day {
        jiff::civil::Time::new(23, 59, 59, 0).ok()?
    } else {
        jiff::civil::Time::new(0, 0, 0, 0).ok()?
    };
    let z = d.to_datetime(time).to_zoned(local_tz()).ok()?;
    Some(z.timestamp().as_second())
}

/// 时区缓存（zoned_of 在列表/详情高频调用；每次 Zoned::now() 读系统时区是浪费）
/// 本地时区缓存（jiff TimeZone 为 Arc 包裹，clone 仅引用计数）。
/// TTL 1 小时：长期驻留的应用在系统时区变更（出差/夏令时）后，
/// 至多 1 小时内所有时间展示切换到新时区，而非永久滞留启动时的时区
static LOCAL_TZ: std::sync::RwLock<Option<(jiff::tz::TimeZone, i64)>> =
    std::sync::RwLock::new(None);
const TZ_TTL_SECS: i64 = 3600;

fn local_tz() -> jiff::tz::TimeZone {
    {
        let guard = LOCAL_TZ.read().unwrap_or_else(|e| e.into_inner());
        if let Some((tz, at)) = guard.as_ref()
            && now_epoch() - at < TZ_TTL_SECS
        {
            return tz.clone();
        }
    }
    let fresh = jiff::Zoned::now().time_zone().clone();
    // 双检防并发重复加载（多线程同时过期）：后到者抢到写锁时可能已被刷新
    let mut guard = LOCAL_TZ.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some((fresh.clone(), now_epoch()));
    fresh
}

fn zoned_of(epoch: i64) -> Option<jiff::Zoned> {
    jiff::Timestamp::from_second(epoch)
        .ok()
        .map(|ts| ts.to_zoned(local_tz()))
}

// ---------- 草稿（撰写/存草稿/覆盖，§9） ----------

/// 历史数据修复（主题乱码）：早期收件路径对未编码的原始 GBK/GB2312 主题头
/// 退 Latin-1，库内主题是"每字节一 U+00xx 字符"的 latin1-mojibake。逆过程：
/// 把 U+00xx 字符还原为对应字节 → GB18030 解码，命中 CJK 即确认为 GBK 主题
/// 并写回（同步重算 subject_tokens 保 FTS 检索一致）。西文 latin1 主题（如
/// é→0xE9）GBK 解码不成 CJK，不误伤；启动后幂等（修完无 U+00xx 字符即空扫）
pub fn repair_gbk_subjects(conn: &mut rusqlite::Connection) -> Result<(), String> {
    let mojibake: Vec<(i64, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, subject FROM emails")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        rows.filter_map(Result::ok)
            .filter(|(_, s)| s.chars().any(|c| matches!(c, '\u{80}'..='\u{ff}')))
            .collect::<Vec<_>>()
    };
    if mojibake.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut fixed = 0usize;
    for (id, s) in mojibake {
        let bytes: Vec<u8> = s
            .chars()
            .filter(|c| (*c as u32) <= 0xff)
            .map(|c| c as u8)
            .collect();
        let (cow, _, _) = encoding_rs::GB18030.decode(&bytes);
        if cow.chars().any(is_cjk) {
            let subject = cow.trim();
            tx.execute(
                "UPDATE emails SET subject = ?2, subject_tokens = ?3 WHERE id = ?1",
                rusqlite::params![id, subject, tokenize(subject)],
            )
            .map_err(|e| e.to_string())?;
            fixed += 1;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    eprintln!("[repair] 主题 GBK 乱码修复 {fixed} 封");
    Ok(())
}

/// 存量正文乱码一次性修复（v12）：乱码 `body_text` 优先用同源 `body_html`
/// 派生的纯文本写回（详情页显示正常的那一列），无 HTML 时按 GB18030 逆推
/// 还原 UTF-8；同步重算 `body_tokens` 保 FTS 检索一致。
///
/// 之前只修收件侧解码（e5abf2f）对新邮件有效，存量数据不会变；93c174b 又
/// 以"应用未发布"为由移除了存量修复 → 老邮件列表预览一直乱码。显示侧
/// `preview_text` 已能让列表立即正常，本函数把搜索/转发/MD 导出也洗干净。
/// marker 保证只跑一次；幂等（修完不再判乱码）；失败不阻断启动。
pub fn repair_mojibake_bodies(conn: &mut rusqlite::Connection) -> Result<(), String> {
    const MARK: &str = "body_mojibake_v12";
    // 标记存独立表，**不能写 settings**：store_is_empty() 用 settings 表行数
    // 判定"空库首设"，往 settings 写标记会让首设向导误走「沿用既有库」的
    // 归属校验并拒绝保存。
    conn.execute(
        "CREATE TABLE IF NOT EXISTS repair_marks(k TEXT PRIMARY KEY, v TEXT NOT NULL)",
        [],
    )
    .map_err(|e| e.to_string())?;
    // 标记值 = 扫描时的「行数:最大 id」：无新邮件时完全跳过；有新邮件或
    // 数据目录被换成另一套库（id 体系变化）时重新全量扫一遍，避免空库
    // 首次启动写死 marker 导致后来导入的存量数据永远不被修复。
    let stamp: String = conn
        .query_row(
            "SELECT count(*) || ':' || COALESCE(max(id), 0) FROM emails",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let prev: String = conn
        .query_row("SELECT v FROM repair_marks WHERE k = ?1", [MARK], |r| {
            r.get(0)
        })
        .unwrap_or_default();
    if prev == stamp {
        return Ok(());
    }
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, body_text, COALESCE(body_html, '') FROM emails
                 WHERE body_text IS NOT NULL AND body_text <> ''",
            )
            .map_err(|e| e.to_string())?;
        let rs = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rs.filter_map(Result::ok).collect()
    };
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut fixed = 0usize;
    for (id, bt, bh) in rows {
        if !looks_mojibake(&bt) {
            continue;
        }
        let from_html = if bh.trim().is_empty() {
            None
        } else {
            let derived = decode_entities(&strip_html(&bh)).trim().to_string();
            (!derived.is_empty() && !looks_mojibake(&derived)).then_some(derived)
        };
        let Some(new_bt) = from_html.or_else(|| recover_gbk_mojibake(&bt)) else {
            continue;
        };
        tx.execute(
            "UPDATE emails SET body_text = ?2, body_tokens = ?3 WHERE id = ?1",
            rusqlite::params![id, new_bt, tokenize(&new_bt)],
        )
        .map_err(|e| e.to_string())?;
        fixed += 1;
    }
    tx.execute(
        "INSERT OR REPLACE INTO repair_marks(k, v) VALUES(?1, ?2)",
        rusqlite::params![MARK, stamp],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    if fixed > 0 {
        eprintln!("[repair] 正文乱码修复 {fixed} 封（v12 一次性）");
    }
    Ok(())
}

/// 会话归组键存量回填（M2-2，v8 升级/首启）：把 NULL 的 thread_key 按 subject
/// 普通化补齐（空主题 → 空串）。幂等：`WHERE thread_key IS NULL` 只扫待补行，
/// 补齐后空扫退出；失败不阻断启动（与联系人回填同策略）。应在主题修复后调用，
/// 使 backfill 基于已修复的 subject 计算归组键。
pub fn backfill_thread_keys(conn: &mut rusqlite::Connection) -> Result<(), String> {
    let targets: Vec<(i64, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, subject FROM emails WHERE thread_key IS NULL")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        rows.filter_map(Result::ok).collect::<Vec<_>>()
    };
    if targets.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    for (id, subject) in targets {
        let key = thread_key_of(&subject);
        tx.execute(
            "UPDATE emails SET thread_key = ?2 WHERE id = ?1",
            rusqlite::params![id, key],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// 撰写表单入参（存草稿；发送为 M3）
pub struct DraftInput {
    /// 覆盖已有草稿的 id（None = 新建）
    pub id: Option<i64>,
    pub from_addr: String,
    pub from_name: String,
    pub to_addrs: String,
    pub cc_addrs: String,
    /// 密送（逗号分隔）；存库含发件存档，MIME 头不携带（lettre drop_bcc）
    pub bcc_addrs: String,
    pub subject: String,
    pub body_text: String,
    /// 回复链：指向被回复邮件 id（§3 in_reply_to）
    pub in_reply_to: Option<i64>,
    /// 定时发送时刻（M4-2，epoch 秒；0 = 非定时。仅草稿有效：存草稿保留，
    /// 到点调度线程入队发送；发送路径写 0 = 立即发并清除既有定时）
    pub scheduled_at: i64,
}

/// 新建/覆盖草稿；返回草稿 id。
///
/// 覆盖要求目标行存在且 folder='draft'；更新走 emails_au 触发器同步 FTS。
/// 幂等：同入参重复提交覆盖同一行，结果一致。
pub fn save_draft(state: &AppState, input: &DraftInput) -> Result<i64, String> {
    save_draft_impl(state, input, None)
}

/// 带 HTML 正文保存草稿（撰写页 HTML 模式）：入库前净化，`body_text` 由调用方
/// 提供纯文本版（检索/预览/转发引用）。净化后为空则按纯文本处理。
pub fn save_draft_with_html(
    state: &AppState,
    input: &DraftInput,
    html: &str,
) -> Result<i64, String> {
    let clean = sanitize_html(html);
    if clean.trim().is_empty() {
        save_draft_impl(state, input, None)
    } else {
        save_draft_impl(state, input, Some(&clean))
    }
}

fn save_draft_impl(
    state: &AppState,
    input: &DraftInput,
    body_html: Option<&str>,
) -> Result<i64, String> {
    let st = tokenize(&input.subject);
    let bt = tokenize(&input.body_text);
    let tk = thread_key_of(&input.subject);
    let now = now_epoch();
    let body_kind = if body_html.is_some() { "html" } else { "plain" };
    state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        if let Some(id) = input.id {
            let is_draft: Option<bool> = tx
                .query_row(
                    "SELECT folder = 'draft' FROM emails WHERE id = ?1",
                    [id],
                    |r| r.get::<_, bool>(0),
                )
                .optional()
                .map_err(e)?;
            if !is_draft.unwrap_or(false) {
                return Err("草稿不存在或已被发送".into());
            }
            tx.execute(
                "UPDATE emails SET from_addr=?2, from_name=?3, to_addrs=?4, cc_addrs=?5,
                        subject=?6, body_text=?7, subject_tokens=?8, body_tokens=?9,
                        in_reply_to=?10, body_kind=?11, body_html=?12, bcc_addrs=?13,
                        thread_key=?14, scheduled_at=?15
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    input.from_addr,
                    input.from_name,
                    input.to_addrs,
                    input.cc_addrs,
                    input.subject,
                    input.body_text,
                    st,
                    bt,
                    input.in_reply_to,
                    body_kind,
                    body_html,
                    input.bcc_addrs,
                    tk,
                    input.scheduled_at,
                ],
            )
            .map_err(e)?;
            tx.commit().map_err(e)?;
            Ok(id)
        } else {
            tx.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    cc_addrs, subject, body_text, subject_tokens, body_tokens,
                                    unread, created_at, in_reply_to, body_kind, body_html, bcc_addrs,
                                    thread_key, scheduled_at)
                 VALUES ('out', 'draft', 'draft', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    input.from_addr,
                    input.from_name,
                    input.to_addrs,
                    input.cc_addrs,
                    input.subject,
                    input.body_text,
                    st,
                    bt,
                    now,
                    input.in_reply_to,
                    body_kind,
                    body_html,
                    input.bcc_addrs,
                    tk,
                    input.scheduled_at,
                ],
            )
            .map_err(e)?;
            let id = tx.last_insert_rowid();
            tx.commit().map_err(e)?;
            Ok(id)
        }
    })
}

// ---------- 列表与详情（§8.3/§9） ----------

/// 列表排序键（A2）：默认星标置顶 + 时间倒序；其余选项放弃置顶，
/// 保证排序语义纯粹（选「时间正序」却把星标邮件顶在最前会很困惑）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    /// 星标置顶 → 时间倒序 → id 倒序（默认，与旧行为一致）
    #[default]
    DateDesc,
    /// 纯时间正序（最早在前）
    DateAsc,
    /// 发件人（无姓名时用地址）不区分大小写升序，同人按时间倒序
    FromAsc,
    /// 主题不区分大小写升序，同主题按时间倒序
    SubjectAsc,
}

impl SortKey {
    /// URL/表单解析：未知值一律回退默认（客户端可控参数，不报错）
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("date_asc") => Self::DateAsc,
            Some("from_asc") => Self::FromAsc,
            Some("subject_asc") => Self::SubjectAsc,
            _ => Self::DateDesc,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DateDesc => "date_desc",
            Self::DateAsc => "date_asc",
            Self::FromAsc => "from_asc",
            Self::SubjectAsc => "subject_asc",
        }
    }
}

/// 排序 SQL（alias 为空或 "e."）。时间列 t 是 SELECT 别名（AS t），
/// SQLite 不允许别名带表前缀（e.t 报 no such column），故 t 始终裸写；
/// starred/from_*/subject 是真实列，随 alias 前缀走
fn order_sql(sort: SortKey, alias: &str) -> String {
    let a = alias;
    match sort {
        SortKey::DateDesc => format!("{a}starred DESC, t DESC, {a}id DESC"),
        SortKey::DateAsc => format!("t ASC, {a}id ASC"),
        SortKey::FromAsc => {
            format!("{a}from_name COLLATE NOCASE ASC, {a}from_addr COLLATE NOCASE ASC, t DESC")
        }
        SortKey::SubjectAsc => format!("{a}subject COLLATE NOCASE ASC, t DESC"),
    }
}

/// 列表查询（folder：inbox|sent|draft|all；unread 仅 inbox 有意义；page 从 1 起）
pub struct ListQuery {
    pub folder: String,
    pub page: i64,
    pub unread_only: bool,
    /// 只看外部发件人（收件箱「外部」筛选按钮）
    pub external_only: bool,
    /// 账号内部域（如 chinatelecom.cn；用于判定外部发件人，空 = 无内部域）
    pub internal_domain: String,
    /// 每页条数（路由层从 settings/URL 解析并钳制；list/search/list_threads
    /// 统一以本字段作 LIMIT，不再用全局常量）
    pub per: i64,
    /// 排序键（A2；不影响命中集，只影响 ORDER BY）
    pub sort: SortKey,
    /// 高级检索（B1）：发件人包含（地址或姓名，路由层已小写；空 = 不限）
    pub from_filter: String,
    /// 高级检索：起始日期 epoch 秒（0 = 不限；按邮件时间）
    pub date_from: i64,
    /// 高级检索：结束日期 epoch 秒（0 = 不限；路由已补到当天 23:59:59）
    pub date_to: i64,
    /// 高级检索：仅看有附件
    pub has_attach: bool,
}

/// 外部发件人判定：域名与内部域相同或为其子域时为内部。
/// 无 @ 的串（本地草稿等）不算外部；空内部域时所有带地址的来信均为外部。
/// 与 `external_sql` 保持同一口径（单测对照两者）。
pub fn is_external_addr(addr: &str, internal_domain: &str) -> bool {
    let addr = addr.trim();
    if addr.is_empty() {
        return false;
    }
    let Some((_, dom)) = addr.rsplit_once('@') else {
        return false;
    };
    let dom = dom.trim().to_ascii_lowercase();
    if dom.is_empty() {
        return false;
    }
    let internal = internal_domain
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    if internal.is_empty() {
        return true;
    }
    !(dom == internal || dom.ends_with(&format!(".{internal}")))
}

/// 外部发件人 SQL 表达式（与 `is_external_addr` 同口径；供列表过滤与行标记复用，
/// 避免"筛出来的"和"标出来的"不一致）。alias 为空或 "e."。
/// 绑定参数：:dom = 内部域（大小写不敏感）。
fn external_sql(alias: &str) -> String {
    let a = alias;
    // :dom 归一化：配置默认值形如 "@chinatelecom.cn"（含前导 @），
    // 直接拼 '%@' || :dom 会得到 '%@@chinatelecom.cn' 永不命中——
    // 必须去掉前导 @ 与空白后再比后缀，否则全部来信被误标外部。
    let dom = "lower(ltrim(trim(:dom), '@'))";
    format!(
        "({a}from_addr <> '' AND NOT (lower({a}from_addr) LIKE '%@' || {dom} \
         OR lower({a}from_addr) LIKE '%.' || {dom}))"
    )
}

/// 外部标记 SQL 列（空内部域时所有带地址来信都算外部）
fn external_col(alias: &str, domain: &str) -> String {
    if domain.trim().trim_start_matches('@').trim().is_empty() {
        format!("({alias}from_addr <> '')")
    } else {
        external_sql(alias)
    }
}

/// 列表行（邮件摘要）
#[derive(Serialize)]
pub struct EmailSummary {
    pub id: i64,
    pub folder: String,
    pub status: String,
    pub subject: String,
    pub from_display: String,
    pub to_addrs: String,
    pub body_preview: String,
    pub attach_cnt: i64,
    pub unread: bool,
    /// 星标（M2-1）：列表置顶 + 金色★图标；各信箱通用（收/发/草稿均可标）
    pub starred: bool,
    /// 外部发件人（列表「外部」标记；口径见 is_external_addr）
    pub from_external: bool,
    pub time_label: String,
    /// 标题安全 HTML：列表为转义原文，检索命中时为 <mark> 高亮版（模板 |safe 输出）
    pub subject_html: String,
    /// 悬停预览原始正文（压缩空白 + 300 字符截断 + 省略号，模板 data-tip 属性用）
    pub preview_tip: String,
    /// 定时发送时刻（M4-2；0 = 非定时，仅草稿行有意义，模板渲染 ⏰ 徽标）
    pub scheduled_at: i64,
    /// 定时时刻短标签（同/跨天口径同 time_label；scheduled_at=0 时为空串）
    pub sched_label: String,
}

/// 会话视图列表行（M2-2）：一行 = 一个 thread_key 归组（代表行=组内最新一封，
/// 星标置顶取组内任一有星标；thread_count = 组内往来邮件数）。
/// 只纳入已收/已发（草稿、发送失败不入会话）；空主题（thread_key=''）不归组
#[derive(Serialize)]
pub struct ThreadSummary {
    /// 代表邮件 id（进会话详情 /threads/{id} 用）
    pub id: i64,
    pub folder: String,
    pub status: String,
    pub subject: String,
    pub from_display: String,
    pub body_preview: String,
    pub attach_cnt: i64,
    pub unread: bool,
    /// 组内任一邮件星标即置顶 + 金★（与单封视图置顶语义一致）
    pub starred: bool,
    pub from_external: bool,
    pub time_label: String,
    /// 组内往来邮件数（会话徽标）
    pub thread_count: i64,
    /// 标题安全 HTML（列表为转义原文，检索命中时带 <mark> 高亮）
    pub subject_html: String,
    pub preview_tip: String,
}

/// 会话详情页数据（M2-2）：该 thread_key 归组的全部往来邮件，时间正序
#[derive(Serialize)]
pub struct ThreadDetail {
    /// 组主题（代表邮件原主题，展示用）
    pub subject: String,
    pub thread_key: String,
    /// 组内邮件摘要（已排除草稿/失败；正序 = 会话内从早到晚）
    pub items: Vec<EmailSummary>,
}

/// folder 白名单校验（未知值回落收件箱；SQL 内插前必须过白名单）。
/// 2026-11-09 移除「全部」：未知/畸形参数不再兜底为跨箱全量（避免意外暴露）
fn folder_of(raw: &str) -> &str {
    match raw {
        "inbox" | "sent" | "draft" => raw,
        _ => "inbox",
    }
}

/// 列表过滤条件片段（list/search/批量操作共用，保证"选中的 N 封"与
/// 列表显示严格同口径——批量操作跨页全选的直接依据）。
///
/// - `alias`：emails 表别名（list 为空串、search/批量为 "e."）；
///   alias 非空时各列引用带前缀
/// - 产出 `(where_sql, ext_col)`：where_sql 形如
///   ` AND folder = 'inbox' AND unread = 1 AND (:extf = 0 OR …)`，
///   ⚠️ 调用方需再拼 WHERE 主体或 FTS MATCH 段；:extf/:dom 由调用方绑定
fn list_filter_sql(q: &ListQuery, alias: &str) -> (String, String) {
    let a = alias;
    let folder = folder_of(&q.folder);
    let folder_sql = format!(" AND {a}folder = '{folder}'");
    let unread_sql = if q.unread_only {
        format!(" AND {a}unread = 1")
    } else {
        String::new()
    };
    let domain = q.internal_domain.trim();
    let ext_col = external_col(a, domain);
    // :extf = 0 时整段条件恒真：无需为"是否过滤"拼两套 SQL
    let ext_sql = format!(" AND (:extf = 0 OR {ext_col})");
    // 高级检索（B1）：同样用"常量参数恒在"的写法——SQL 结构固定，绑定参数
    // 恒为 4 个；未启用时 :fromf='' / :dfrom=0 / :dto=0 / :attf=0 短路
    let adv_sql = format!(
        " AND (:fromf = '' OR instr(lower({a}from_addr), :fromf) > 0
                        OR instr(lower({a}from_name), :fromf) > 0)
          AND (:dfrom = 0 OR COALESCE({a}received_at, {a}sent_at, {a}created_at) >= :dfrom)
          AND (:dto = 0 OR COALESCE({a}received_at, {a}sent_at, {a}created_at) <= :dto)
          AND (:attf = 0 OR {a}attach_cnt > 0)"
    );
    (
        format!("{folder_sql}{unread_sql}{ext_sql}{adv_sql}"),
        ext_col,
    )
}

/// 批量操作目标解析（POST /emails/batch 双模式）：
/// - 页内精确：`ids` 指定集合
/// - 跨页全选：`select_all=1` + 可选 `exclude_ids`（排除集），配 ListQuery
///   过滤条件——筛选条件与列表同口径（list_filter_sql），保证"全部 M 封"
///   就是当前列表筛选命中的集合
pub enum BatchTarget {
    /// 页内精确 ids（空 = 无操作）
    Ids(Vec<i64>),
    /// 跨页全选：当前筛选全集 - exclude_ids + 附加排除（见 batch_apply 注释）
    SelectAll { exclude: Vec<i64>, query: ListQuery },
}

/// 批量动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BatchAction {
    MarkRead,
    MarkUnread,
    /// 批量星标（M2-1 扩展进批量栏）：置 1，各信箱列表置顶
    Star,
    /// 批量取消星标：置 0（已星标行才计入受影响数）
    Unstar,
    Delete,
}

impl BatchAction {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "read" => Some(Self::MarkRead),
            "unread" => Some(Self::MarkUnread),
            "star" => Some(Self::Star),
            "unstar" => Some(Self::Unstar),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// 批量执行（M1-1）。单事务；delete 先收附件路径、删行（FTS 触发器同步）、
/// 提交后删文件、回收空目录——与单删同一模式。
/// 返回受影响行数（Ids 模式含不存在的 id；SelectAll 模式 = 命中数）。
pub fn batch_apply(
    state: &AppState,
    target: &BatchTarget,
    action: BatchAction,
) -> Result<usize, String> {
    let read_set = |sql_where: &str, params: &[&dyn rusqlite::ToSql]| {
        state.with_conn(|conn| {
            let mut stmt = conn
                .prepare(&format!("SELECT id FROM emails WHERE 1=1 {sql_where}"))
                .map_err(e)?;
            let rows = stmt.query_map(params, |r| r.get::<_, i64>(0)).map_err(e)?;
            rows.collect::<Result<Vec<i64>, _>>().map_err(e)
        })
    };
    // 解析目标 ids
    let ids: Vec<i64> = match target {
        BatchTarget::Ids(v) if v.is_empty() => return Ok(0),
        BatchTarget::Ids(v) => v.clone(),
        BatchTarget::SelectAll { exclude, query } => {
            let ext_filter = if query.external_only { 1 } else { 0 };
            let domain = query.internal_domain.trim().to_string();
            let from_filter = query.from_filter.trim().to_lowercase();
            let date_from = query.date_from;
            let date_to = query.date_to;
            let attach_filter = if query.has_attach { 1 } else { 0 };
            let (filter_sql, _) = list_filter_sql(query, "");
            // NOT IN 空集在 SQLite 恒真（与语义一致），但显式分段防拼接歧义
            let exclude_sql = if exclude.is_empty() {
                String::new()
            } else {
                let marks = exclude.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                format!(" AND id NOT IN ({marks})")
            };
            // 顺序必须与 filter_sql 中命名参数首次出现顺序一致：
            // :extf → :dom → :fromf → :dfrom → :dto → :attf（positional 绑定）
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![
                &ext_filter,
                &domain,
                &from_filter,
                &date_from,
                &date_to,
                &attach_filter,
            ];
            let owned: Vec<i64> = exclude.to_vec();
            for x in &owned {
                params.push(x as &dyn rusqlite::ToSql);
            }
            let sql = format!("{filter_sql}{exclude_sql}");
            read_set(&sql, &params)?
        }
    };
    if ids.is_empty() {
        return Ok(0);
    }
    // 单事务执行
    let root = attach_root(state);
    let n = state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        let marks = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let changed = match action {
            BatchAction::MarkRead | BatchAction::MarkUnread => {
                let v: i64 = if action == BatchAction::MarkRead {
                    0
                } else {
                    1
                };
                // 仅收件箱有意义（unread 列语义限定；批量标已读时对草稿/发件 noop）
                tx.execute(
                    &format!(
                        "UPDATE emails SET unread = {v} WHERE folder = 'inbox' AND id IN ({marks})"
                    ),
                    rusqlite::params_from_iter(ids.iter()),
                )
                .map_err(e)?
            }
            // 批量星标（M2-1 进批量栏）：全信箱行均合法（星标置顶对所有文件夹生效）。
            // starred 不在 emails_au 触发器的 FTS 监听列内，翻转不引发检索重写
            BatchAction::Star | BatchAction::Unstar => {
                let v: i64 = if action == BatchAction::Star { 1 } else { 0 };
                tx.execute(
                    &format!("UPDATE emails SET starred = {v} WHERE id IN ({marks})"),
                    rusqlite::params_from_iter(ids.iter()),
                )
                .map_err(e)?
            }
            BatchAction::Delete => {
                // 先收独立附件路径（行删除前；外键级联删附件行）
                let mut stmt = tx
                    .prepare(&format!(
                        "SELECT a.file_path FROM attachments a
                          JOIN emails e ON e.id = a.email_id
                          WHERE a.storage = 'file' AND e.id IN ({marks})"
                    ))
                    .map_err(e)?;
                let files: Vec<String> = stmt
                    .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(e)?
                    .filter_map(Result::ok)
                    .collect();
                drop(stmt);
                let n = tx
                    .execute(
                        &format!("DELETE FROM emails WHERE id IN ({marks})"),
                        rusqlite::params_from_iter(ids.iter()),
                    )
                    .map_err(e)?;
                tx.commit().map_err(e)?;
                // 提交后删文件（与单删/clean 同模式：失败残留由孤儿回收兜底）
                for rel in &files {
                    let _ = std::fs::remove_file(root.join(rel));
                }
                prune_empty_dirs(&root);
                return Ok::<usize, String>(n);
            }
        };
        tx.commit().map_err(e)?;
        Ok(changed)
    })?;
    Ok(n)
}

/// 列表分页查询，返回 (当前页行, 命中总数)
pub fn list(state: &AppState, q: &ListQuery) -> Result<(Vec<EmailSummary>, i64), String> {
    let (filter_sql, ext_col) = list_filter_sql(q, "");
    let ext_filter = if q.external_only { 1 } else { 0 };
    let domain = q.internal_domain.trim();
    let from_filter = q.from_filter.trim().to_lowercase();
    let date_from = q.date_from;
    let date_to = q.date_to;
    let attach_filter = if q.has_attach { 1 } else { 0 };
    let where_sql = format!("WHERE 1=1{filter_sql}");
    let order = order_sql(q.sort, "");
    let page = q.page.max(1);
    // 页码来自查询串：巨大值乘法需饱和（debug 构建溢出即 panic）
    let offset = (page - 1).saturating_mul(q.per);
    state.with_conn(|conn| {
        let total: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM emails {where_sql}"),
                rusqlite::named_params! { ":extf": ext_filter, ":dom": domain, ":fromf": from_filter, ":dfrom": date_from, ":dto": date_to, ":attf": attach_filter },
                |r| r.get(0),
            )
            .map_err(e)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, folder, status, subject, from_addr, from_name, to_addrs, body_text,
                        attach_cnt, unread,
                        COALESCE(received_at, sent_at, created_at) AS t,
                        {ext_col} AS from_external, starred, scheduled_at,
                        COALESCE(body_html, '') AS body_html
                 FROM emails {where_sql}
                 ORDER BY {order}
                 LIMIT :lim OFFSET :off"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map(
                rusqlite::named_params! {
                    ":extf": ext_filter, ":dom": domain, ":fromf": from_filter, ":dfrom": date_from, ":dto": date_to, ":attf": attach_filter, ":lim": q.per, ":off": offset
                },
                |r| summary_from_row(r, None),
            )
            .map_err(e)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(e)?;
        Ok((items, total))
    })
}

/// 会话视图列表（M2-2）：按 thread_key 归组，每组取代表行（组内最新一封，
/// 星标优先）。复用 list_filter_sql 的行级过滤，但 unread/external/搜索仅作用于
/// 代表行（"最新一封未读/来自外部/命中的会话"），口径与单封视图一致、实现干净。
/// 额外排除草稿与发送失败（folder='draft' / status draft/failed 不入会话），
/// 并排除空主题（thread_key 为空串，按行独立不归组）
pub fn list_threads(state: &AppState, q: &ListQuery) -> Result<(Vec<ThreadSummary>, i64), String> {
    let (filter_sql, ext_col) = list_filter_sql(q, "e.");
    let ext_filter = if q.external_only { 1 } else { 0 };
    let domain = q.internal_domain.trim();
    let from_filter = q.from_filter.trim().to_lowercase();
    let date_from = q.date_from;
    let date_to = q.date_to;
    let attach_filter = if q.has_attach { 1 } else { 0 };
    let page = q.page.max(1);
    // 页码来自查询串：巨大值乘法需饱和（debug 构建溢出即 panic）
    let offset = (page - 1).saturating_mul(q.per);
    // where_sql 作用于子查询内行（e 别名）：组级过滤沿用行级（代表行口径）
    let where_sql = format!(
        "WHERE 1=1{filter_sql}
         AND e.thread_key IS NOT NULL AND e.thread_key != ''
         AND e.folder <> 'draft' AND e.status NOT IN ('draft','failed')"
    );
    // 外层排序列（代表行字段）：会话按组内最新时间/最新发件人排序
    let order = order_sql(q.sort, "");
    state.with_conn(|conn| {
        // 会话组总数 = 满足条件的 distinct thread_key 数
        let total: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM (
                        SELECT e.thread_key FROM emails e {where_sql} GROUP BY e.thread_key)"
                ),
                rusqlite::named_params! { ":extf": ext_filter, ":dom": domain, ":fromf": from_filter, ":dfrom": date_from, ":dto": date_to, ":attf": attach_filter },
                |r| r.get(0),
            )
            .map_err(e)?;
        // 每组取 rn=1 的代表行（组内最新 + 星标优先），thread_count = 组内件数
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, folder, status, subject, from_addr, from_name, to_addrs, body_text,
                        attach_cnt, unread, t, thread_count, from_external, starred, body_html
                 FROM (
                    SELECT e.id, e.folder, e.status, e.subject, e.from_addr, e.from_name,
                           e.to_addrs, e.body_text, e.attach_cnt, e.unread,
                           COALESCE(e.body_html, '') AS body_html,
                           COALESCE(e.received_at, e.sent_at, e.created_at) AS t,
                           COUNT(*) OVER (PARTITION BY e.thread_key) AS thread_count,
                           ROW_NUMBER() OVER (PARTITION BY e.thread_key
                               ORDER BY e.starred DESC,
                                        COALESCE(e.received_at, e.sent_at, e.created_at) DESC,
                                        e.id DESC) AS rn,
                           {ext_col} AS from_external, e.starred
                    FROM emails e {where_sql}
                 )
                 WHERE rn = 1
                 ORDER BY {order}
                 LIMIT :lim OFFSET :off"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map(
                rusqlite::named_params! {
                    ":extf": ext_filter, ":dom": domain, ":fromf": from_filter, ":dfrom": date_from, ":dto": date_to, ":attf": attach_filter, ":lim": q.per, ":off": offset
                },
                thread_summary_from_row,
            )
            .map_err(e)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(e)?;
        Ok((items, total))
    })
}

/// 会话列表行映射（列序：id,folder,status,subject,from_addr,from_name,to_addrs,
/// body_text,attach_cnt,unread,t,thread_count,from_external,starred,body_html）
fn thread_summary_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadSummary> {
    let subject: String = r.get(3)?;
    let name: String = r.get(5)?;
    let addr: String = r.get(4)?;
    let body: String = r.get(7)?;
    let html: String = r.get(14)?;
    Ok(ThreadSummary {
        id: r.get(0)?,
        folder: r.get(1)?,
        status: r.get(2)?,
        subject: subject.clone(),
        from_display: if name.is_empty() { addr } else { name },
        body_preview: preview_text(&body, &html),
        preview_tip: preview_tip(&body, &html),
        attach_cnt: r.get(8)?,
        unread: r.get::<_, i64>(9)? == 1,
        starred: r.get::<_, i64>(13)? == 1,
        from_external: r.get::<_, i64>(12)? == 1,
        time_label: time_label(r.get(10)?),
        thread_count: r.get::<_, i64>(11)?,
        subject_html: html_escape(&subject),
    })
}

/// 会话详情（M2-2）：按组内任一邮件 id 定位 thread_key，返回该归组全部往来邮件
/// （时间正序；草稿/失败不入）。Ok(None) = id 不存在或不属于任何会话组
pub fn thread_mails(
    state: &AppState,
    rep_id: i64,
    internal_domain: &str,
) -> Result<Option<ThreadDetail>, String> {
    let meta: Option<(String, String)> = state.with_conn(|conn| {
        conn.query_row(
            "SELECT thread_key, subject FROM emails
             WHERE id = ?1 AND thread_key IS NOT NULL AND thread_key != ''",
            [rep_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(e)
    })?;
    let Some((thread_key, subject)) = meta else {
        return Ok(None);
    };
    let domain = internal_domain.trim();
    let ext_col = external_col("", domain);
    let items = state.with_conn(|conn| {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, folder, status, subject, from_addr, from_name, to_addrs, body_text,
                        attach_cnt, unread,
                        COALESCE(received_at, sent_at, created_at) AS t,
                        {ext_col} AS from_external, starred, scheduled_at,
                        COALESCE(body_html, '') AS body_html
                 FROM emails
                 WHERE thread_key = ?1 AND folder <> 'draft'
                   AND status NOT IN ('draft','failed')
                 ORDER BY COALESCE(received_at, sent_at, created_at) ASC, id ASC"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map([&thread_key], |r| summary_from_row(r, None))
            .map_err(e)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e)
    })?;
    Ok(Some(ThreadDetail {
        subject,
        thread_key,
        items,
    }))
}

/// 发件箱/草稿箱列表的「对方」展示（2026-11-09）：解析 to_addrs 取姓名/地址，
/// 有姓名用姓名，多个用「、」连接，超过 3 个截断补「等 N 位」；无法解析出
/// 任何地址时原样回显（草稿可能暂存未完成输入）
fn recipient_display(to_addrs: &str) -> String {
    const MAX: usize = 3;
    let list = contacts::parse_addr_list(to_addrs);
    if list.is_empty() {
        let t = to_addrs.trim();
        return if t.is_empty() {
            "（无收件人）".into()
        } else {
            t.into()
        };
    }
    let shown: Vec<String> = list
        .iter()
        .take(MAX)
        .map(|(a, n)| if n.is_empty() { a.clone() } else { n.clone() })
        .collect();
    if list.len() > MAX {
        format!("{} 等 {} 位", shown.join("、"), list.len())
    } else {
        shown.join("、")
    }
}

/// 列表行映射（list/search/thread_mails 共用；列序：id,folder,status,subject,
/// from_addr,from_name,to_addrs,body_text,attach_cnt,unread,t,from_external,
/// starred,scheduled_at,body_html）。highlight_words=Some 时标题做命中高亮
fn summary_from_row(
    r: &rusqlite::Row<'_>,
    highlight_words: Option<&[String]>,
) -> rusqlite::Result<EmailSummary> {
    let subject: String = r.get(3)?;
    let subject_html = match highlight_words {
        Some(words) => highlight_html(&subject, words),
        None => html_escape(&subject),
    };
    let name: String = r.get(5)?;
    let addr: String = r.get(4)?;
    let folder: String = r.get(1)?;
    let to_addrs: String = r.get(6)?;
    let body: String = r.get(7)?;
    let html: String = r.get(14)?;
    let scheduled_at = r.get::<_, i64>(13)?;
    // 发件箱/草稿箱列表展示收件人（2026-11-09）：这两箱的「对方」是收件人，
    // 显示发件人（自己）没有信息量
    let from_display = if folder == "sent" || folder == "draft" {
        recipient_display(&to_addrs)
    } else if name.is_empty() {
        addr
    } else {
        name
    };
    Ok(EmailSummary {
        id: r.get(0)?,
        folder: r.get(1)?,
        status: r.get(2)?,
        subject,
        from_display,
        to_addrs,
        body_preview: preview_text(&body, &html),
        preview_tip: preview_tip(&body, &html),
        attach_cnt: r.get(8)?,
        unread: r.get::<_, i64>(9)? == 1,
        from_external: r.get::<_, i64>(11)? == 1,
        time_label: time_label(r.get(10)?),
        subject_html,
        // 列尾追加（列序 12）：list/search 两条 SELECT 同位拼接，缺列即此函数报错
        starred: r.get::<_, i64>(12)? == 1,
        // 列尾追加（列序 13）：定时发送（仅草稿行 >0；三条 SELECT 同位）
        scheduled_at,
        sched_label: if scheduled_at > 0 {
            time_label(scheduled_at)
        } else {
            String::new()
        },
    })
}

/// 标题命中高亮（应用层实现，§4.3）：先在原文定位命中区间再分段转义，
/// 避免 escape 实体（&amp; 等）内字母被误高亮；大小写折叠仅 ASCII（中文恒等、偏移不错位）
fn highlight_html(subject: &str, words: &[String]) -> String {
    if subject.is_empty() || words.is_empty() {
        return html_escape(subject);
    }
    let lower = subject.to_ascii_lowercase();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for word in words {
        if word.is_empty() {
            continue;
        }
        let target = word.to_ascii_lowercase();
        let mut from = 0;
        while let Some(pos) = lower[from..].find(&target) {
            let start = from + pos;
            let end = start + target.len();
            spans.push((start, end));
            from = end;
        }
    }
    if spans.is_empty() {
        return html_escape(subject);
    }
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut out = String::with_capacity(subject.len() + merged.len() * 13);
    let mut pos = 0;
    for (start, end) in merged {
        out.push_str(&html_escape(&subject[pos..start]));
        out.push_str("<mark>");
        out.push_str(&html_escape(&subject[start..end]));
        out.push_str("</mark>");
        pos = end;
    }
    out.push_str(&html_escape(&subject[pos..]));
    out
}

/// 检索语句构造（§4.3）：与索引同源的 tokenize_words 分词，
/// 每词双引号包裹后 AND 拼接（词内无语法字符，双引号隔离 FTS 注入）；
/// 全部为停用词/无有效词 → None（调用方回退普通列表）
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_query(input: &str) -> Option<String> {
    let terms: Vec<String> = tokenize_words(input)
        .into_iter()
        .filter(|w| !w.contains('"'))
        .map(|w| format!("\"{w}\""))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" AND "))
    }
}

/// 全文检索列表（§4.3）：FTS5 MATCH + BM25 排序（标题 5 倍权重），
/// folder/unread 过滤与 list 一致；返回 Ok(None) 表示输入无有效词（调用方回退 list）
#[cfg_attr(not(test), allow(dead_code))]
pub fn search(
    state: &AppState,
    input: &str,
    q: &ListQuery,
) -> Result<Option<(Vec<EmailSummary>, i64)>, String> {
    let Some(match_expr) = build_query(input) else {
        return Ok(None);
    };
    // 复用 list_filter_sql（alias=e.）：检索结果与列表筛选同口径
    let (filter_sql, ext_col) = list_filter_sql(q, "e.");
    let ext_filter = if q.external_only { 1 } else { 0 };
    let domain = q.internal_domain.trim();
    let from_filter = q.from_filter.trim().to_lowercase();
    let date_from = q.date_from;
    let date_to = q.date_to;
    let attach_filter = if q.has_attach { 1 } else { 0 };
    let page = q.page.max(1);
    let offset = (page - 1).saturating_mul(q.per);
    let words = tokenize_words(input);
    // 排序：默认（无显式排序）保持 BM25 相关度优先；用户选了具体排序键
    // （时间/发件人/主题）则按所选排序——否则「选时间正序 + 搜索」时
    // 结果仍按相关度排，控件会显得失效
    let order = if q.sort == SortKey::DateDesc {
        "bm25(email_fts, 5.0, 1.0), e.id DESC".to_string()
    } else {
        order_sql(q.sort, "e.")
    };
    state.with_conn(|conn| {
        let total: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)
                     FROM email_fts JOIN emails e ON e.id = email_fts.rowid
                     WHERE email_fts MATCH :match{filter_sql}"
                ),
                rusqlite::named_params! { ":match": match_expr, ":extf": ext_filter, ":dom": domain, ":fromf": from_filter, ":dfrom": date_from, ":dto": date_to, ":attf": attach_filter },
                |r| r.get(0),
            )
            .map_err(e)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT e.id, e.folder, e.status, e.subject, e.from_addr, e.from_name,
                        e.to_addrs, e.body_text, e.attach_cnt, e.unread,
                        COALESCE(e.received_at, e.sent_at, e.created_at) AS t,
                        {ext_col} AS from_external, e.starred, e.scheduled_at,
                        COALESCE(e.body_html, '') AS body_html
                 FROM email_fts JOIN emails e ON e.id = email_fts.rowid
                 WHERE email_fts MATCH :match{filter_sql}
                 ORDER BY {order}
                 LIMIT :lim OFFSET :off"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map(
                rusqlite::named_params! {
                    ":match": match_expr, ":extf": ext_filter, ":dom": domain, ":fromf": from_filter, ":dfrom": date_from, ":dto": date_to, ":attf": attach_filter,
                    ":lim": q.per, ":off": offset
                },
                |r| summary_from_row(r, Some(&words)),
            )
            .map_err(e)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(e)?;
        Ok(Some((items, total)))
    })
}

/// 正文摘要：无条件取详情同源正文（见 `preview_base`）+ 压缩空白后取前
/// 100 字符（chars 截断防中文 panic）。
/// 预览正文的统一来源：与详情页**无条件同源**。
///
/// 详情页（email.html:155）优先渲染 `body_html`（净化后的 text/html 列），
/// 列表/会话/搜索过去读 `body_text`（text/plain 列）——同一封邮件两列内容，
/// 历史入库的 text/plain 被错声明的 charset 解成乱码，就出现「详情正常、
/// 列表乱码」。之前多次修复都在给 body_text 做解码兜底（实体/latin1/GBK），
/// 既救不回存量，也挡不住下一种错声明。这里直接从详情页那一列派生纯文本：
/// 不依赖任何乱码识别阈值，存量邮件不迁移、不重收、不重启即刻正常。
/// 无 HTML 的纯文本邮件才退回 body_text（并清理标签残留/解实体）。
fn preview_base(text: &str, html: &str) -> String {
    let raw = if html.trim().is_empty() {
        text.to_string()
    } else {
        strip_html(html)
    };
    // text/plain 被错标成 HTML 源码、或老邮件把标签当正文入库时，
    // 预览会显示 "<div>...</div>/CSS" 这类"乱码"——同样剥掉
    let cleaned = if has_html_markup(&raw) {
        strip_html(&raw)
    } else {
        raw
    };
    decode_entities(&cleaned)
}

/// 正文里是否含明确的 HTML 标签痕迹（不是普通文本里的 < > 符号）
fn has_html_markup(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "<div", "<p>", "<p ", "<span", "<table", "<br", "<html", "<body", "<style",
        "<script", "<font", "<td", "<tr", "<!--", "<b>", "<b ", "<strong", "<em>",
        "<i>", "<u>", "<a ", "<img", "<li", "<ul", "<h1", "<h2", "<h3",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// 正文摘要：详情同源修复 + 压缩空白后取前 100 字符
fn preview_text(text: &str, html: &str) -> String {
    flatten_ws(&preview_base(text, html))
        .chars()
        .take(100)
        .collect()
}

/// 悬停预览正文：同源修复 + 压缩空白 + 300 字符截断 + 超长补省略号
fn preview_tip(text: &str, html: &str) -> String {
    let flat = flatten_ws(&preview_base(text, html));
    let mut tip: String = flat.chars().take(300).collect();
    if flat.chars().count() > 300 {
        tip.push('…');
    }
    tip
}

/// 文本是否疑似字符编码乱码（三道护栏，防误伤正常中文/西文）：
/// ① 含 U+FFFD（解码丢字）；
/// ② UTF-8 被 latin1/cp1252 解码的特征串（Ã/Â/â€ 或 C1 控制字符）；
/// ③ 按 GB18030 逆推回字节恰为合法 UTF-8 且含 CJK（"璇—□镮囬"类）。
/// 高位字符 <4 一律不判，短文本巧合面太大。
pub(crate) fn looks_mojibake(text: &str) -> bool {
    if text.contains('\u{FFFD}') {
        return true;
    }
    if text.chars().filter(|c| (*c as u32) >= 0x80).count() < 4 {
        return false;
    }
    let latin1 = text
        .chars()
        .filter(|c| ('\u{80}'..='\u{FF}').contains(c))
        .count();
    let latin1_markers = text.contains('Ã')
        || text.contains('Â')
        || text.contains("â€")
        || text.chars().any(|c| ('\u{80}'..='\u{9F}').contains(&c))
        // 正常西文重音字符稀疏（占比低）；latin1 误标的乱码整串都是高位字符
        || (latin1 >= 4 && latin1 * 3 >= text.chars().count());
    if latin1_markers {
        return true;
    }
    recover_gbk_mojibake(text).is_some()
}

/// UTF-8 被按 GBK 误解码的乱码还原：文本按 GB18030 逆推回字节后恰为合法
/// UTF-8（且含 CJK、与原文不同）即认定乱码并返回重解码结果。
/// 误伤防护：高位字符 <4 不处理；编码不可逆（含 U+FFFD，字节已丢）放弃；
/// 修复结果须自身不再可逆推（防多层套娃改写）。
pub(crate) fn recover_gbk_mojibake(text: &str) -> Option<String> {
    if text.chars().filter(|c| (*c as u32) >= 0x80).count() < 4 {
        return None;
    }
    let (bytes, _, had_errors) = encoding_rs::GB18030.encode(text);
    if had_errors {
        return None;
    }
    let fixed = String::from_utf8(bytes.into_owned()).ok()?;
    if fixed == text || !fixed.chars().any(is_cjk) {
        return None;
    }
    if recover_gbk_mojibake(&fixed).is_some() {
        return None; // 修复结果还能再"修"一层 → 疑巧合文本，不动
    }
    Some(fixed)
}

/// 空白折叠（连续空白/换行折叠为单空格；preview 与 preview_tip 共用）
fn flatten_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 附件行（详情页/预览/另存；不携带 BLOB 本体）
#[derive(Serialize)]
pub struct AttachView {
    pub id: i64,
    pub filename: String,
    pub mime: String,
    pub size_orig: i64,
    pub storage: String,
    pub detached: bool,
    /// 预览方式（空 = 不支持内嵌预览）：img | pdf | text（text 类回显强制 text/plain 防脚本）
    pub preview_kind: String,
}

/// 附件预览方式判定（M5）：MIME 优先；MIME 缺失或为 octet-stream 时按文件扩展名兜底
/// （真实邮件附件头常不规范）；text/html 与 image/svg+xml 归入不可预览
/// （内嵌渲染会执行脚本）
fn preview_kind_of(mime: &str, filename: &str) -> &'static str {
    let m = mime.trim().to_ascii_lowercase();
    if m.is_empty() || m == "application/octet-stream" {
        let ext = filename
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        return match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => "img",
            "pdf" => "pdf",
            "txt" | "csv" | "log" | "json" | "xml" | "md" | "yml" | "yaml" | "ini" | "conf"
            | "sql" | "toml" | "ts" | "js" | "css" => "text",
            // Office 预览（B8）：docx/xlsx/pptx 视觉渲染，xls 由 SheetJS 解析
            "docx" | "docm" => "docx",
            "xlsx" | "xlsm" => "xlsx",
            "pptx" | "pptm" => "pptx",
            "xls" => "xls",
            _ => "",
        };
    }
    if m == "application/pdf" {
        return "pdf";
    }
    if m.starts_with("text/") && m != "text/html" {
        return "text";
    }
    // SVG 可携带脚本：顶层渲染会执行，不按图片内嵌预览
    if m.starts_with("image/") && m != "image/svg+xml" {
        return "img";
    }
    match m.as_str() {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.ms-word.document.macroenabled.12" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.ms-excel.sheet.macroenabled.12" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.ms-powerpoint.presentation.macroenabled.12" => "pptx",
        "application/vnd.ms-excel" => "xls",
        _ => "",
    }
}

/// 邮件详情（全部展示字段 + 附件列表）
#[derive(Serialize)]
pub struct EmailDetail {
    pub id: i64,
    pub direction: String,
    pub folder: String,
    pub status: String,
    pub subject: String,
    pub from_addr: String,
    pub from_name: String,
    pub to_addrs: String,
    pub cc_addrs: String,
    /// 密送地址（逗号分隔）；仅自己发出的邮件有值，收件恒空（MIME 头不含 Bcc）
    pub bcc_addrs: String,
    pub body_text: String,
    /// 净化后的 HTML 正文（空串 = 无 HTML 版式，详情页回退纯文本）
    pub body_html: String,
    pub attach_cnt: i64,
    pub attach_bytes: i64,
    pub unread: bool,
    /// 星标（M2-1；详情页按钮回显 + 回复/转发等场景不继承）
    pub starred: bool,
    pub in_reply_to: Option<i64>,
    pub error: String,
    pub time_full: String,
    /// 超过单封收件上限：只拉了信头，正文为占位说明（详情页据此显示提示条）
    pub too_large: bool,
    /// 定时发送时刻（M4-2；0 = 非定时，仅草稿态有意义）
    pub scheduled_at: i64,
    /// 定时时刻完整文本 `YYYY-MM-DD HH:MM`（scheduled_at=0 时为空串；模板直接渲染）
    pub scheduled_full: String,
    /// 撤销窗口内（scheduled_at 在未来 10s 内，A1）：详情页显示「即将发送」
    /// 而非「已定时」，提示此时仍可撤销
    pub scheduled_soon: bool,
    /// 原始信头（A5；空串 = 旧版本收取或未保存）：源码页排查投递/编码问题
    pub raw_headers: String,
    pub attachments: Vec<AttachView>,
}

pub fn get(state: &AppState, id: i64) -> Option<EmailDetail> {
    // 注意：本函数全程单次借锁——time epoch 并入主查询，勿在闭包内再调 with_conn
    state.with_conn(|conn| {
        let row = conn
            .query_row(
                "SELECT direction, folder, status, subject, from_addr, from_name, to_addrs,
                        cc_addrs, COALESCE(bcc_addrs, ''), body_text, COALESCE(body_html, ''),
                        attach_cnt, attach_bytes, unread, in_reply_to, COALESCE(error, ''),
                        COALESCE(received_at, sent_at, created_at), too_large, starred,
                        scheduled_at, COALESCE(raw_headers, '')
                 FROM emails WHERE id = ?1",
                [id],
                |r| {
                    Ok(EmailDetail {
                        id,
                        direction: r.get(0)?,
                        folder: r.get(1)?,
                        status: r.get(2)?,
                        subject: r.get(3)?,
                        from_addr: r.get(4)?,
                        from_name: r.get(5)?,
                        to_addrs: r.get(6)?,
                        cc_addrs: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                        bcc_addrs: r.get(8)?,
                        body_text: r.get(9)?,
                        body_html: r.get(10)?,
                        attach_cnt: r.get(11)?,
                        attach_bytes: r.get(12)?,
                        unread: r.get::<_, i64>(13)? == 1,
                        in_reply_to: r.get(14)?,
                        error: r.get(15)?,
                        time_full: time_full(r.get(16)?),
                        too_large: r.get::<_, i64>(17)? == 1,
                        starred: r.get::<_, i64>(18)? == 1,
                        scheduled_at: r.get(19)?,
                        scheduled_full: {
                            let at = r.get::<_, i64>(19)?;
                            if at > 0 { time_full(at) } else { String::new() }
                        },
                        scheduled_soon: {
                            let at = r.get::<_, i64>(19)?;
                            at > 0 && at <= now_epoch() + 10
                        },
                        raw_headers: r.get(20)?,
                        attachments: Vec::new(),
                    })
                },
            )
            .optional()
            .ok()??;
        let mut detail = row;
        detail.attachments = attachment_views(conn, id);
        Some(detail)
    })
}

fn attachment_views(conn: &rusqlite::Connection, email_id: i64) -> Vec<AttachView> {
    conn.prepare(
        "SELECT id, filename, COALESCE(mime,'application/octet-stream'), size_orig, storage,
                detached = 1
         FROM attachments WHERE email_id = ?1 ORDER BY id",
    )
    .and_then(|mut stmt| {
        stmt.query_map([email_id], |r| {
            let mime: String = r.get(2)?;
            let filename: String = r.get(1)?;
            Ok(AttachView {
                id: r.get(0)?,
                preview_kind: preview_kind_of(&mime, &filename).into(),
                filename,
                mime,
                size_orig: r.get(3)?,
                storage: r.get(4)?,
                detached: r.get(5)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
    })
    .unwrap_or_default()
}

/// 星标翻转（M2-1）：单语句原子取反，返回翻转后的状态。
/// 本地语义（不上 IMAP）；收件同步按 remote_uid 查重后仅 INSERT 新行、
/// 不 UPDATE 已有行，星标不会被同步覆盖（recv.rs store_incoming_conn）。
/// None = 邮件不存在（404）
pub fn toggle_star(state: &AppState, id: i64) -> Option<bool> {
    let flipped: Result<Option<bool>, String> = state.with_conn(|conn| {
        let changed = conn
            .execute(
                "UPDATE emails SET starred = 1 - starred WHERE id = ?1",
                [id],
            )
            .map_err(e)?;
        if changed == 0 {
            return Ok(None);
        }
        let v: i64 = conn
            .query_row("SELECT starred FROM emails WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .map_err(e)?;
        Ok(Some(v == 1))
    });
    flipped.unwrap_or(None)
}

/// 本地标记已读（IMAP 服务器回写由 seen worker 批量完成；§6.1）。返回是否生效
///
/// `seen_pending = 1` 与 `unread = 0` 同一语句写入：业务状态与"待回写"意向一起落盘，
/// 进程被杀后标志仍在，重启由 worker 补发（旧实现每次点击 spawn 一个线程各登录一次，
/// 连点 N 封 = N 次 IMAP 登录，企业服务器易限流）
pub fn mark_read(state: &AppState, id: i64) -> bool {
    let flipped = state.with_conn(|conn| {
        exec_one(
            conn,
            "UPDATE emails SET unread = 0,
                    seen_pending = CASE
                        WHEN direction = 'in' AND remote_uid IS NOT NULL THEN 1
                        ELSE seen_pending END
             WHERE id = ?1 AND unread = 1",
            id,
        )
    });
    if flipped {
        state.notify_seen();
    }
    flipped
}

/// 标为未读（A5 配套）：详情页主动留待办。只改本地 unread，
/// 不写 seen_pending（IMAP 无"反向已读"语义，不存在要回写的操作）。
/// 返回是否发生翻转（已是未读时为 false，幂等）。
pub fn mark_unread(state: &AppState, id: i64) -> bool {
    state.with_conn(|conn| {
        conn.execute(
            "UPDATE emails SET unread = 1 WHERE id = ?1 AND unread = 0",
            [id],
        )
        .map(|n| n == 1)
        .unwrap_or(false)
    })
}

/// 单行 CAS 更新：仅当恰好影响 1 行返回 true（0 行 = 条件不满足/重复提交，幂等）
fn exec_one(conn: &rusqlite::Connection, sql: &str, id: i64) -> bool {
    conn.execute(sql, [id]).map(|n| n == 1).unwrap_or(false)
}

// ---------- 删除（§5.3 硬删：行 + FTS（触发器）+ 独立附件文件） ----------

pub fn delete(state: &AppState, id: i64) -> Result<bool, String> {
    let root = attach_root(state);
    state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        // 收集独立文件相对路径（行删除前；外键级联会随 emails 删除附件行）
        let mut stmt = tx
            .prepare("SELECT file_path FROM attachments WHERE email_id = ?1 AND storage = 'file'")
            .map_err(e)?;
        let files: Vec<String> = stmt
            .query_map([id], |r| r.get::<_, String>(0))
            .map_err(e)?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);
        let n = tx
            .execute("DELETE FROM emails WHERE id = ?1", [id])
            .map_err(e)?;
        tx.commit().map_err(e)?;
        if n == 1 {
            for rel in &files {
                let _ = std::fs::remove_file(root.join(rel));
            }
            prune_empty_dirs(&root);
        }
        Ok(n == 1)
    })
}

/// 回收空目录（删邮件/清理器共用；孤儿文件全量回收为 M5 清理器职责）
pub fn prune_empty_dirs(root: &Path) {
    if let Ok(months) = std::fs::read_dir(root) {
        for m in months.flatten() {
            if !m.path().is_dir() {
                continue;
            }
            if let Ok(days) = std::fs::read_dir(m.path()) {
                for d in days.flatten() {
                    if d.path().is_dir()
                        && std::fs::read_dir(d.path())
                            .map(|mut i| i.next().is_none())
                            .unwrap_or(false)
                    {
                        let _ = std::fs::remove_dir(d.path());
                    }
                }
            }
            if std::fs::read_dir(m.path())
                .map(|mut i| i.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(m.path());
            }
        }
    }
}

fn attach_root(state: &AppState) -> PathBuf {
    state.data_dir().join("attachments")
}

/// 附件根目录（独立连接版）
fn attach_root_of(data_dir: &Path) -> PathBuf {
    data_dir.join("attachments")
}

/// 附件入库上限（字节）：settings inline_attach_max_mb（默认 20MB）
fn inline_limit_conn(conn: &rusqlite::Connection) -> u64 {
    conn.query_row(
        "SELECT v FROM settings WHERE k = 'inline_attach_max_mb'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|v| v.parse::<u64>().ok())
    .unwrap_or(20)
    .saturating_mul(1024 * 1024)
}

// ---------- 附件两级存储（§5.1） ----------

/// 附件入库上限（字节；AppState 薄封装，唯一实现在 inline_limit_conn）
fn inline_limit(state: &AppState) -> u64 {
    state.with_conn(|conn| inline_limit_conn(conn))
}

/// 文件名净化：Windows/Unix 保留字符替换为 `_`
/// 头部值清洗：控制字符（含 CR/LF）替换为空格。库内字段来自收件解析或用户
/// 输入，若夹带 CRLF 会被拼进邮件头产生头注入（伪造 From/To/X-* 等）；
/// RFC 5322 头值域本就不允许裸控制字符，清洗无信息损失。
/// eml 重建（拼头字符串）与 SMTP 发信（交给 lettre 前）共用。
pub(crate) fn head_safe(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// 控制字符折叠为单个空格（连续 CR/LF 不产生连续空位，头尾不产空白）：
/// 用于显示型文本（通知正文/联系人姓名），比 head_safe 更整洁。
pub(crate) fn collapse_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        if ch.is_control() {
            if !out.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// 文件名净化：非法字符与控制字符统一替换为 `_`，按字节截断到 200
/// （文件名组件上限通常 255 字节；中文多字节需按字节而非字符计）。
/// 入参来自邮件 MIME 头的 filename——恶意邮件可控，是落盘路径的唯一屏障。
fn safe_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(200));
    let mut bytes = 0usize;
    for c in name.chars() {
        let mapped = match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        };
        let len = mapped.len_utf8();
        if bytes + len > 200 {
            break;
        }
        bytes += len;
        out.push(mapped);
    }
    if out.is_empty() {
        return "attachment".to_string();
    }
    out
}

/// 附件入库上下文（insert_attachment 参数组，避免超长参数列表）
struct AttachCtx<'a> {
    tx: &'a rusqlite::Connection,
    root: &'a Path,
    month: &'a str,
    email_id: i64,
    limit: u64,
}

/// 单附件入库核心（事务内调用，add_attachments 与收件入库共用）：
/// gzip → 按上限分级（inline BLOB / 独立文件）→ 行插入。written 记录已写盘路径供回滚清理。
fn insert_attachment(
    ctx: &AttachCtx,
    filename: &str,
    mime: &str,
    orig: &[u8],
    written: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let gz = gzip(orig).map_err(e)?;
    let sha = hex_sha256(orig);
    let size_orig = orig.len() as i64;
    let n: i64 = ctx
        .tx
        .query_row(
            "SELECT count(*) FROM attachments WHERE email_id = ?1",
            [ctx.email_id],
            |r| r.get::<_, i64>(0),
        )
        .map_err(e)?
        + 1;
    if orig.len() as u64 <= ctx.limit {
        ctx.tx
            .execute(
                "INSERT INTO attachments(email_id, filename, mime, size_orig, size_gz,
                                         storage, data, sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'inline', ?6, ?7)",
                rusqlite::params![
                    ctx.email_id,
                    filename,
                    mime,
                    size_orig,
                    gz.len() as i64,
                    gz,
                    sha
                ],
            )
            .map_err(e)?;
    } else {
        // 序号避让：`{n}_{name}.gz` 若已存在（删中间附件后序号复用、或同名文件重复添加）
        // 必须递增，否则后写覆盖前一个附件文件，两条 DB 行指向同一路径（审计 P2-10）
        let mut rel = String::new();
        for seq in n..n.saturating_add(10_000) {
            let candidate = format!("{}/{}/{}_{}.gz", ctx.month, ctx.email_id, seq, filename);
            if !ctx.root.join(&candidate).exists() {
                rel = candidate;
                break;
            }
        }
        if rel.is_empty() {
            // 极端兜底：内容寻址命名（sha256 前 16 位），仍保证唯一
            rel = format!(
                "{}/{}/{}_{}.gz",
                ctx.month,
                ctx.email_id,
                &sha[..16.min(sha.len())],
                filename
            );
        }
        let dest = ctx.root.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|err| format!("创建附件目录失败：{err}"))?;
        }
        std::fs::write(&dest, &gz).map_err(|err| format!("写入附件文件失败：{err}"))?;
        written.push(dest);
        ctx.tx
            .execute(
                "INSERT INTO attachments(email_id, filename, mime, size_orig, size_gz,
                                         storage, file_path, sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'file', ?6, ?7)",
                rusqlite::params![
                    ctx.email_id,
                    filename,
                    mime,
                    size_orig,
                    gz.len() as i64,
                    rel,
                    sha
                ],
            )
            .map_err(e)?;
    }
    Ok(())
}

/// 冗余计数回写（附件增删后同步 emails.attach_cnt/attach_bytes）
fn sync_attach_stats(tx: &rusqlite::Connection, email_id: i64) -> Result<(), String> {
    tx.execute(
        "UPDATE emails SET
             attach_cnt  = (SELECT count(*) FROM attachments WHERE email_id = ?1),
             attach_bytes = (SELECT COALESCE(sum(size_orig), 0) FROM attachments WHERE email_id = ?1)
         WHERE id = ?1",
        [email_id],
    )
    .map_err(e)?;
    Ok(())
}

/// 追加附件（写入方：草稿保存/M4 收件）。
///
/// 读文件 → gzip → 按上限分级：≤ 上限 inline BLOB；超限独立文件（月份目录取邮件时间）。
/// 任一文件读取失败则整体失败（事务回滚，不留半成品）；**0 字节文件跳过并回报**——
/// 旧实现遇空文件直接整单失败，用户选错一个文件就白填整封信（审计 P2-9）。
/// 返回 (成功入库数, 被跳过的空文件名)。
pub fn add_attachments(
    state: &AppState,
    email_id: i64,
    paths: &[String],
) -> Result<(usize, Vec<String>), String> {
    if paths.is_empty() {
        return Ok((0, Vec::new()));
    }
    let limit = inline_limit(state);
    let root = attach_root(state);
    let month = month_dir_of(state, email_id);
    let mut written_files: Vec<PathBuf> = Vec::new();
    // 敏感路径守卫（审计 M2）：paths 是表单值（客户端可改），入库前逐一校验，
    // 拒绝读取库/密钥/配置自身（防"读库+读密钥 → 外泄 keyfile 密码"原语）
    for p in paths {
        guard_sensitive_path(state, p)?;
    }
    let result = state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        let mut count = 0usize;
        let mut skipped: Vec<String> = Vec::new();
        for p in paths {
            let orig = std::fs::read(p)
                .map_err(|err| format!("读取附件失败 {}: {err}", safe_filename(p)))?;
            let filename = Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .map(safe_filename)
                .unwrap_or_else(|| "attachment".into());
            if orig.is_empty() {
                skipped.push(filename);
                continue;
            }
            insert_attachment(
                &AttachCtx {
                    tx: &tx,
                    root: &root,
                    month: &month,
                    email_id,
                    limit,
                },
                &filename,
                mime_of(&filename),
                &orig,
                &mut written_files,
            )?;
            count += 1;
        }
        sync_attach_stats(&tx, email_id)?;
        tx.commit().map_err(e)?;
        Ok((count, skipped))
    });
    if result.is_err() {
        // 事务失败：清理本次已写盘的独立文件（防残留）
        for f in &written_files {
            let _ = std::fs::remove_file(f);
        }
    }
    result
}

/// 以字节形式添加附件（B6 转发为附件：.eml 在内存生成，不落临时文件）。
/// 与 add_attachments 共用 insert_attachment/统计同步；失败时回滚已写盘文件。
pub fn add_attachment_bytes(
    state: &AppState,
    email_id: i64,
    filename: &str,
    mime: &str,
    data: &[u8],
) -> Result<(), String> {
    if data.is_empty() {
        return Err("附件内容为空".to_string());
    }
    let limit = inline_limit(state);
    let root = attach_root(state);
    let month = month_dir_of(state, email_id);
    let name = safe_filename(filename);
    state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        let mut written: Vec<PathBuf> = Vec::new();
        let stored = insert_attachment(
            &AttachCtx {
                tx: &tx,
                root: &root,
                month: &month,
                email_id,
                limit,
            },
            &name,
            mime,
            data,
            &mut written,
        );
        if let Err(err) = stored {
            for p in &written {
                let _ = std::fs::remove_file(p);
            }
            return Err(err);
        }
        sync_attach_stats(&tx, email_id)?;
        tx.commit().map_err(e)?;
        Ok(())
    })
}

/// 富文本撰写页"超大内嵌图片转附件"策略的载荷形态（前端从 data:image 抽取）
#[derive(Clone)]
pub struct InlineImage {
    pub filename: String,
    pub mime: String,
    /// 原始字节（非 base64——解码在解析表单 JSON 时完成）
    pub bytes: Vec<u8>,
}

/// 内嵌图片入库为附件（与 add_attachments 同一套分级存储语义）。
/// 前端把 ≥2MB 的 data:image 从正文抽出转到这里，防止内嵌 base64 撑爆单封体积。
pub fn add_inline_images(
    state: &AppState,
    email_id: i64,
    imgs: &[InlineImage],
) -> Result<usize, String> {
    if imgs.is_empty() {
        return Ok(0);
    }
    let limit = inline_limit(state);
    let root = attach_root(state);
    let month = month_dir_of(state, email_id);
    let mut written_files: Vec<PathBuf> = Vec::new();
    let result = state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        let mut count = 0usize;
        for img in imgs {
            insert_attachment(
                &AttachCtx {
                    tx: &tx,
                    root: &root,
                    month: &month,
                    email_id,
                    limit,
                },
                &img.filename,
                &img.mime,
                &img.bytes,
                &mut written_files,
            )?;
            count += 1;
        }
        sync_attach_stats(&tx, email_id)?;
        tx.commit().map_err(e)?;
        Ok(count)
    });
    if result.is_err() {
        for f in &written_files {
            let _ = std::fs::remove_file(f);
        }
    }
    result
}

/// 草稿重存时按 keep 列表裁剪附件（删除行 + 独立文件；attach_cnt/bytes 回写）
pub fn remove_attachments_except(
    state: &AppState,
    email_id: i64,
    keep: &[i64],
) -> Result<(), String> {
    let root = attach_root(state);
    // 事务内只删行，独立文件在提交成功后删除（commit 失败则文件仍有效，不留半删除状态）
    let victims: Vec<String> = state.with_conn(|conn| -> Result<Vec<String>, String> {
        let tx = conn.transaction().map_err(e)?;
        let rows: Vec<(i64, Option<String>)> = crate::db::collect_rows(
            &tx,
            "SELECT id, file_path FROM attachments WHERE email_id = ?1",
            &[&email_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(e)?;
        let victims: Vec<(i64, Option<String>)> = rows
            .into_iter()
            .filter(|(aid, _)| !keep.contains(aid))
            .collect();
        for (aid, _) in &victims {
            tx.execute("DELETE FROM attachments WHERE id = ?1", [aid])
                .map_err(e)?;
        }
        sync_attach_stats(&tx, email_id)?;
        tx.commit().map_err(e)?;
        Ok(victims.into_iter().filter_map(|(_, rel)| rel).collect())
    })?;
    for rel in victims {
        let _ = std::fs::remove_file(root.join(rel));
    }
    prune_empty_dirs(&root);
    Ok(())
}

/// 路径词法归一（无 I/O）：去 `.`/冗余分隔符，解析 `..` 为父目录。
/// 仅用于路径比对，不保证与文件系统真实路径一致（符号链接场景见 comparable_path）
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop(); // 根目录上 pop 为 no-op
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 比较用路径串：目标存在则取 canonicalize 实路径（穿透符号链接与 `..`）；
/// 含不存在组件时先词法归一，再对最长存在前缀 canonicalize（穿祖先符号链接，
/// 如 macOS `/var`→`/private/var`，保证与敏感文件实路径同一基准），余下
/// 部分逐组件附加。统一小写：macOS/Windows 文件系统大小写不敏感；Linux
/// 上大小写变体过度拦截无攻击获益，保守可用
fn comparable_path(p: &Path) -> String {
    let clean = lexical_normalize(p);
    if let Ok(real) = std::fs::canonicalize(&clean) {
        return real.to_string_lossy().to_lowercase();
    }
    let comps: Vec<std::path::Component<'_>> = clean.components().collect();
    // 最长存在前缀：从根逐组件累积，遇第一个不存在组件截断
    let mut acc = PathBuf::new();
    let mut cut = comps.len();
    for (i, c) in comps.iter().enumerate() {
        acc.push(c.as_os_str());
        if !acc.exists() {
            cut = i;
            break;
        }
    }
    if cut == 0 {
        return clean.to_string_lossy().to_lowercase();
    }
    let mut out = std::fs::canonicalize(&acc).unwrap_or(acc);
    for c in &comps[cut..] {
        out.push(c.as_os_str());
    }
    out.to_string_lossy().to_lowercase()
}

/// 敏感路径守卫（审计 M1/M2）：拒绝“另存为/附件添加”目标指向本应用关键
/// 数据文件——数据库（corpmail.db 及 wal/shm）、凭据回退密钥（corpmail.key）、
/// 配置（corpmail.toml）及它们的原子写临时文件。
///
/// 背景：dest/attach_paths 是表单值，客户端可改（合法值来自系统对话框）。
/// 一旦上游净化器或 CSP 被击穿出现脚本执行，这两个入口就是“写坏数据库/
/// 密钥/配置”与“读出密钥+数据库外泄 keyfile 密码”的现成原语。本守卫独立
/// 于前端与净化防线，属纵深防御。精确到文件级，不拦目录。
pub fn guard_sensitive_path(state: &AppState, raw: &str) -> Result<(), String> {
    let target = comparable_path(Path::new(raw));
    let data = state.data_dir();
    let anchor = state.anchor_dir().to_path_buf();
    let hit = ["corpmail.db", "corpmail.db-wal", "corpmail.db-shm"]
        .iter()
        .any(|n| comparable_path(&data.join(n)) == target)
        || [
            "corpmail.key",
            "corpmail.key.tmp",
            "corpmail.toml",
            "corpmail.toml.tmp",
        ]
        .iter()
        .any(|n| comparable_path(&anchor.join(n)) == target);
    if hit {
        return Err(format!(
            "目标路径属于应用数据文件（数据库/密钥/配置），已拒绝：{raw}"
        ));
    }
    Ok(())
}

/// 附件元数据 + 字节（另存为/预览；detached 或文件缺失 → None）。
/// 归属校验：email_id 与 attach_id 联合匹配——URL 里的邮件 id 必须是附件
/// 真实属主，防 /emails/{other}/attachments/{aid} 跨邮件取附件（审计 L1）
pub fn attachment_bytes(
    state: &AppState,
    email_id: i64,
    attach_id: i64,
) -> Option<(AttachView, Vec<u8>)> {
    let root = attach_root(state);
    state.with_conn(|conn| {
        let row = conn
            .query_row(
                "SELECT filename, storage, COALESCE(file_path,''), data, detached = 1,
                        COALESCE(mime, 'application/octet-stream')
                 FROM attachments WHERE id = ?1 AND email_id = ?2",
                [attach_id, email_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<Vec<u8>>>(3)?,
                        r.get::<_, bool>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()
            .ok()??;
        let (filename, storage, file_path, data, detached, mime) = row;
        if detached {
            return None; // 大附件文件已按策略清理（§5.1）
        }
        let gz = match storage.as_str() {
            "inline" => data?,
            _ => std::fs::read(root.join(&file_path)).ok()?,
        };
        let bytes = gunzip(&gz)?;
        let meta = AttachView {
            id: attach_id,
            preview_kind: preview_kind_of(&mime, &filename).into(),
            filename,
            mime,
            size_orig: bytes.len() as i64,
            storage,
            detached,
        };
        Some((meta, bytes))
    })
}

/// 另存为：解压落盘到用户选择的路径（前端 __TAURI__.dialog 选好 dest 后 POST）。
/// dest 属表单值（客户端可改），先过敏感路径守卫（M1：防覆盖库/密钥/配置）
pub fn save_attachment_to(
    state: &AppState,
    email_id: i64,
    attach_id: i64,
    dest: &str,
) -> Result<(), String> {
    guard_sensitive_path(state, dest)?;
    let (_, bytes) = attachment_bytes(state, email_id, attach_id)
        .ok_or_else(|| "附件文件已被清理或不存在".to_string())?;
    std::fs::write(dest, &bytes).map_err(|err| format!("另存失败：{err}"))
}

/// 全部另存（A4）：把邮件所有未清理的附件解压落盘到用户选择的目录。
/// 返回 (saved, skipped)；skipped = 已被策略清理/数据缺失的条数（不阻断其他文件）。
/// 同名不覆盖：磁盘已存在或本批已用过时追加 “ (2)” 序号，避免静默覆盖用户文件。
pub fn save_all_attachments(
    state: &AppState,
    email_id: i64,
    dest_dir: &str,
) -> Result<(usize, usize), String> {
    let dir = Path::new(dest_dir);
    if !dir.is_dir() {
        return Err("目标目录不存在".to_string());
    }
    let root = attach_root(state);
    let rows: Vec<AttachRow> = state
        .with_conn(|conn| {
            crate::db::collect_rows(
                conn,
                "SELECT filename, COALESCE(mime,'application/octet-stream'), storage,
                    file_path, data, detached = 1
             FROM attachments WHERE email_id = ?1 ORDER BY id",
                &[&email_id],
                attach_row_from,
            )
        })
        .map_err(e)?;
    if rows.is_empty() {
        return Err("该邮件没有附件".to_string());
    }
    let mut saved = 0usize;
    let mut skipped = 0usize;
    let mut used: Vec<String> = Vec::new();
    for (filename, _mime, storage, file_path, data, detached) in rows {
        if detached {
            skipped += 1;
            continue;
        }
        let gz = match storage.as_str() {
            "inline" => match data {
                Some(d) => d,
                None => {
                    skipped += 1;
                    continue;
                }
            },
            _ => match file_path.and_then(|p| std::fs::read(root.join(p)).ok()) {
                Some(b) => b,
                None => {
                    skipped += 1;
                    continue;
                }
            },
        };
        let Some(bytes) = gunzip(&gz) else {
            skipped += 1;
            continue;
        };
        // 文件名只取 basename（历史脏数据/恶意名不得穿越目录）
        let base = Path::new(&filename)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty() && *s != "." && *s != "..")
            .unwrap_or("attachment");
        let name = unique_file_name(dir, base, &used);
        used.push(name.clone());
        let target = dir.join(&name);
        guard_sensitive_path(state, &target.to_string_lossy())?;
        std::fs::write(&target, &bytes).map_err(|err| format!("写入「{name}」失败：{err}"))?;
        saved += 1;
    }
    Ok((saved, skipped))
}

/// 目录内去重名：`报告.pdf` → `报告 (2).pdf` → `报告 (3).pdf`。
/// 同时检查磁盘已有文件与本批用过的名字（不静默覆盖）。
fn unique_file_name(dir: &Path, filename: &str, used: &[String]) -> String {
    let taken = |name: &str| used.iter().any(|u| u == name) || dir.join(name).exists();
    if !taken(filename) {
        return filename.to_string();
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() => (s, format!(".{e}")),
        _ => (filename, String::new()),
    };
    for n in 2..10_000 {
        let cand = format!("{stem} ({n}){ext}");
        if !taken(&cand) {
            return cand;
        }
    }
    format!("{stem}-{}{ext}", used.len() + 1)
}

/// 复制源邮件的全部附件到目标邮件（转发用；附件独立成行、独立文件，互不影响）。
///
/// 读取源附件（inline BLOB / 独立文件）→ 解压 → 按目标邮件的上限规则重新入库。
/// 返回复制数量；源附件已 detached（文件被清理）则跳过该条（转发不因一个附件整体失败，
/// 但调用方应提示用户核对）。detached 数量通过 Ok 返回值中的第二个元素给调用方。
/// 附件读取行：(filename, mime, storage, file_path, data, detached)
type AttachRow = (
    String,
    String,
    String,
    Option<String>,
    Option<Vec<u8>>,
    bool,
);

fn attach_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<AttachRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

pub fn copy_attachments(
    state: &AppState,
    from_id: i64,
    to_id: i64,
) -> Result<(usize, usize), String> {
    let root = attach_root(state);
    let limit = inline_limit(state);
    let month = month_dir_of(state, to_id);
    // 源附件快照：文件名/类型/存储/路径/数据（本连接内取完即释放锁）
    let rows: Vec<AttachRow> = state
        .with_conn(|conn| {
            crate::db::collect_rows(
                conn,
                "SELECT filename, COALESCE(mime,'application/octet-stream'), storage,
                    file_path, data, detached = 1
             FROM attachments WHERE email_id = ?1 ORDER BY id",
                &[&from_id],
                attach_row_from,
            )
        })
        .map_err(e)?;
    let mut copied = 0usize;
    let mut skipped = 0usize;
    for (filename, mime, storage, file_path, data, detached) in rows {
        let gz = if detached {
            skipped += 1;
            continue;
        } else if storage == "inline" {
            match data {
                Some(d) => d,
                None => {
                    skipped += 1;
                    continue;
                }
            }
        } else {
            match std::fs::read(root.join(file_path.unwrap_or_default())) {
                Ok(gz) => gz,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            }
        };
        let Some(bytes) = gunzip(&gz) else {
            skipped += 1;
            continue;
        };
        state.with_conn(|conn| -> Result<(), String> {
            let tx = conn.transaction().map_err(e)?;
            let mut written: Vec<PathBuf> = Vec::new();
            let res = insert_attachment(
                &AttachCtx {
                    tx: &tx,
                    root: &root,
                    month: &month,
                    email_id: to_id,
                    limit,
                },
                &filename,
                &mime,
                &bytes,
                &mut written,
            );
            if let Err(err) = res {
                drop(tx);
                for f in &written {
                    let _ = std::fs::remove_file(f);
                }
                return Err(err);
            }
            sync_attach_stats(&tx, to_id)?;
            tx.commit().map_err(e)
        })?;
        copied += 1;
    }
    Ok((copied, skipped))
}

/// 出站附件载荷（发送路径用：解压后的原始字节，交 lettre 编码为 multipart）
#[derive(Clone, Debug)]
pub struct SendAttachment {
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// 读取待发邮件的全部附件（发送前置校验，独立连接版）。
///
/// 语义：任何一个附件不可用即整体报错——**绝不"少发一个附件"静默成功**，
/// 静默丢附件比发送失败更伤用户信任（审计 P0-2）。detached（清理器已删文件）
/// 明确要求重新添加；记录损坏/文件缺失同样阻断并指出具体附件名。
pub fn outgoing_attachments(
    conn: &rusqlite::Connection,
    root: &Path,
    email_id: i64,
) -> Result<Vec<SendAttachment>, String> {
    let rows: Vec<AttachRow> = crate::db::collect_rows(
        conn,
        "SELECT filename, COALESCE(mime,'application/octet-stream'), storage,
                file_path, data, detached = 1
         FROM attachments WHERE email_id = ?1 ORDER BY id",
        &[&email_id],
        attach_row_from,
    )
    .map_err(|err| format!("读取附件清单失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (filename, mime, storage, file_path, data, detached) in rows {
        if detached {
            return Err(format!(
                "附件「{filename}」的文件已按清理策略删除，无法随本次邮件发送；请移除后重新添加"
            ));
        }
        let gz = match storage.as_str() {
            "inline" => {
                data.ok_or_else(|| format!("附件「{filename}」数据缺失（inline 记录为空）"))?
            }
            _ => std::fs::read(root.join(file_path.unwrap_or_default()))
                .map_err(|err| format!("附件「{filename}」读取失败：{err}"))?,
        };
        let bytes = gunzip(&gz).ok_or_else(|| format!("附件「{filename}」数据损坏（解压失败）"))?;
        out.push(SendAttachment {
            filename,
            mime,
            bytes,
        });
    }
    Ok(out)
}

// ---------- 内部辅助 ----------

/// 附件独立文件的月份目录（§5.1：邮件时间所在月）
fn month_dir_conn(conn: &rusqlite::Connection, email_id: i64) -> String {
    let epoch: i64 = conn
        .query_row(
            "SELECT COALESCE(received_at, sent_at, created_at) FROM emails WHERE id = ?1",
            [email_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    zoned_of(epoch)
        .map(|z| format!("{:04}-{:02}", z.year(), z.month() as u32))
        .unwrap_or_else(|| "unknown".into())
}

/// 附件独立文件的月份目录（§5.1：邮件时间所在月；AppState 薄封装）
fn month_dir_of(state: &AppState, email_id: i64) -> String {
    state.with_conn(|conn| month_dir_conn(conn, email_id))
}

fn mime_of(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    match lower.rsplit('.').next() {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("txt") => "text/plain",
        Some("zip") => "application/zip",
        Some("doc") | Some("docx") => "application/msword",
        Some("xls") | Some("xlsx") => "application/vnd.ms-excel",
        _ => "application/octet-stream",
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------- 单元测试 ----------

#[cfg(test)]
#[path = "../tests/mail.rs"]
mod tests;
