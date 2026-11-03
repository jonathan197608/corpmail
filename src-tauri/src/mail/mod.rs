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
mod recv;
pub(crate) mod seen;
mod send;

pub use recv::{
    IncomingMail, LastCursor, MailDest, MailFetcher, RealFetcher, seen_params_of,
    store_incoming_conn,
};
// 仅单测使用的入口：生产收件走 fetch_batched + store_incoming_conn（避免为测试放宽生产可见性）
#[cfg(test)]
pub use recv::{FetchOutcome, store_incoming};
pub use send::{
    RealSender, SmtpConfig, dispatch_one, enqueue_draft, recover_interrupted_sends, reopen_failed,
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

/// 列表分页大小
pub const PAGE_SIZE: i64 = 50;

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
                    _ => Some(std::borrow::Cow::Borrowed(value)),
                }
            });
            b
        });
    SANITIZER.clean(raw).to_string()
}

/// 是否 http(s) 远程 URL（用于图片代理改写）
fn is_remote_http_url(value: &str) -> bool {
    let lower = value.trim_start().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
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
pub fn strip_html(html: &str) -> String {
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

/// 撰写表单入参（存草稿；发送为 M3）
pub struct DraftInput {
    /// 覆盖已有草稿的 id（None = 新建）
    pub id: Option<i64>,
    pub from_addr: String,
    pub from_name: String,
    pub to_addrs: String,
    pub cc_addrs: String,
    pub subject: String,
    pub body_text: String,
    /// 回复链：指向被回复邮件 id（§3 in_reply_to）
    pub in_reply_to: Option<i64>,
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
                        in_reply_to=?10, body_kind=?11, body_html=?12
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
                ],
            )
            .map_err(e)?;
            tx.commit().map_err(e)?;
            Ok(id)
        } else {
            tx.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    cc_addrs, subject, body_text, subject_tokens, body_tokens,
                                    unread, created_at, in_reply_to, body_kind, body_html)
                 VALUES ('out', 'draft', 'draft', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10, ?11, ?12)",
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

/// 列表查询（folder：inbox|sent|draft|all；unread 仅 inbox 有意义；page 从 1 起）
pub struct ListQuery {
    pub folder: String,
    pub page: i64,
    pub unread_only: bool,
    /// 只看外部发件人（收件箱「外部」筛选按钮）
    pub external_only: bool,
    /// 账号内部域（如 chinatelecom.cn；用于判定外部发件人，空 = 无内部域）
    pub internal_domain: String,
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
    /// 外部发件人（列表「外部」标记；口径见 is_external_addr）
    pub from_external: bool,
    pub time_label: String,
    /// 标题安全 HTML：列表为转义原文，检索命中时为 <mark> 高亮版（模板 |safe 输出）
    pub subject_html: String,
    /// 悬停预览原始正文（压缩空白 + 300 字符截断 + 省略号，模板 data-tip 属性用）
    pub preview_tip: String,
}

/// folder 白名单校验（未知值按 all 处理；SQL 内插前必须过白名单）
fn folder_of(raw: &str) -> &str {
    match raw {
        "inbox" | "sent" | "draft" | "all" => raw,
        _ => "all",
    }
}

/// 列表分页查询，返回 (当前页行, 命中总数)
pub fn list(state: &AppState, q: &ListQuery) -> Result<(Vec<EmailSummary>, i64), String> {
    let folder = folder_of(&q.folder);
    let folder_sql = if folder == "all" {
        String::new()
    } else {
        format!(" AND folder = '{folder}'")
    };
    let unread_sql = if q.unread_only { " AND unread = 1" } else { "" };
    let domain = q.internal_domain.trim();
    let ext_col = external_col("", domain);
    let ext_filter = if q.external_only { 1 } else { 0 };
    // :extf = 0 时整段条件恒真：无需为"是否过滤"拼两套 SQL
    let where_sql = format!("WHERE 1=1{folder_sql}{unread_sql} AND (:extf = 0 OR {ext_col})");
    let page = q.page.max(1);
    // 页码来自查询串：巨大值乘法需饱和（debug 构建溢出即 panic）
    let offset = (page - 1).saturating_mul(PAGE_SIZE);
    state.with_conn(|conn| {
        let total: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM emails {where_sql}"),
                rusqlite::named_params! { ":extf": ext_filter, ":dom": domain },
                |r| r.get(0),
            )
            .map_err(e)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, folder, status, subject, from_addr, from_name, to_addrs, body_text,
                        attach_cnt, unread,
                        COALESCE(received_at, sent_at, created_at) AS t,
                        {ext_col} AS from_external
                 FROM emails {where_sql}
                 ORDER BY t DESC, id DESC
                 LIMIT :lim OFFSET :off"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map(
                rusqlite::named_params! {
                    ":extf": ext_filter, ":dom": domain, ":lim": PAGE_SIZE, ":off": offset
                },
                |r| summary_from_row(r, None),
            )
            .map_err(e)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(e)?;
        Ok((items, total))
    })
}

