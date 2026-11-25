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
    // 第七轮 P3-3：与发送侧共用 `<...>` 感知切分——`Doe, John <john@x.com>`
    // 旧实现盲切逗号，姓名被污染成 "John"（发送正确但联系人库错）。
    for token in crate::mail::split_addr_tokens(raw) {
        let part = token.trim();
        if part.is_empty() {
            continue;
        }
        // R15 P2：邮件组语法 `Team: a@x.cn, b@x.cn;` 的首 token 含组名前缀，
        // 旧实现只判 contains('@') 就把 "Team: a@x.cn" 当完整地址返回 ——
        // addr_plausible 会因空白挡它入库，但撰写页 addr_chips 显示畸形 chip、
        // merge_addrs（回复全部）拼出 `Team: a@x.cn <...>` 畸形 To 头被 SMTP 拒。
        // 冒号前是无空白的组名时剥掉前缀（地址本地部分不允许裸冒号）。
        let part = if let Some((group, rest)) = part.split_once(':')
            && !group.contains(char::is_whitespace)
            && !group.is_empty()
            && rest.contains('@')
        {
            rest.trim()
        } else {
            part
        };
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

/// 自动采集地址的可入库判定（比 `validate_addr` 宽松——允许无点内网域名，
/// 但拦控制字符/空白/分隔符/尖括号/超长）。这些地址可能来自恶意邮件头，
/// 脏数据会污染建议列表与模糊匹配；显示层虽有 HTML 转义兜底，仍应入库前拦截。
fn addr_plausible(addr: &str) -> bool {
    addr.len() <= 254
        && addr
            .rsplit_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
        && !addr.chars().any(|c| {
            c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | '"' | ',' | ';' | '\'')
        })
}

/// 联系人姓名清洗：控制字符折叠为单个空格（连续 CR/LF 不产生连续空位），
/// 头尾控制不产空白——恶意邮件头的 CR/LF 不应进入建议列表与联系人页。
/// 复用 crate 级 collapse_control（与系统通知正文同一口径）。
/// R15 P2：长度上限下沉到此处（60 字符，按 char 边界截断），与手动保存
/// 路由层校验同口径——旧实现只对手动路径限 60，自动采集（收/发件显示名）
/// 可把数百字符的超长姓名直接入库，撑破联系人页/联想列表布局。
fn name_safe(name: &str) -> String {
    let collapsed = super::collapse_control(name);
    if collapsed.chars().count() <= 60 {
        return collapsed;
    }
    collapsed.chars().take(60).collect()
}

