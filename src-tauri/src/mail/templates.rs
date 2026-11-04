//! 邮件模板 / 快捷短语（B2，schema v10 `templates` 表）
//!
//! - 设置页「模板」tab 维护：名称 + 纯文本内容 + 可选 HTML 内容
//! - 撰写页「插入模板」下拉：纯文本模式插入 body_text，富文本模式优先插
//!   body_html（空则把 body_text 转义成段落）
//! - 内容长度设上限（100KB/字段）：模板是手写短语，正常远小于此；防误贴大段
//!   HTML 撑大数据库与渲染开销

use crate::state::AppState;
use rusqlite::OptionalExtension;
use serde::Serialize;

/// 单条模板（preview 为列表展示用的纯文本摘录）
#[derive(Serialize, Clone)]
pub struct Template {
    pub id: i64,
    pub name: String,
    pub body_text: String,
    pub body_html: String,
    pub preview: String,
}

/// 列表展示摘录：取纯文本前 80 字符，换行折叠为空格
fn preview_of(text: &str) -> String {
    let line: String = text.chars().take(80).collect();
    let line = line.replace(['\r', '\n'], " ");
    let trimmed = line.trim();
    if text.chars().count() > 80 {
        format!("{trimmed}…")
    } else {
        trimmed.to_string()
    }
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Template> {
    let body_text: String = r.get(2)?;
    Ok(Template {
        id: r.get(0)?,
        name: r.get(1)?,
        preview: preview_of(&body_text),
        body_text,
        body_html: r.get(3)?,
    })
}

/// 全部模板（最近更新在前）
pub fn list(state: &AppState) -> Vec<Template> {
    state
        .with_conn(|conn| {
            crate::db::collect_rows(
                conn,
                "SELECT id, name, body_text, body_html FROM templates
                 ORDER BY updated_at DESC, id DESC",
                &[],
                map_row,
            )
        })
        .unwrap_or_default()
}

/// 单条模板（编辑回显/测试用；不存在或非法 id → None）
#[cfg_attr(not(test), allow(dead_code))]
pub fn get(state: &AppState, id: i64) -> Option<Template> {
    state
        .with_conn(|conn| {
            conn.query_row(
                "SELECT id, name, body_text, body_html FROM templates WHERE id = ?1",
                [id],
                map_row,
            )
            .optional()
        })
        .ok()?
}

/// 保存（id=None 新增，否则覆盖）。返回模板 id。
pub fn save(
    state: &AppState,
    id: Option<i64>,
    name: &str,
    body_text: &str,
    body_html: &str,
) -> Result<i64, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("模板名称不能为空".into());
    }
    if name.chars().count() > 60 {
        return Err("模板名称最多 60 个字符".into());
    }
    if body_text.trim().is_empty() && body_html.trim().is_empty() {
        return Err("模板内容不能为空".into());
    }
    const MAX_BODY: usize = 100 * 1024;
    if body_text.len() > MAX_BODY || body_html.len() > MAX_BODY {
        return Err("模板内容过长（单字段上限 100KB）".into());
    }
    let now = super::now_epoch();
    state.with_conn(|conn| match id.filter(|v| *v > 0) {
        Some(id) => {
            let n = conn
                .execute(
                    "UPDATE templates SET name = ?2, body_text = ?3, body_html = ?4,
                                    updated_at = ?5 WHERE id = ?1",
                    rusqlite::params![id, name, body_text, body_html, now],
                )
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("模板不存在（可能已被删除）".to_string());
            }
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO templates(name, body_text, body_html, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![name, body_text, body_html, now],
            )
            .map_err(|e| e.to_string())?;
            Ok(conn.last_insert_rowid())
        }
    })
}

/// 删除（幂等：不存在也返回 Ok）
pub fn delete(state: &AppState, id: i64) -> Result<(), String> {
    state
        .with_conn(|conn| {
            conn.execute("DELETE FROM templates WHERE id = ?1", [id])
                .map(|_| ())
        })
        .map_err(|e| e.to_string())
}
