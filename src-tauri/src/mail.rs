//! 邮件存储层（M2，设计 §3/§4.2/§5.1/§5.3）
//!
//! - emails/attachments 表业务读写；FTS 一致性由 AI/AD/AU 触发器保证，
//!   写入方职责仅是先分词落影子列（subject_tokens/body_tokens，§4.1）
//! - 附件两级存储（§5.1）：`size_orig ≤ inline_attach_max_mb`（默认 20MB）→ gzip 入库 BLOB；
//!   超限 → gzip 独立文件 `attachments/{YYYY-MM}/{email_id}/{n}_{safe}.gz`（按月分目录）
//! - 删除为硬删无回收站（§5.3 确认项 D）：emails 行（触发器连带 FTS 行、外键级联附件行）
//!   + 独立附件文件（应用层清理，空目录一并回收）
//! - 列表分页 50/页（§8.3）；时间展示 label：今天 HH:MM / 同年 M月D日 / 跨年 YYYY-M-D

use crate::state::{AppState, defaults, keys};
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

/// jieba 精确模式预分词：空格拼接（空文本返回空串，FTS 无 term 亦合法）
pub fn tokenize(text: &str) -> String {
    JIEBA
        .cut(text, true)
        .iter()
        .map(|t| t.word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
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
                let name: String = r.get(5)?;
                let addr: String = r.get(4)?;
                Ok(EmailSummary {
                    id: r.get(0)?,
                    folder: r.get(1)?,
                    status: r.get(2)?,
                    subject: r.get(3)?,
                    from_display: if name.is_empty() { addr } else { name },
                    to_addrs: r.get(6)?,
                    body_preview: preview(&r.get::<_, String>(7)?),
                    attach_cnt: r.get(8)?,
                    unread: r.get::<_, i64>(9)? == 1,
                    time_label: time_label(r.get(10)?),
                })
            })
            .map_err(e)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(e)?;
        Ok((items, total))
    })
}

/// 正文摘要：压缩空白后取前 100 字符（chars 截断防中文 panic）
fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(100).collect()
}

/// 附件行（详情页/另存；不携带 BLOB 本体）
#[derive(Serialize)]
pub struct AttachView {
    pub id: i64,
    pub filename: String,
    pub mime: String,
    pub size_orig: i64,
    pub storage: String,
    pub detached: bool,
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
            Ok(AttachView {
                id: r.get(0)?,
                filename: r.get(1)?,
                mime: r.get(2)?,
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

/// 回收空目录（崩溃残留防护的轻量版；孤儿文件全量回收为 M5 清理器职责）
fn prune_empty_dirs(root: &Path) {
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
    state.with_conn(|conn| {
        let tx = conn.transaction().map_err(e)?;
        let mut count = 0usize;
        for p in paths {
            let orig = std::fs::read(p).map_err(|err| format!("读取附件失败 {}: {err}", safe_filename(p)))?;
            if orig.is_empty() {
                return Err("附件为空文件".into());
            }
            let gz = gzip(&orig).map_err(e)?;
            let sha = hex_sha256(&orig);
            let filename = std::path::Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .map(safe_filename)
                .unwrap_or_else(|| "attachment".into());
            let size_orig = orig.len() as i64;
            let n: i64 = tx
                .query_row(
                    "SELECT count(*) FROM attachments WHERE email_id = ?1",
                    [email_id],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(e)?
                + 1;
            if orig.len() as u64 <= limit {
                tx.execute(
                    "INSERT INTO attachments(email_id, filename, mime, size_orig, size_gz,
                                             storage, data, sha256)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'inline', ?6, ?7)",
                    rusqlite::params![
                        email_id,
                        filename,
                        mime_of(&filename),
                        size_orig,
                        gz.len() as i64,
                        gz,
                        sha,
                    ],
                )
                .map_err(e)?;
            } else {
                let rel = format!("{month}/{email_id}/{n}_{}.gz", filename);
                let dest = root.join(&rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|err| format!("创建附件目录失败：{err}"))?;
                }
                std::fs::write(&dest, &gz).map_err(|err| format!("写入附件文件失败：{err}"))?;
                written_files.push(dest);
                tx.execute(
                    "INSERT INTO attachments(email_id, filename, mime, size_orig, size_gz,
                                             storage, file_path, sha256)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'file', ?6, ?7)",
                    rusqlite::params![
                        email_id,
                        filename,
                        mime_of(&filename),
                        size_orig,
                        gz.len() as i64,
                        rel,
                        sha,
                    ],
                )
                .map_err(e)?;
            }
            count += 1;
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
        Ok(count)
    })
    .inspect_err(|_| {
        // 事务失败：清理本次已写盘的独立文件（防残留）
        for f in &written_files {
            let _ = std::fs::remove_file(f);
        }
    })
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

/// 附件元数据 + 字节（另存为；detached 或文件缺失 → None）
pub fn attachment_bytes(state: &AppState, attach_id: i64) -> Option<(AttachView, Vec<u8>)> {
    let root = attach_root(state);
    state.with_conn(|conn| {
        let row = conn
            .query_row(
                "SELECT filename, storage, COALESCE(file_path,''), data, detached = 1
                 FROM attachments WHERE id = ?1",
                [attach_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<Vec<u8>>>(3)?,
                        r.get::<_, bool>(4)?,
                    ))
                },
            )
            .optional()
            .ok()??;
        let (filename, storage, file_path, data, detached) = row;
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
            filename,
            mime: String::new(),
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