/// 写入/合并一条联系人（事务内或独立连接均可）。
///
/// 空地址/不可信地址忽略；已有记录时：新姓名非空才覆盖（避免后续只带地址的邮件清掉姓名），
/// hits 累加、updated_at 取新旧最大值——`at` 传入的是**邮件实际往来时间**，
/// 历史邮件/乱序收取（先收新信后收旧信）不得把"最近联系"往回拨。
pub fn upsert_conn(conn: &Connection, addr: &str, name: &str, at: i64) -> rusqlite::Result<()> {
    let addr = addr.trim();
    if !addr_plausible(addr) {
        return Ok(());
    }
    let name = name_safe(name.trim());
    conn.execute(
        "INSERT INTO contacts(addr, name, updated_at, hits) VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(addr) DO UPDATE SET
             name = CASE
                 WHEN contacts.manual_name = 1 THEN contacts.name
                 WHEN excluded.name <> '' THEN excluded.name
                 ELSE contacts.name END,
             updated_at = MAX(contacts.updated_at, excluded.updated_at),
             hits = contacts.hits + 1",
        rusqlite::params![addr, name, at],
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
    // R15 P2：Unicode 大小写折叠。旧实现用 to_ascii_lowercase + SQLite lower()
    // 都只折 ASCII，`MÜLLER` 查不到 `müller@x.cn`（WHERE 的 LIKE 也只折
    // ASCII，排序改了也没用）——与已修的高级筛选 from 口径不一致。
    // db::open 已注册 unicode_lower；WHERE 与排序两边都改才能真命中。
    let exact = q.to_lowercase();
    let upattern = format!("%{}%", escape_like(&q.to_lowercase()));
    let uprefix = format!("{}%", escape_like(&q.to_lowercase()));
    let mut stmt = conn.prepare(
        "SELECT addr, name FROM contacts
         WHERE unicode_lower(addr) LIKE ?1 ESCAPE '\\'
            OR unicode_lower(name) LIKE ?1 ESCAPE '\\'
         ORDER BY CASE
                    WHEN unicode_lower(addr) = ?2 THEN 0
                    WHEN unicode_lower(addr) LIKE ?3 ESCAPE '\\' THEN 1
                    ELSE 2
                  END,
                  updated_at DESC, hits DESC
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(rusqlite::params![upattern, exact, uprefix, limit], |r| {
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
/// Recent = 最近联系优先（默认）、Hits = 联系最多、Name = 姓名、Addr = 地址。
/// ORDER BY 为静态字符串（枚举驱动，无注入面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactsSort {
    Recent,
    Hits,
    Name,
    Addr,
}

impl ContactsSort {
    /// URL/表单值归一：未知/缺省 → Recent（与 mail::SortKey::parse 同口径）
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("hits") => Self::Hits,
            Some("name") => Self::Name,
            Some("addr") => Self::Addr,
            _ => Self::Recent,
        }
    }

    /// 模板回显用的键名（选中态比较）
    pub fn key(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Hits => "hits",
            Self::Name => "name",
            Self::Addr => "addr",
        }
    }

    fn order_by(self) -> &'static str {
        match self {
            Self::Recent => "updated_at DESC, hits DESC",
            Self::Hits => "hits DESC, updated_at DESC",
            Self::Name => "(name = '') ASC, name ASC, addr ASC",
            Self::Addr => "addr ASC",
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
    // R15 P2：按姓名排序时旧实现 WHERE name <> '' 把所有只从纯地址采集的
    // 联系人整批隐藏（total/分页同步缩水），切回"最近联系"又冒出来，用户
    // 以为联系人丢了。改为保留全部、用排序键 (name='') 让未命名沉底。
    let t = q.trim();
    if t.is_empty() {
        let mut stmt = conn.prepare(&format!(
            "SELECT addr, name, updated_at, hits FROM contacts ORDER BY {order} LIMIT ?1"
        ))?;
        return stmt
            .query_map([limit], contact_row)?
            .collect::<Result<Vec<_>, _>>();
    }
    // R16 P2：与 suggest(:144 unicode_lower) 同口径。SQLite 内建 LIKE 只折
    // ASCII，联系人页搜 MÜLLER 命中不到 müller@x.cn，而撰写页联想能命中——
    // 同一数据两个入口结论相反，用户以为联系人丢失。
    let pattern = format!("%{}%", escape_like(&t.to_lowercase()));
    let mut stmt = conn.prepare(&format!(
        "SELECT addr, name, updated_at, hits FROM contacts \
         WHERE unicode_lower(addr) LIKE ?1 ESCAPE '\\' \
            OR unicode_lower(name) LIKE ?1 ESCAPE '\\' \
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

/// 只采集不计数（收件 To/Cc 中的其他地址）：更新姓名/最近联系，hits 不变。
/// 与 upsert_conn 的分工：只有「直接往来」才累加往来次数（见 participants），
/// 群发/抄送名单里的人只入库供撰写联想，不制造并不存在的“往来 N 次”。
pub fn touch_conn(conn: &Connection, addr: &str, name: &str, at: i64) -> rusqlite::Result<()> {
    let addr = addr.trim();
    if !addr_plausible(addr) {
        return Ok(());
    }
    let name = name_safe(name.trim());
    conn.execute(
        "INSERT INTO contacts(addr, name, updated_at, hits) VALUES (?1, ?2, ?3, 0)
         ON CONFLICT(addr) DO UPDATE SET
             name = CASE
                 WHEN contacts.manual_name = 1 THEN contacts.name
                 WHEN excluded.name <> '' THEN excluded.name
                 ELSE contacts.name END,
             updated_at = MAX(contacts.updated_at, excluded.updated_at)",
        rusqlite::params![addr, name, at],
    )?;
    Ok(())
}

/// 本账号地址（小写；无发件记录 → 空串）：取已发送邮件里出现最多的 From。
/// 账号配置属于 corpmail.toml（不落 settings 库），收件线程只有 DB 连接，
/// 因此用「已发送邮件的发件人」反推自身地址——不依赖配置读取，且天然覆盖
/// 别名/多账号场景。联系人采集与统计据此排除「自己」。
pub fn self_addr_conn(conn: &Connection) -> String {
    conn.query_row(
        "SELECT lower(from_addr) FROM emails
          WHERE direction = 'out' AND from_addr <> ''
          GROUP BY lower(from_addr) ORDER BY count(*) DESC LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_default()
    .trim()
    .to_string()
}

/// 去重（地址小写）+ 排除本账号，追加到 out
fn push_deduped(
    list: Vec<Participant>,
    seen: &mut std::collections::HashSet<String>,
    self_l: &str,
    out: &mut Vec<Participant>,
) {
    for (addr, name) in list {
        let a = addr.trim();
        if a.is_empty() || !a.contains('@') {
            continue;
        }
        let key = a.to_ascii_lowercase();
        if !self_l.is_empty() && key == self_l {
            continue;
        }
        if seen.insert(key) {
            out.push((a.to_string(), name));
        }
    }
}

/// 参与人（地址, 姓名）
pub type Participant = (String, String);

/// 单封邮件参与人拆分（2026-11-09 统计口径修正）：
/// - direct（计入往来次数）：收件 = From；发件 = To/Cc/Bcc；
/// - visible（仅采集）：收件的其余 To/Cc —— 群发通知的抄送名单不算“与你往来”。
///
/// 两者均按地址去重并排除本账号；同一地址 direct 优先。
pub fn participants(
    direction: &str,
    from_addr: &str,
    from_name: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    self_l: &str,
) -> (Vec<Participant>, Vec<Participant>) {
    let mut seen = std::collections::HashSet::new();
    let mut direct = Vec::new();
    let mut visible = Vec::new();
    if direction == "in" {
        push_deduped(
            vec![(from_addr.to_string(), from_name.to_string())],
            &mut seen,
            self_l,
            &mut direct,
        );
        let mut others = parse_addr_list(to);
        others.extend(parse_addr_list(cc));
        push_deduped(others, &mut seen, self_l, &mut visible);
    } else {
        let mut all = parse_addr_list(to);
        all.extend(parse_addr_list(cc));
        all.extend(parse_addr_list(bcc));
        push_deduped(all, &mut seen, self_l, &mut direct);
    }
    (direct, visible)
}

/// 历史联系人回填（v4 升级）：从 emails 提取收/发件人。
///
/// 第十二轮 P1-1：旧实现以「contacts 表非空」作唯一幂等判据——升级后只要先
/// 收到一封新邮件（增量采集已插入联系人），历史回填就永久跳过；回填中途崩溃
/// 后重启也因表非空而不再续跑，通讯录永久残缺。改为复用 repair_chunked：
/// 游标与每批数据同事务落 repair_marks，中断后下次启动从断点续跑；
/// upsert/touch 本身幂等，重复扫不产生重复联系人。
pub fn backfill(conn: &mut Connection) -> Result<(), String> {
    let self_l = self_addr_conn(conn);
    crate::mail::repair_chunked(
        conn,
        "contacts_backfill",
        |conn, after| {
            type BackfillRow = (i64, String, String, String, String, String, String, i64);
            let rows: Vec<BackfillRow> = crate::db::collect_rows(
                conn,
                "SELECT id, direction, from_addr, COALESCE(from_name,''), to_addrs, \
                     COALESCE(cc_addrs,''), COALESCE(bcc_addrs,''), \
                     COALESCE(received_at, sent_at, created_at, 0) FROM emails \
                     WHERE id > ?1 ORDER BY id LIMIT ?2",
                &[&after, &crate::mail::REPAIR_CHUNK],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
            let scanned = rows.last().map(|r| r.0).unwrap_or(0);
            Ok((rows, scanned))
        },
        |tx, (_, dir, from_addr, from_name, to_addrs, cc_addrs, bcc_addrs, at)| {
            let (direct, visible) = participants(
                dir, from_addr, from_name, to_addrs, cc_addrs, bcc_addrs, &self_l,
            );
            for (addr, name) in direct {
                upsert_conn(tx, &addr, &name, *at).map_err(|e| e.to_string())?;
            }
            for (addr, name) in visible {
                touch_conn(tx, &addr, &name, *at).map_err(|e| e.to_string())?;
            }
            Ok(())
        },
    )?;
    Ok(())
}

/// 存量统计修复（v12 口径，幂等）：按历史邮件重算
/// - updated_at = 该地址所有往来邮件的最大实际时间（received_at/sent_at，退化 created_at）
/// - hits = 直接往来邮件数（收件=From，发件=To/Cc/Bcc；单封去重、排除自己）；
/// - 收件抄送名单只算「最近联系」，不计往来次数。
///
/// 同时删除本账号自身条目（早期把「自己」也采集成了联系人）。
/// contacts 是派生数据，但手动新增/改名的姓名保留——只更新能追溯到邮件的地址。
/// 标记写 "2"：v11 写的是 "1"，升级后自动重算一次。
pub fn repair_stats(conn: &mut Connection) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;
    use std::collections::HashMap;

    let done: Option<String> = conn
        .query_row(
            "SELECT v FROM settings WHERE k = ?1",
            [crate::state::keys::CONTACTS_STATS_FIXED],
            |r| r.get(0),
        )
        .optional()?;
    if done.as_deref() == Some("2") {
        return Ok(());
    }
    // contacts 为空（新库/首启）无需修复，也不写标记——保持 store_is_empty 语义
    let n: i64 = conn.query_row("SELECT count(*) FROM contacts", [], |r| r.get(0))?;
    if n == 0 {
        return Ok(());
    }
    let self_l = self_addr_conn(conn);
    // addr(小写) → (最近往来时间, 直接往来邮件数)；visible 仅刷新时间，计数保持。
    // 审计 R52：按 id 分批读（每批 500），聚合表只保留地址维度，不再一次性
    // 把整表邮件行读进内存（大信箱上是几十万行 × 多个地址串）
    let mut stat: HashMap<String, (i64, i64)> = HashMap::new();
    let mut after = 0i64;
    loop {
        type StatRow = (i64, String, String, String, String, String, i64);
        let rows: Vec<StatRow> = crate::db::collect_rows(
            conn,
            "SELECT id, direction, from_addr, to_addrs, COALESCE(cc_addrs,''), \
             COALESCE(bcc_addrs,''), COALESCE(received_at, sent_at, created_at, 0) FROM emails \
             WHERE id > ?1 ORDER BY id LIMIT ?2",
            &[&after, &crate::mail::REPAIR_CHUNK],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )?;
        let Some(last) = rows.last().map(|r| r.0) else {
            break;
        };
        for (_, dir, from_addr, to_addrs, cc_addrs, bcc_addrs, at) in rows {
            let (direct, visible) = participants(
                &dir, &from_addr, "", &to_addrs, &cc_addrs, &bcc_addrs, &self_l,
            );
            for (addr, _) in direct {
                let e = stat.entry(addr.to_ascii_lowercase()).or_insert((at, 0));
                e.0 = e.0.max(at);
                e.1 += 1;
            }
            for (addr, _) in visible {
                let e = stat.entry(addr.to_ascii_lowercase()).or_insert((at, 0));
                e.0 = e.0.max(at);
            }
        }
        after = last;
    }
    let tx = conn.transaction()?;
    if !self_l.is_empty() {
        tx.execute("DELETE FROM contacts WHERE lower(addr) = ?1", [&self_l])?;
    }
    for (addr, (at, hits)) in stat {
        tx.execute(
            "UPDATE contacts SET updated_at = ?2, hits = ?3 WHERE lower(addr) = ?1",
            rusqlite::params![addr, at, hits],
        )?;
    }
    tx.execute(
        "INSERT INTO settings(k, v) VALUES (?1, '2') ON CONFLICT(k) DO UPDATE SET v = '2'",
        [crate::state::keys::CONTACTS_STATS_FIXED],
    )?;
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
    // 第十一轮 P2-4：与自动采集 upsert_conn 同口径折叠控制字符——用户从邮件
    // 签名粘贴进来的 CR/LF 不应进入建议列表与联系人页（显示注入/撑破布局）。
    let name = name_safe(name.trim());
    // 编辑已有联系人只改姓名，不刷新 updated_at——「最近联系」是邮件往来的
    // 时间戳，改个名字不应让它变成"今天联系过"
    // R15 P1：手动保存打上 manual_name=1 锁定标记，后续 upsert/touch/backfill
    // 都不得再用邮件显示名覆盖（用户备注/改名是权威数据源）。
    conn.execute(
        "INSERT INTO contacts(addr, name, updated_at, hits, manual_name)
         VALUES (?1, ?2, ?3, 1, 1)
         ON CONFLICT(addr) DO UPDATE SET name = excluded.name, manual_name = 1",
        rusqlite::params![addr.trim(), name, now],
    )?;
    Ok(())
}
