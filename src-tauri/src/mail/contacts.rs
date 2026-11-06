//! 联系人库（v4）：姓名 + 邮件地址，随收/发件自动采集，撰写页模糊匹配。
//!
//! - 采集时机：收件入库（From/To/Cc）、发送成功（To/Cc）
//! - 合并策略：addr 主键 NOCASE；新值为空不覆盖旧姓名；命中次数累加、updated_at 刷新
//! - 匹配：姓名或地址包含查询串（大小写不敏感），精确地址命中优先、再按最近使用排序
//! - 回填：contacts 为空时从历史 emails 一次性提取（幂等，仅首启执行一次）

use rusqlite::Connection;

/// 联系人条目（建议列表返回）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub addr: String,
    pub name: String,
}

/// 解析地址串为 (地址, 姓名) 列表。
///
/// 支持常见书写形式：
/// - `a@x.com`
/// - `张三 <zhang@x.com>`
/// - `"张三" <zhang@x.com>`
/// - 逗号/分号分隔的多个地址（`;` 同时是组分隔符，按分隔符切开即可覆盖）
///
/// 非法片段（无 `@`）静默丢弃；地址统一去空白。
pub fn parse_addr_list(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in raw.split([',', ';']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let (Some(lt), Some(gt)) = (part.rfind('<'), part.rfind('>'))
            && lt < gt
        {
            let addr = part[lt + 1..gt].trim().trim_matches('"').trim();
            let name = part[..lt].trim().trim_matches('"').trim();
            if addr.contains('@') {
                out.push((addr.to_string(), name.to_string()));
            }
        } else {
            let addr = part.trim_matches('"').trim();
            if addr.contains('@') {
                out.push((addr.to_string(), String::new()));
            }
        }
    }
    out
}

/// 写入/合并一条联系人（事务内或独立连接均可）。
///
/// 空地址忽略；已有记录时：新姓名非空才覆盖（避免后续只带地址的邮件清掉姓名），
/// hits 累加、updated_at 刷新。
pub fn upsert_conn(conn: &Connection, addr: &str, name: &str, now: i64) -> rusqlite::Result<()> {
    let addr = addr.trim();
    if addr.is_empty() || !addr.contains('@') {
        return Ok(());
    }
    let name = name.trim();
    conn.execute(
        "INSERT INTO contacts(addr, name, updated_at, hits) VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(addr) DO UPDATE SET
             name = CASE WHEN excluded.name <> '' THEN excluded.name ELSE contacts.name END,
             updated_at = excluded.updated_at,
             hits = contacts.hits + 1",
        rusqlite::params![addr, name, now],
    )?;
    Ok(())
}

/// 模糊匹配（撰写页收/抄送建议）。
///
/// `q` 为空返回最近使用的 8 条；否则姓名或地址包含查询串。
/// 排序：精确地址命中 → 地址前缀命中 → 最近使用 → 使用次数。
/// `%`/`_` 由调用方转义后传入（见 escape_like）。
pub fn suggest(conn: &Connection, q: &str, limit: usize) -> rusqlite::Result<Vec<Contact>> {
    let q = q.trim();
    let limit = limit as i64;
    if q.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT addr, name FROM contacts
             ORDER BY updated_at DESC, hits DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(Contact {
                addr: r.get(0)?,
                name: r.get(1)?,
            })
        })?;
        return rows.collect();
    }
    let pattern = format!("%{}%", escape_like(q));
    let prefix = format!("{}%", escape_like(q));
    let exact = q.to_ascii_lowercase();
    let mut stmt = conn.prepare(
        "SELECT addr, name FROM contacts
         WHERE addr LIKE ?1 ESCAPE '\\' OR name LIKE ?1 ESCAPE '\\'
         ORDER BY CASE
                    WHEN lower(addr) = ?2 THEN 0
                    WHEN addr LIKE ?3 ESCAPE '\\' THEN 1
                    ELSE 2
                  END,
                  updated_at DESC, hits DESC
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(rusqlite::params![pattern, exact, prefix, limit], |r| {
        Ok(Contact {
            addr: r.get(0)?,
            name: r.get(1)?,
        })
    })?;
    rows.collect()
}

/// LIKE 通配符转义（配合 ESCAPE '\'）
pub fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 联系人管理页条目（M3-3）：含最近联系时间戳与使用次数
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContactRow {
    pub addr: String,
    pub name: String,
    pub updated_at: i64,
    pub hits: i64,
}

/// 联系人排序键（批次四，与邮件列表排序下拉对齐）：
/// Recent = 最近联系优先（默认）、Hits = 联系最多、Name = 姓名。
/// ORDER BY 为静态字符串（枚举驱动，无注入面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactsSort {
    Recent,
    Hits,
    Name,
}

impl ContactsSort {
    /// URL/表单值归一：未知/缺省 → Recent（与 mail::SortKey::parse 同口径）
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("hits") => Self::Hits,
            Some("name") => Self::Name,
            _ => Self::Recent,
        }
    }

    /// 模板回显用的键名（选中态比较）
    pub fn key(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Hits => "hits",
            Self::Name => "name",
        }
    }

    fn order_by(self) -> &'static str {
        match self {
            Self::Recent => "updated_at DESC, hits DESC",
            Self::Hits => "hits DESC, updated_at DESC",
            Self::Name => "name ASC, addr ASC",
        }
    }
}