/// 列表行映射（list/search 共用；列序：id,folder,status,subject,from_addr,from_name,
/// to_addrs,body_text,attach_cnt,unread,t）。highlight_words=Some 时标题做命中高亮
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
    let body: String = r.get(7)?;
    Ok(EmailSummary {
        id: r.get(0)?,
        folder: r.get(1)?,
        status: r.get(2)?,
        subject,
        from_display: if name.is_empty() { addr } else { name },
        to_addrs: r.get(6)?,
        body_preview: preview(&body),
        preview_tip: preview_tip(&body),
        attach_cnt: r.get(8)?,
        unread: r.get::<_, i64>(9)? == 1,
        from_external: r.get::<_, i64>(11)? == 1,
        time_label: time_label(r.get(10)?),
        subject_html,
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
    let folder = folder_of(&q.folder);
    let folder_sql = if folder == "all" {
        String::new()
    } else {
        format!(" AND e.folder = '{folder}'")
    };
    let unread_sql = if q.unread_only {
        " AND e.unread = 1"
    } else {
        ""
    };
    let domain = q.internal_domain.trim();
    let ext_col = external_col("e.", domain);
    let ext_filter = if q.external_only { 1 } else { 0 };
    let ext_sql = format!(" AND (:extf = 0 OR {ext_col})");
    let page = q.page.max(1);
    let offset = (page - 1).saturating_mul(PAGE_SIZE);
    let words = tokenize_words(input);
    state.with_conn(|conn| {
        let total: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)
                     FROM email_fts JOIN emails e ON e.id = email_fts.rowid
                     WHERE email_fts MATCH :match{folder_sql}{unread_sql}{ext_sql}"
                ),
                rusqlite::named_params! { ":match": match_expr, ":extf": ext_filter, ":dom": domain },
                |r| r.get(0),
            )
            .map_err(e)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT e.id, e.folder, e.status, e.subject, e.from_addr, e.from_name,
                        e.to_addrs, e.body_text, e.attach_cnt, e.unread,
                        COALESCE(e.received_at, e.sent_at, e.created_at) AS t,
                        {ext_col} AS from_external
                 FROM email_fts JOIN emails e ON e.id = email_fts.rowid
                 WHERE email_fts MATCH :match{folder_sql}{unread_sql}{ext_sql}
                 ORDER BY bm25(email_fts, 5.0, 1.0), e.id DESC
                 LIMIT :lim OFFSET :off"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map(
                rusqlite::named_params! {
                    ":match": match_expr, ":extf": ext_filter, ":dom": domain,
                    ":lim": PAGE_SIZE, ":off": offset
                },
                |r| summary_from_row(r, Some(&words)),
            )
            .map_err(e)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(e)?;
        Ok(Some((items, total)))
    })
}

/// 正文摘要：压缩空白后取前 100 字符（chars 截断防中文 panic）
fn preview(text: &str) -> String {
    flatten_ws(text).chars().take(100).collect()
}

/// 悬停预览正文：压缩空白 + 300 字符截断 + 超长补省略号（M5 列表 tooltip）
fn preview_tip(text: &str) -> String {
    let flat = flatten_ws(text);
    let mut tip: String = flat.chars().take(300).collect();
    if flat.chars().count() > 300 {
        tip.push('…');
    }
    tip
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
    "" // 明确不支持的类型，不再兜底
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
    pub body_text: String,
    /// 净化后的 HTML 正文（空串 = 无 HTML 版式，详情页回退纯文本）
    pub body_html: String,
    pub attach_cnt: i64,
    pub attach_bytes: i64,
    pub unread: bool,
    pub in_reply_to: Option<i64>,
    pub error: String,
    pub time_full: String,
    /// 超过单封收件上限：只拉了信头，正文为占位说明（详情页据此显示提示条）
    pub too_large: bool,
    pub attachments: Vec<AttachView>,
}

pub fn get(state: &AppState, id: i64) -> Option<EmailDetail> {
    // 注意：本函数全程单次借锁——time epoch 并入主查询，勿在闭包内再调 with_conn
    state.with_conn(|conn| {
        let row = conn
            .query_row(
                "SELECT direction, folder, status, subject, from_addr, from_name, to_addrs,
                        cc_addrs, body_text, COALESCE(body_html, ''), attach_cnt, attach_bytes,
                        unread, in_reply_to, COALESCE(error, ''),
                        COALESCE(received_at, sent_at, created_at), too_large
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
                        body_text: r.get(8)?,
                        body_html: r.get(9)?,
                        attach_cnt: r.get(10)?,
                        attach_bytes: r.get(11)?,
                        unread: r.get::<_, i64>(12)? == 1,
                        in_reply_to: r.get(13)?,
                        error: r.get(14)?,
                        time_full: time_full(r.get(15)?),
                        too_large: r.get::<_, i64>(16)? == 1,
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
fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
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

/// 附件元数据 + 字节（另存为/预览；detached 或文件缺失 → None）
pub fn attachment_bytes(state: &AppState, attach_id: i64) -> Option<(AttachView, Vec<u8>)> {
    let root = attach_root(state);
    state.with_conn(|conn| {
        let row = conn
            .query_row(
                "SELECT filename, storage, COALESCE(file_path,''), data, detached = 1,
                        COALESCE(mime, 'application/octet-stream')
                 FROM attachments WHERE id = ?1",
                [attach_id],
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

/// 另存为：解压落盘到用户选择的路径（前端 __TAURI__.dialog 选好 dest 后 POST）
pub fn save_attachment_to(state: &AppState, attach_id: i64, dest: &str) -> Result<(), String> {
    let (_, bytes) =
        attachment_bytes(state, attach_id).ok_or_else(|| "附件文件已被清理或不存在".to_string())?;
    std::fs::write(dest, &bytes).map_err(|err| format!("另存失败：{err}"))
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
