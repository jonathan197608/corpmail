//! 邮件存储层（M2，设计 §3/§4.2/§5.1/§5.3）
//!
//! - emails/attachments 表业务读写；FTS 一致性由 AI/AD/AU 触发器保证，
//!   写入方职责仅是先分词落影子列（subject_tokens/body_tokens，§4.1）
//! - 附件两级存储（§5.1）：`size_orig ≤ inline_attach_max_mb`（默认 20MB）→ gzip 入库 BLOB；
//!   超限 → gzip 独立文件 `attachments/{YYYY-MM}/{email_id}/{n}_{safe}.gz`（按月分目录）
//! - 删除为硬删无回收站（§5.3 确认项 D）：emails 行（触发器连带 FTS 行、外键级联附件行）
//!   + 独立附件文件（应用层清理，空目录一并回收）
//! - 列表分页 50/页（§8.3）；时间展示 label：今天 HH:MM / 同年 M月D日 / 跨年 YYYY-M-D

use crate::state::{AppState, defaults, full_address, keys};
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

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

/// HTML 转义（展示层通用；router.rs 复用本实现，避免双份漂移）
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

fn zoned_of(epoch: i64) -> Option<jiff::Zoned> {
    let tz = jiff::Zoned::now().time_zone().clone();
    jiff::Timestamp::from_second(epoch)
        .ok()
        .map(|ts| ts.to_zoned(tz))
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
    let st = tokenize(&input.subject);
    let bt = tokenize(&input.body_text);
    let now = now_epoch();
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
                        in_reply_to=?10
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
                ],
            )
            .map_err(e)?;
            tx.commit().map_err(e)?;
            Ok(id)
        } else {
            tx.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    cc_addrs, subject, body_text, subject_tokens, body_tokens,
                                    unread, created_at, in_reply_to)
                 VALUES ('out', 'draft', 'draft', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10)",
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
    let where_sql = format!("WHERE 1=1{folder_sql}{unread_sql}");
    let page = q.page.max(1);
    let offset = (page - 1) * PAGE_SIZE;
    state.with_conn(|conn| {
        let total: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM emails {where_sql}"),
                [],
                |r| r.get(0),
            )
            .map_err(e)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, folder, status, subject, from_addr, from_name, to_addrs, body_text,
                        attach_cnt, unread,
                        COALESCE(received_at, sent_at, created_at) AS t
                 FROM emails {where_sql}
                 ORDER BY t DESC, id DESC
                 LIMIT ?1 OFFSET ?2"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map(rusqlite::params![PAGE_SIZE, offset], |r| {
                summary_from_row(r, None)
            })
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
    let page = q.page.max(1);
    let offset = (page - 1) * PAGE_SIZE;
    let words = tokenize_words(input);
    state.with_conn(|conn| {
        let total: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)
                     FROM email_fts JOIN emails e ON e.id = email_fts.rowid
                     WHERE email_fts MATCH ?1{folder_sql}{unread_sql}"
                ),
                rusqlite::params![match_expr],
                |r| r.get(0),
            )
            .map_err(e)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT e.id, e.folder, e.status, e.subject, e.from_addr, e.from_name,
                        e.to_addrs, e.body_text, e.attach_cnt, e.unread,
                        COALESCE(e.received_at, e.sent_at, e.created_at) AS t
                 FROM email_fts JOIN emails e ON e.id = email_fts.rowid
                 WHERE email_fts MATCH ?1{folder_sql}{unread_sql}
                 ORDER BY bm25(email_fts, 5.0, 1.0), e.id DESC
                 LIMIT ?2 OFFSET ?3"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map(rusqlite::params![match_expr, PAGE_SIZE, offset], |r| {
                summary_from_row(r, Some(&words))
            })
            .map_err(e)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(e)?;
        Ok(Some((items, total)))
    })
}

/// 正文摘要：压缩空白后取前 100 字符（chars 截断防中文 panic）
fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(100).collect()
}

/// 悬停预览正文：压缩空白 + 300 字符截断 + 超长补省略号（M5 列表 tooltip）
fn preview_tip(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut tip: String = flat.chars().take(300).collect();
    if flat.chars().count() > 300 {
        tip.push('…');
    }
    tip
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
/// （真实邮件附件头常不规范）；text/html 归入不可预览（内嵌渲染会执行脚本）
fn preview_kind_of(mime: &str, filename: &str) -> &'static str {
    let m = mime.to_ascii_lowercase();
    let by_mime = if m.starts_with("image/") {
        Some("img")
    } else if m == "application/pdf" {
        Some("pdf")
    } else if m.starts_with("text/") && m != "text/html" {
        Some("text")
    } else if m.is_empty() || m == "application/octet-stream" {
        None // 头部不可信，走扩展名兜底
    } else {
        Some("") // 明确不支持的类型，不再兜底
    };
    if let Some(kind) = by_mime {
        if !kind.is_empty() {
            return kind;
        }
        return "";
    }
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" => "img",
        "pdf" => "pdf",
        "txt" | "csv" | "log" | "json" | "xml" | "md" | "yml" | "yaml" | "ini" | "conf" | "sql"
        | "toml" | "ts" | "js" | "css" => "text",
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
    pub body_text: String,
    pub attach_cnt: i64,
    pub attach_bytes: i64,
    pub unread: bool,
    pub in_reply_to: Option<i64>,
    pub error: String,
    pub time_full: String,
    pub attachments: Vec<AttachView>,
}