/// 联系人管理页列表（M3-3）：q 空返回全部，否则按姓名/地址模糊；
/// 排序由 ContactsSort 决定（默认最近联系倒序）。limit 上限由调用方控制。
pub fn list_rows(
    conn: &Connection,
    q: &str,
    limit: usize,
    sort: ContactsSort,
) -> rusqlite::Result<Vec<ContactRow>> {
    let limit = limit as i64;
    let order = sort.order_by();
    let t = q.trim();
    if t.is_empty() {
        let mut stmt = conn.prepare(&format!(
            "SELECT addr, name, updated_at, hits FROM contacts \
             ORDER BY {order} LIMIT ?1"
        ))?;
        return stmt
            .query_map([limit], contact_row)?
            .collect::<Result<Vec<_>, _>>();
    }
    let pattern = format!("%{}%", escape_like(t));
    let mut stmt = conn.prepare(&format!(
        "SELECT addr, name, updated_at, hits FROM contacts \
         WHERE addr LIKE ?1 ESCAPE '\\' OR name LIKE ?1 ESCAPE '\\' \
         ORDER BY {order} LIMIT ?2"
    ))?;
    stmt.query_map(rusqlite::params![pattern, limit], contact_row)?
        .collect::<Result<Vec<_>, _>>()
}

fn contact_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ContactRow> {
    Ok(ContactRow {
        addr: r.get(0)?,
        name: r.get(1)?,
        updated_at: r.get(2)?,
        hits: r.get(3)?,
    })
}

/// 删除联系人（M3-3）。返回是否真的删除（不存在 → false）。
/// 仅手动删除生效：随收/发件自动采集会在下次往来时重新建档（预期行为）。
pub fn remove(conn: &Connection, addr: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM contacts WHERE addr = ?1", [addr.trim()])?;
    Ok(n > 0)
}

/// 回填：contacts 为空且历史邮件存在时，从 emails 一次性提取。
///
/// 幂等：非空即跳过（首启执行一次；之后由收/发件增量采集维护）。
/// 覆盖收件（From/To/Cc）与本地已发送（To/Cc）。
pub fn backfill(conn: &mut Connection) -> rusqlite::Result<()> {
    let count: i64 = conn.query_row("SELECT count(*) FROM contacts", [], |r| r.get(0))?;
    if count > 0 {
        return Ok(());
    }
    let rows: Vec<(String, String, String, String)> = crate::db::collect_rows(
        conn,
        "SELECT from_addr, COALESCE(from_name,''), to_addrs, COALESCE(cc_addrs,'') FROM emails",
        &[],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if rows.is_empty() {
        return Ok(());
    }
    let now = super::now_epoch();
    let tx = conn.transaction()?;
    for (from_addr, from_name, to_addrs, cc_addrs) in rows {
        upsert_conn(&tx, &from_addr, &from_name, now)?;
        for (addr, name) in parse_addr_list(&to_addrs) {
            upsert_conn(&tx, &addr, &name, now)?;
        }
        for (addr, name) in parse_addr_list(&cc_addrs) {
            upsert_conn(&tx, &addr, &name, now)?;
        }
    }
    // 回填完成后修正计数：首次出现记为 1 即可，不做历史使用频次统计
    tx.execute("UPDATE contacts SET hits = 1", [])?;
    tx.commit()
}

#[cfg(test)]
#[path = "../tests/contacts.rs"]
mod tests;

/// 地址格式校验（B3 手动新增/编辑）：必须含 @、无空白、长度上限 254。
/// 只做"能作为邮件地址使用"的最低限度校验——联系人库不是协议解析器，
/// 真投递时 SMTP 会再校验；这里拦住明显的误输入（缺 @、带空格）。
pub fn validate_addr(addr: &str) -> Result<String, String> {
    let addr = addr.trim();
    if addr.is_empty() {
        return Err("邮件地址不能为空".into());
    }
    if addr.len() > 254 {
        return Err("邮件地址过长".into());
    }
    if addr.chars().any(char::is_whitespace) {
        return Err("邮件地址不能包含空格".into());
    }
    let Some((local, domain)) = addr.rsplit_once('@') else {
        return Err("邮件地址缺少 @".into());
    };
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return Err("邮件地址格式不正确".into());
    }
    Ok(addr.to_string())
}

/// 手动新增/编辑（B3）：已存在时只更新姓名（hits 与往来次数语义无关，
/// 不因手动编辑而累加）；新增时 hits=1。
pub fn save_manual(conn: &Connection, addr: &str, name: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO contacts(addr, name, updated_at, hits) VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(addr) DO UPDATE SET
             name = excluded.name,
             updated_at = excluded.updated_at",
        rusqlite::params![addr.trim(), name.trim(), now],
    )?;
    Ok(())
}