pub fn get(state: &AppState, id: i64) -> Option<EmailDetail> {
    // 注意：本函数全程单次借锁——time epoch 并入主查询，勿在闭包内再调 with_conn
    state.with_conn(|conn| {
        let row = conn
            .query_row(
                "SELECT direction, folder, status, subject, from_addr, from_name, to_addrs,
                        cc_addrs, body_text, attach_cnt, attach_bytes, unread, in_reply_to,
                        COALESCE(error, ''), COALESCE(received_at, sent_at, created_at)
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
                        attach_cnt: r.get(9)?,
                        attach_bytes: r.get(10)?,
                        unread: r.get::<_, i64>(11)? == 1,
                        in_reply_to: r.get(12)?,
                        error: r.get(13)?,
                        time_full: time_full(r.get(14)?),
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

/// 本地标记已读（IMAP 服务器回写为 M4；§6.1）。返回是否生效
pub fn mark_read(state: &AppState, id: i64) -> bool {
    state.with_conn(|conn| {
        conn.execute(
            "UPDATE emails SET unread = 0 WHERE id = ?1 AND unread = 1",
            [id],
        )
        .map(|n| n == 1)
        .unwrap_or(false)
    })
}

// ---------- SMTP 发送（M3，设计 §2 发送队列 / §7 幂等 / §9 compose/send） ----------

/// 出站消息（lettre Message 的平台无关形态；注入测试可检视内容）
pub struct OutgoingMessage {
    pub from_addr: String,
    pub from_name: String,
    pub to_addrs: Vec<String>,
    pub cc_addrs: Vec<String>,
    pub subject: String,
    pub body_text: String,
}

/// SMTP 发送注入点（生产 lettre SMTPS；单测注入假实现，避免网络依赖——与 ConnTester 同模式）
pub trait MailSender {
    fn send(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        msg: &OutgoingMessage,
    ) -> Result<(), String>;
}

/// 生产实现：与 conncheck::smtp_test 同 transport 配置（relay + port + credentials，465 隐式 TLS）
pub struct RealSender;

impl MailSender for RealSender {
    #[allow(clippy::too_many_lines)]
    fn send(
        &self,
        host: &str,
        port: u16,
        account: &str,
        password: &str,
        msg: &OutgoingMessage,
    ) -> Result<(), String> {
        use lettre::Transport;
        use lettre::message::Mailbox;
        use lettre::transport::smtp::authentication::Credentials;
        let build = |raw: &str, name: &str| -> Result<Mailbox, String> {
            let addr = raw
                .trim()
                .parse()
                .map_err(|err| format!("邮箱地址无效 {raw}：{err}"))?;
            let name = name.trim();
            Ok(if name.is_empty() {
                Mailbox::new(None, addr)
            } else {
                Mailbox::new(Some(name.to_string()), addr)
            })
        };
        let mut builder = lettre::Message::builder().from(build(&msg.from_addr, &msg.from_name)?);
        for t in &msg.to_addrs {
            builder = builder.to(build(t, "")?);
        }
        for c in &msg.cc_addrs {
            builder = builder.cc(build(c, "")?);
        }
        let message = builder
            .subject(msg.subject.clone())
            .body(msg.body_text.clone())
            .map_err(|err| format!("构建邮件失败：{err}"))?;
        let transport = lettre::SmtpTransport::relay(host)
            .map_err(|err| format!("服务器地址无效：{err}"))?
            .port(port)
            .credentials(Credentials::new(account.to_string(), password.to_string()))
            .build();
        transport
            .send(&message)
            .map_err(|err| format!("发送失败：{err}"))
            .map(|_| ())
    }
}

/// 收件人/抄送串拆分（逗号或分号分隔，去空白）
fn split_addrs(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// 草稿 → 排队（原子 CAS：仅 status='draft' 可入队；重复提交 0 行返回 false，幂等 §7）
pub fn enqueue_draft(state: &AppState, id: i64) -> bool {
    state.with_conn(|conn| {
        conn.execute(
            "UPDATE emails SET status = 'queued', error = NULL WHERE id = ?1 AND status = 'draft'",
            [id],
        )
        .map(|n| n == 1)
        .unwrap_or(false)
    })
}

/// failed → draft 回编辑（§9 /emails/{id}/resend 重发入口；仅 failed 可转）
pub fn reopen_failed(state: &AppState, id: i64) -> bool {
    state.with_conn(|conn| {
        conn.execute(
            "UPDATE emails SET status = 'draft', folder = 'draft', error = NULL
             WHERE id = ?1 AND status = 'failed'",
            [id],
        )
        .map(|n| n == 1)
        .unwrap_or(false)
    })
}

/// 单封发送核心（发送 worker 与单测共用；基于独立连接，不持 AppState）。
///
/// 状态机（§3）：`queued →（CAS 抢占）sending → sent / failed`；
/// sent 终态同步迁 `folder='sent'` 并记 sent_at。失败原因写 error 列。
/// 返回 Ok(终态)（"sent"/"failed"）；未抢占到排队状态或邮件缺失返回 Err。
/// 发送参数快照（worker 每封派发前从配置生成；配置源 mailbox.toml，
/// 不再读库——验收拍板：DB 不保存 mailbox.toml 已有配置）
#[derive(Clone, Debug)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
}

impl SmtpConfig {
    /// 从配置快照生成（含默认值回退；端口非法回退 465）
    pub fn from_config(cfg: &crate::conf::ConfigMap) -> Self {
        let d = defaults();
        let get = |k: &str| -> String {
            cfg.get(k)
                .cloned()
                .or_else(|| {
                    d.iter()
                        .find(|(dk, _)| *dk == k)
                        .map(|(_, v)| (*v).to_string())
                })
                .unwrap_or_default()
        };
        Self {
            host: get(keys::SMTP_HOST),
            port: get(keys::SMTP_PORT).parse().unwrap_or(465),
        }
    }
}

pub fn dispatch_one(
    conn: &rusqlite::Connection,
    sender: &dyn MailSender,
    id: i64,
    smtp: &SmtpConfig,
) -> Result<&'static str, String> {
    // CAS 抢占：重复派发同一封（双击/崩溃重放）时 0 行，天然幂等
    let n = conn
        .execute(
            "UPDATE emails SET status = 'sending' WHERE id = ?1 AND status = 'queued'",
            [id],
        )
        .map_err(e)?;
    if n != 1 {
        return Err("邮件不在排队状态（可能已在发送）".into());
    }
    let mark = |status: &str, err: Option<&str>| -> Result<(), String> {
        let now = now_epoch();
        let err_v = err.unwrap_or("");
        conn.execute(
            "UPDATE emails SET status = ?2, error = ?3,
                    sent_at = CASE WHEN ?2 = 'sent' THEN ?4 ELSE sent_at END,
                    folder  = CASE WHEN ?2 = 'sent' THEN 'sent' ELSE folder END
             WHERE id = ?1",
            rusqlite::params![id, status, err_v, now],
        )
        .map(|_| ())
        .map_err(e)
    };
    let row = conn
        .query_row(
            "SELECT from_addr, COALESCE(from_name,''), to_addrs, COALESCE(cc_addrs,''),
                    subject, body_text
             FROM emails WHERE id = ?1",
            [id],
            |r| {
                Ok(OutgoingMessage {
                    from_addr: r.get(0)?,
                    from_name: r.get(1)?,
                    to_addrs: split_addrs(&r.get::<_, String>(2)?),
                    cc_addrs: split_addrs(&r.get::<_, String>(3)?),
                    subject: r.get(4)?,
                    body_text: r.get(5)?,
                })
            },
        )
        .map_err(|err| format!("读取待发邮件失败：{err}"))?;
    let msg = row;
    if msg.to_addrs.is_empty() {
        mark("failed", Some("收件人为空"))?;
        return Ok("failed");
    }
    // 发信参数：调用方快照（mailbox.toml）；密码仍走 conn（凭据与配置分离）
    let password = match crate::secrets::retrieve(conn, &msg.from_addr) {
        crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => p,
        crate::secrets::GetResult::Missing => {
            mark("failed", Some("凭据缺失，请在设置中重新保存密码"))?;
            return Ok("failed");
        }
    };
    match sender.send(&smtp.host, smtp.port, &msg.from_addr, &password, &msg) {
        Ok(()) => {
            mark("sent", None)?;
            Ok("sent")
        }
        Err(err) => {
            mark("failed", Some(&err))?;
            Ok("failed")
        }
    }
}

// ---------- IMAP 收件与未读回写（M4，设计 §6.1/§6.2） ----------

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
        // 增量范围：uidvalidity 未变且有游标 → uid+1 起续拉；否则按 SINCE 天数（§6.1）。
        // 范围写法必须是 "N:*"（UID 范围）；曾误写 "N,*"（set 语义 = UID 恰为 N 或最大值），
        // 部分服务器（如 Coremail）下增量结果不可靠 —— M5 实测修复。
        // 增量命中 0 封时再以 SINCE 兜底重查（个别服务器对 UID 范围的返回仍有 quirk；
        // 入库按 remote_uid/message_id 幂等去重，重复拉取无害）
        let uids: Vec<u32> = if last.uid > 0 && last.uidvalidity == uidvalidity {
            let inc: Vec<u32> = sess
                .uid_search(format!("{}:*", last.uid + 1))
                .map_err(|e| format!("检索新邮件失败：{e}"))?
                .into_iter()
                .filter(|u| *u > last.uid)
                .collect();
            if inc.is_empty() {
                let since = imap_since(first_days);
                eprintln!("[fetch] 增量命中 0 封，SINCE {since} 兜底重查");
                sess.uid_search(format!("SINCE {since}"))
                    .map_err(|e| format!("检索新邮件失败：{e}"))?
                    .into_iter()
                    .collect()
            } else {
                inc
            }
        } else {
            let since = imap_since(first_days);
            sess.uid_search(format!("SINCE {since}"))
                .map_err(|e| format!("检索新邮件失败：{e}"))?
                .into_iter()
                .collect()
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

/// HTML 剥标签（无 regex 依赖的手工实现）+ 基础实体解码
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                for t in chars.by_ref() {
                    if t == '>' {
                        break;
                    }
                }
            }
            '&' => {
                let mut entity = String::new();
                let mut terminated = false;
                for t in chars.by_ref() {
                    if t == ';' {
                        terminated = true;
                        break;
                    }
                    entity.push(t);
                    if entity.len() > 8 {
                        break;
                    }
                }
                if terminated {
                    match entity.as_str() {
                        "nbsp" => out.push(' '),
                        "lt" => out.push('<'),
                        "gt" => out.push('>'),
                        "amp" => out.push('&'),
                        "quot" => out.push('"'),
                        _ => out.push_str(&entity),
                    }
                } else {
                    out.push('&');
                    out.push_str(&entity);
                }
            }
            _ => out.push(c),
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
/// 单封失败中止整批（调用方以整体事务语义报告）；逐封独立提交更稳的取舍见 §6.1。
pub fn store_incoming_conn(
    conn: &mut rusqlite::Connection,
    data_dir: &Path,
    mails: &[IncomingMail],
) -> Result<usize, String> {
    let mut added = 0usize;
    for m in mails {
        let remote = format!("{}:{}", m.uidvalidity, m.uid);
        let st = tokenize(&m.subject);
        let bt = tokenize(&m.body_text);
        let now = now_epoch();
        let attach_bytes: i64 = m.attachments.iter().map(|a| a.data.len() as i64).sum();
        let dup_remote: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM emails WHERE remote_uid = ?1",
                [&remote],
                |r| r.get(0),
            )
            .optional()
            .map_err(e)?;
        if dup_remote.is_some() {
            continue;
        }
        if !m.message_id.is_empty() {
            let dup_mid: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM emails WHERE message_id = ?1",
                    [&m.message_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(e)?;
            if dup_mid.is_some() {
                continue;
            }
        }
        let unread = !m.seen;
        conn.execute(
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
        let id = conn.last_insert_rowid();
        added += 1;
        for a in &m.attachments {
            store_attachment_bytes_conn(conn, data_dir, id, &a.filename, &a.mime, &a.data)?;
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
    state.data_dir.join("attachments")
}

/// 附件根目录（独立连接版）
fn attach_root_of(data_dir: &Path) -> PathBuf {
    data_dir.join("attachments")
}

/// 附件入库上限（字节）：settings inline_attach_max_mb（默认 20MB；独立连接版）
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

/// 附件入库上限（字节）：settings inline_attach_max_mb（默认 20MB）
fn inline_limit(state: &AppState) -> u64 {
    let d = defaults();
    state
        .get_or(keys::INLINE_ATTACH_MAX_MB, &d)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20)
        .saturating_mul(1024 * 1024)
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

/// 单附件入库核心（事务内调用，add_attachments 与收件 store_attachment_bytes_conn 共用）：
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
        let rel = format!("{}/{}/{}_{}.gz", ctx.month, ctx.email_id, n, filename);
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
/// 任一文件读取失败则整体失败（事务回滚，不留半成品）。
pub fn add_attachments(state: &AppState, email_id: i64, paths: &[String]) -> Result<usize, String> {
    if paths.is_empty() {
        return Ok(0);
    }
    let limit = inline_limit(state);
    let root = attach_root(state);
    let month = month_dir_of(state, email_id);
    let mut written_files: Vec<PathBuf> = Vec::new();
    let result = state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        let mut count = 0usize;
        for p in paths {
            let orig = std::fs::read(p)
                .map_err(|err| format!("读取附件失败 {}: {err}", safe_filename(p)))?;
            if orig.is_empty() {
                return Err("附件为空文件".into());
            }
            let filename = std::path::Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .map(safe_filename)
                .unwrap_or_else(|| "attachment".into());
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
        Ok(count)
    });
    if result.is_err() {
        // 事务失败：清理本次已写盘的独立文件（防残留）
        for f in &written_files {
            let _ = std::fs::remove_file(f);
        }
    }
    result
}

/// 收件附件入库核心（独立连接版，M4 §6.1，不落临时文件）
pub fn store_attachment_bytes_conn(
    conn: &mut rusqlite::Connection,
    data_dir: &Path,
    email_id: i64,
    filename: &str,
    mime: &str,
    orig: &[u8],
) -> Result<(), String> {
    let limit = inline_limit_conn(conn);
    let root = attach_root_of(data_dir);
    let month = month_dir_conn(conn, email_id);
    let mut written_files: Vec<PathBuf> = Vec::new();
    let tx = conn.transaction().map_err(e)?;
    let result = (|| {
        insert_attachment(
            &AttachCtx {
                tx: &tx,
                root: &root,
                month: &month,
                email_id,
                limit,
            },
            &safe_filename(filename),
            mime,
            orig,
            &mut written_files,
        )?;
        sync_attach_stats(&tx, email_id)?;
        Ok(())
    })();
    match result {
        Ok(()) => tx.commit().map_err(e),
        Err(err) => {
            for f in &written_files {
                let _ = std::fs::remove_file(f);
            }
            Err(err)
        }
    }
}

/// 草稿重存时按 keep 列表裁剪附件（删除行 + 独立文件；attach_cnt/bytes 回写）
pub fn remove_attachments_except(
    state: &AppState,
    email_id: i64,
    keep: &[i64],
) -> Result<(), String> {
    let root = attach_root(state);
    state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        let mut stmt = tx
            .prepare("SELECT id, file_path FROM attachments WHERE email_id = ?1")
            .map_err(e)?;
        let victims: Vec<(i64, Option<String>)> = stmt
            .query_map([email_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(e)?
            .filter_map(Result::ok)
            .filter(|(id, _)| !keep.contains(id))
            .collect();
        drop(stmt);
        for (aid, rel) in &victims {
            tx.execute("DELETE FROM attachments WHERE id = ?1", [aid])
                .map_err(e)?;
            if let Some(rel) = rel {
                let _ = std::fs::remove_file(root.join(rel));
            }
        }
        tx.execute(
            "UPDATE emails SET
                 attach_cnt  = (SELECT count(*) FROM attachments WHERE email_id = ?1),
                 attach_bytes = (SELECT COALESCE(sum(size_orig), 0) FROM attachments WHERE email_id = ?1)
             WHERE id = ?1",
            [email_id],
        )
        .map_err(e)?;
        tx.commit().map_err(e)?;
        Ok(())
    })
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

// ---------- 内部辅助 ----------

/// 附件独立文件的月份目录（§5.1：邮件时间所在月；独立连接版）
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

/// 附件独立文件的月份目录（§5.1：邮件时间所在月；在 with_conn 外调用）
fn month_dir_of(state: &AppState, email_id: i64) -> String {
    let epoch: i64 = state.with_conn(|conn| {
        conn.query_row(
            "SELECT COALESCE(received_at, sent_at, created_at) FROM emails WHERE id = ?1",
            [email_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    });
    zoned_of(epoch)
        .map(|z| format!("{:04}-{:02}", z.year(), z.month() as u32))
        .unwrap_or_else(|| "unknown".into())
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
mod tests {
    use super::*;
    use crate::state::AppState;

    fn temp_state(tag: &str) -> (AppState, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mailbox-m2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (AppState::open(&dir), dir)
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn draft(subject: &str, body: &str) -> DraftInput {
        DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "单测用户".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            subject: subject.into(),
            body_text: body.into(),
            in_reply_to: None,
        }
    }

    #[test]
    fn tokenize_splits_chinese_into_space_joined_terms() {
        let t = tokenize("宽带提速配案清单");
        assert!(t.contains("宽带"), "分词应含两字词：{t}");
        assert!(t.contains("配案"), "分词应含领域词：{t}");
        assert!(t.split(' ').any(|w| w == "清单"), "分词应含完整词：{t}");
        assert_eq!(tokenize(""), "");
    }

    #[test]
    fn tokenize_words_keeps_ascii_compound_and_filters_stopwords() {
        let w = tokenize_words("SP-2026-1023 报价 v2.0 的了 the Invoice");
        assert!(
            w.contains(&"sp-2026-1023".to_string()),
            "编号整词保留并小写：{w:?}"
        );
        assert!(w.contains(&"v2.0".to_string()), "版本号整词：{w:?}");
        assert!(w.contains(&"invoice".to_string()), "英文小写化：{w:?}");
        assert!(w.iter().any(|x| x.contains("报价")), "中文词保留：{w:?}");
        assert!(
            !w.contains(&"的".to_string()) && !w.contains(&"了".to_string()),
            "中文停用词过滤：{w:?}"
        );
        assert!(!w.contains(&"the".to_string()), "英文停用词过滤：{w:?}");
        assert!(w.iter().all(|x| !x.contains(' ')), "词内无空白");
    }

    #[test]
    fn tokenize_words_drops_trailing_punctuation_from_ascii_words() {
        let w = tokenize_words("宽带提速，清单。 fox.");
        assert!(w.contains(&"fox".to_string()), "词尾句点不并入：{w:?}");
        assert!(!w.contains(&"fox.".to_string()), "{w:?}");
        assert!(w.contains(&"宽带".to_string()), "{w:?}");
        assert!(w.iter().any(|x| x == "清单"), "中文词不受影响：{w:?}");
        assert_eq!(
            build_query("SP-2026-1023"),
            Some("\"sp-2026-1023\"".to_string())
        );
        assert_eq!(build_query("的了 the"), None, "全停用词应回退");
        assert_eq!(build_query("   "), None, "纯空白应回退");
    }

    #[test]
    fn preview_tip_truncates_long_body_with_ellipsis() {
        assert_eq!(preview_tip("简短 正文"), "简短 正文", "短文原样保留");
        assert_eq!(preview_tip("a\r\n\t b"), "a b", "压缩空白");
        let long: String = "字".repeat(400);
        let tip = preview_tip(&long);
        assert_eq!(tip.chars().count(), 301, "300 字 + 省略号");
        assert!(tip.ends_with('…'), "超长以省略号收尾");
        assert_eq!(
            preview_tip(""),
            "",
            "空正文返回空串（空 data-tip 不触发气泡）"
        );
    }

    #[test]
    fn preview_kind_falls_back_to_extension_for_octet_stream() {
        // MIME 缺失/八进制流 → 扩展名兜底（真实邮件附件头常不规范）
        assert_eq!(
            preview_kind_of("application/octet-stream", "照片.png"),
            "img"
        );
        assert_eq!(preview_kind_of("", "报告.pdf"), "pdf");
        assert_eq!(
            preview_kind_of("application/octet-stream", "日志.log"),
            "text"
        );
        assert_eq!(preview_kind_of("application/octet-stream", "打包.zip"), "");
        // 明确不支持的 MIME 不兜底（防伪装扩展名绕过）
        assert_eq!(preview_kind_of("application/zip", "伪装.png"), "");
        // MIME 直接命中优先于扩展名
        assert_eq!(preview_kind_of("image/jpeg", "无扩展名"), "img");
        // text/html 拒绝内嵌（脚本执行风险）
        assert_eq!(preview_kind_of("text/html", "x.html"), "");
    }

    #[test]
    fn highlight_html_is_case_insensitive_and_escape_safe() {
        // ASCII 大小写不敏感 + 中文命中
        let out = highlight_html(
            "Invoice SP-2026 激励",
            &["invoice".into(), "激励".into(), "sp-2026".into()],
        );
        assert!(out.contains("<mark>Invoice</mark>"), "{out}");
        assert!(out.contains("<mark>激励</mark>"), "{out}");
        assert!(out.contains("<mark>SP-2026</mark>"), "{out}");
        // 转义实体内字母不误高亮（原文只有 &，无 amp）
        let out2 = highlight_html("A&B 报告", &["amp".into()]);
        assert!(!out2.contains("<mark>"), "原文无 amp 不应高亮：{out2}");
        // HTML 注入防御：原文先转义再包 mark
        let out3 = highlight_html("<script>x</script>", &["script".into()]);
        assert!(out3.contains("&lt;"), "原文已转义：{out3}");
        assert!(!out3.contains("<script"), "不得出现未转义标签：{out3}");
        // 重叠区间合并不产生嵌套 mark
        let out4 = highlight_html("激励激励", &["激励".into()]);
        assert_eq!(out4, "<mark>激励激励</mark>");
    }

    #[test]
    fn search_ranks_matches_and_highlights_subject() {
        let (state, dir) = temp_state("search");
        save_draft(&state, &draft("激励配案通知", "正文与关键词无关")).unwrap();
        save_draft(&state, &draft("别的主题", "完全无关正文")).unwrap();
        let q = ListQuery {
            folder: "all".into(),
            page: 1,
            unread_only: false,
        };
        let (items, total) = search(&state, "激励", &q).unwrap().unwrap();
        assert_eq!(total, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subject, "激励配案通知");
        assert!(
            items[0].subject_html.contains("<mark>激励</mark>"),
            "{}",
            items[0].subject_html
        );
        // 多词 AND：两词须命中同一封
        let (items2, _) = search(&state, "激励 配案", &q).unwrap().unwrap();
        assert_eq!(items2.len(), 1);
        // 输入全为停用词 → None 回退
        assert!(search(&state, "的了", &q).unwrap().is_none());
        // 无命中
        let (items3, total3) = search(&state, "zzqqxy", &q).unwrap().unwrap();
        assert_eq!(total3, 0);
        assert!(items3.is_empty());
        // folder 过滤：草稿 folder 内检索
        let q_inbox = ListQuery {
            folder: "inbox".into(),
            page: 1,
            unread_only: false,
        };
        let inbox_result = search(&state, "激励", &q_inbox).unwrap().unwrap();
        assert_eq!(inbox_result.1, 0, "inbox 无邮件");
        cleanup(&dir);
    }

    #[test]
    fn save_draft_creates_then_overwrites_with_fts_sync() {
        let (state, dir) = temp_state("draft");
        let id = save_draft(&state, &draft("激励 配案", "月租 单 元数据")).unwrap();
        // FTS 已入索引（影子列由触发器搬运）
        let hit: i64 = state.with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM email_fts WHERE email_fts MATCH '激励'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(hit, 1);
        // 覆盖
        let input = DraftInput {
            id: Some(id),
            ..draft("改题", "改文")
        };
        assert_eq!(save_draft(&state, &input).unwrap(), id);
        let total: i64 = state.with_conn(|c| {
            c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(total, 1, "覆盖不应新增行");
        // FTS 同步：旧词失效、新词命中
        let old_hit: i64 = state.with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM email_fts WHERE email_fts MATCH '激励'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(old_hit, 0, "更新后旧分词应出索引");
        cleanup(&dir);
    }

    #[test]
    fn save_draft_rejects_missing_id() {
        let (state, dir) = temp_state("draft-miss");
        let input = DraftInput {
            id: Some(999),
            ..draft("s", "b")
        };
        assert!(save_draft(&state, &input).is_err());
        cleanup(&dir);
    }

    #[test]
    fn list_filters_folder_and_pages() {
        let (state, dir) = temp_state("list");
        for i in 0..3 {
            save_draft(&state, &draft(&format!("草稿{i}"), "正文")).unwrap();
        }
        let (items, total) = list(
            &state,
            &ListQuery {
                folder: "draft".into(),
                page: 1,
                unread_only: false,
            },
        )
        .unwrap();
        assert_eq!((items.len() as i64, total), (3, 3));
        let (items, total) = list(
            &state,
            &ListQuery {
                folder: "inbox".into(),
                page: 1,
                unread_only: false,
            },
        )
        .unwrap();
        assert_eq!((items.len() as i64, total), (0, 0));
        let (items, _) = list(
            &state,
            &ListQuery {
                folder: "all".into(),
                page: 1,
                unread_only: true,
            },
        )
        .unwrap();
        assert_eq!(items.len(), 0, "草稿 unread 恒 0");
        // 预览非空且来自正文
        let (items, _) = list(
            &state,
            &ListQuery {
                folder: "draft".into(),
                page: 1,
                unread_only: false,
            },
        )
        .unwrap();
        assert!(items.iter().all(|i| i.body_preview == "正文"));
        assert_eq!(items[0].subject, "草稿2", "按时间倒序");
        cleanup(&dir);
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

    /// 假发送器：记录收到的参数与内容；fail=Some 时按给定原因失败
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
            account: &str,
            _password: &str,
            msg: &OutgoingMessage,
        ) -> Result<(), String> {
            self.seen.lock().unwrap().push((
                host.into(),
                port,
                account.into(),
                OutgoingMessage {
                    from_addr: msg.from_addr.clone(),
                    from_name: msg.from_name.clone(),
                    to_addrs: msg.to_addrs.clone(),
                    cc_addrs: msg.cc_addrs.clone(),
                    subject: msg.subject.clone(),
                    body_text: msg.body_text.clone(),
                },
            ));
            match self.fail {
                Some(r) => Err(r.into()),
                None => Ok(()),
            }
        }
    }

    /// 排队态邮件（发件人固定单测账号，可带收件人覆盖）
    fn queued_email(state: &AppState, to: &str) -> i64 {
        let input = DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: to.into(),
            cc_addrs: String::new(),
            subject: "月度激励汇总".into(),
            body_text: "请查收汇总表。".into(),
            in_reply_to: None,
        };
        let id = save_draft(state, &input).unwrap();
        assert!(enqueue_draft(state, id));
        id
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
        // 发送参数快照：从 mailbox.toml 内存镜像生成（配置不落库）
        let smtp = SmtpConfig::from_config(&state.config_snapshot());
        assert_eq!(smtp.host, "smtp.test.cn");
        let result = state.with_conn(|c| dispatch_one(c, &sender, id, &smtp));
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
    fn dispatch_failure_marks_failed_with_reason() {
        let (state, dir) = temp_state("m3-fail");
        state
            .store_password("demo@chinatelecom.cn", "pw-m3")
            .unwrap();
        let id = queued_email(&state, "peer@x.cn");
        let sender = FakeSender::failing("连接超时");
        let smtp = SmtpConfig::from_config(&state.config_snapshot());
        let result = state.with_conn(|c| dispatch_one(c, &sender, id, &smtp));
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
                .with_conn(|c| dispatch_one(c, &sender, id, &smtp))
                .is_err()
        );
        // 排队但收件人空白：转 failed（凭据存在仍先校验收件人）
        let blank_id = queued_email(&state, "   ");
        assert_eq!(
            state
                .with_conn(|c| dispatch_one(c, &sender, blank_id, &smtp))
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
                .with_conn(|c| dispatch_one(c, &sender2, id2, &smtp))
                .unwrap(),
            "sent"
        );
        assert!(
            state
                .with_conn(|c| dispatch_one(c, &sender2, id2, &smtp))
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
                .with_conn(|c| dispatch_one(c, &sender, id, &smtp))
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

    /// 假收件器：记录 mark_seen 调用参数（fetch 路径由真实 fetcher 单独覆盖，不测网络）
    type SeenCall = (String, u16, String, u32, u32);

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
            _account: &str,
            _password: &str,
            _first_days: i64,
            _last: LastCursor,
        ) -> Result<FetchOutcome, String> {
            Ok(FetchOutcome {
                uidvalidity: 0,
                max_uid: 0,
                mails: Vec::new(),
            })
        }

        fn mark_seen(
            &self,
            host: &str,
            port: u16,
            account: &str,
            _password: &str,
            uidvalidity: u32,
            uid: u32,
        ) -> Result<(), String> {
            self.seen_marks.lock().unwrap().push((
                host.into(),
                port,
                account.into(),
                uidvalidity,
                uid,
            ));
            Ok(())
        }
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
                calls[0].4
            ),
            ("imap.test.cn", 993, "demo@chinatelecom.cn", 42, 7),
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
        let draft_id = save_draft(&state, &draft("本地草稿", "体")).unwrap();
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
        assert!(parts[2].len() == 4, "年份四位：{s}");
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
    fn get_returns_detail_and_mark_read_clears_flag() {
        let (state, dir) = temp_state("detail");
        let id = save_draft(&state, &draft("详情", "内容体")).unwrap();
        let d = get(&state, id).unwrap();
        assert_eq!(d.subject, "详情");
        assert_eq!(d.folder, "draft");
        assert!(!d.time_full.is_empty(), "详情时间 label 应非空");
        // in 邮件 unread 语义
        state.with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                        subject_tokens, body_tokens, unread, created_at)
                     VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', '来件', '来 件',
                             '内 容', 1, 1)",
                [],
            )
            .unwrap();
        });
        let in_id = id + 1;
        assert!(get(&state, in_id).unwrap().unread);
        assert!(mark_read(&state, in_id));
        assert!(!get(&state, in_id).unwrap().unread);
        assert!(!mark_read(&state, in_id), "重复标记应无效果");
        assert!(get(&state, 404).is_none());
        cleanup(&dir);
    }

    #[test]
    fn delete_removes_rows_fts_and_files() {
        let (state, dir) = temp_state("del");
        let id = save_draft(&state, &draft("删除", "删除体")).unwrap();
        // 造两个临时附件文件（一个 inline 一个 file 需要 >20MB 太重；这里直接手插一行 file 引用真实文件）
        let root = dir.join("attachments").join("2026-10").join(id.to_string());
        std::fs::create_dir_all(&root).unwrap();
        let file_rel = format!("2026-10/{id}/1_proof.gz");
        std::fs::write(dir.join("attachments").join(&file_rel), b"gzip-bytes").unwrap();
        state.with_conn(|c| {
            c.execute(
                "INSERT INTO attachments(email_id, filename, size_orig, size_gz, storage,
                                             file_path, sha256)
                     VALUES (?1, 'proof.txt', 4, 10, 'file', ?2, 'deadbeef')",
                rusqlite::params![id, file_rel],
            )
            .unwrap();
        });
        assert!(delete(&state, id).unwrap());
        // 行与 FTS 与附件行全清
        let (mail_n, att_n, fts_n): (i64, i64, i64) = state.with_conn(|c| {
            (
                c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
                    .unwrap(),
                c.query_row("SELECT count(*) FROM attachments", [], |r| r.get(0))
                    .unwrap(),
                c.query_row("SELECT count(*) FROM email_fts", [], |r| r.get(0))
                    .unwrap(),
            )
        });
        assert_eq!((mail_n, att_n, fts_n), (0, 0, 0));
        assert!(
            !dir.join("attachments").join(&file_rel).exists(),
            "独立文件应删除"
        );
        assert!(!delete(&state, id).unwrap(), "重复删除返回 false");
        cleanup(&dir);
    }

    #[test]
    fn attachments_inline_and_file_tiers() {
        let (state, dir) = temp_state("attach");
        let id = save_draft(&state, &draft("附件", "附件体")).unwrap();
        // 小文件 → inline
        let small = dir.join("small.txt");
        std::fs::write(&small, b"hello attachment").unwrap();
        add_attachments(&state, id, &[small.to_str().unwrap().into()]).unwrap();
        let (storage,): (String,) = state.with_conn(|c| {
            c.query_row(
                "SELECT storage FROM attachments WHERE email_id = ?1",
                [id],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap()
        });
        assert_eq!(storage, "inline");
        let d = get(&state, id).unwrap();
        assert_eq!(d.attach_cnt, 1);
        assert_eq!(d.attach_bytes, 16);
        // 另存 roundtrip（gzip 解压回原文）
        let aid = d.attachments[0].id;
        let dest = dir.join("saved.txt");
        save_attachment_to(&state, aid, dest.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello attachment");
        // keep 裁剪
        remove_attachments_except(&state, id, &[]).unwrap();
        assert_eq!(get(&state, id).unwrap().attach_cnt, 0);
        cleanup(&dir);
    }

    #[test]
    fn add_attachments_rejects_missing_file_atomically() {
        let (state, dir) = temp_state("attach-miss");
        let id = save_draft(&state, &draft("附件缺", "体")).unwrap();
        let small = dir.join("ok.txt");
        std::fs::write(&small, b"data").unwrap();
        let err = add_attachments(
            &state,
            id,
            &[
                small.to_str().unwrap().into(),
                dir.join("nope.bin").to_str().unwrap().into(),
            ],
        )
        .unwrap_err();
        assert!(err.contains("读取附件失败"), "{err}");
        let n: i64 = state.with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM attachments WHERE email_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(n, 0, "整体失败应回滚不留半成品");
        cleanup(&dir);
    }
}
