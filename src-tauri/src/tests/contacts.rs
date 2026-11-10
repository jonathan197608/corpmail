//! 联系人库单元测试（白盒：use super::* 继承 contacts 模块作用域）

use super::*;
use crate::state::AppState;
use std::path::PathBuf;

fn temp_state(tag: &str) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("corpmail-contacts-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (AppState::open(&dir), dir)
}

#[test]
fn parse_addr_list_handles_common_forms() {
    let got = parse_addr_list("张三 <zhang@x.cn>, li@x.cn; \"王五\" <wang@x.cn>");
    assert_eq!(
        got,
        vec![
            ("zhang@x.cn".to_string(), "张三".to_string()),
            ("li@x.cn".to_string(), String::new()),
            ("wang@x.cn".to_string(), "王五".to_string()),
        ]
    );
    // 非法片段（无 @）丢弃
    assert!(parse_addr_list("not-an-address, ok@x.cn").len() == 1);
    assert!(parse_addr_list("").is_empty());
}

#[test]
fn upsert_merges_name_and_counts_hits() {
    let (state, dir) = temp_state("upsert");
    state
        .with_conn(|c| {
            upsert_conn(c, "a@x.cn", "张三", 100).unwrap();
            // 空姓名不得覆盖已有姓名
            upsert_conn(c, "a@x.cn", "", 200).unwrap();
            // 新姓名覆盖
            upsert_conn(c, "a@x.cn", "张三丰", 300).unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    let (name, hits, at): (String, i64, i64) = state
        .with_conn(|c| {
            c.query_row(
                "SELECT name, hits, updated_at FROM contacts WHERE addr = 'a@x.cn'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .unwrap();
    assert_eq!(name, "张三丰", "非空新姓名应覆盖");
    assert_eq!(hits, 3, "每次 upsert 累加命中");
    assert_eq!(at, 300);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 乱序收取历史邮件（先新后旧）不得把「最近联系」往回拨
#[test]
fn upsert_keeps_latest_contact_time() {
    let (state, dir) = temp_state("upsert-max");
    state.with_conn(|c| {
        upsert_conn(c, "a@x.cn", "", 300).unwrap();
        upsert_conn(c, "a@x.cn", "", 200).unwrap(); // 旧信后到
        upsert_conn(c, "a@x.cn", "", 500).unwrap();
    });
    let (at, hits): (i64, i64) = state.with_conn(|c| {
        c.query_row(
            "SELECT updated_at, hits FROM contacts WHERE addr = 'a@x.cn'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(at, 500, "最近联系应取最大时间");
    assert_eq!(hits, 3, "命中次数照常累加");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn suggest_matches_name_or_addr_and_orders_exact_first() {
    let (state, dir) = temp_state("suggest");
    state
        .with_conn(|c| {
            upsert_conn(c, "zhang@x.cn", "张三", 10).unwrap();
            upsert_conn(c, "zhangwei@y.cn", "张伟", 20).unwrap();
            upsert_conn(c, "other@z.cn", "李四", 30).unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    // 按姓名模糊
    let by_name = state.with_conn(|c| suggest(c, "张", 8)).unwrap();
    let addrs: Vec<&str> = by_name.iter().map(|c| c.addr.as_str()).collect();
    assert!(addrs.contains(&"zhang@x.cn") && addrs.contains(&"zhangwei@y.cn"));
    // 按地址模糊 + 精确地址优先
    let by_addr = state.with_conn(|c| suggest(c, "zhangwei@y.cn", 8)).unwrap();
    assert_eq!(by_addr[0].addr, "zhangwei@y.cn", "精确命中排最前");
    // LIKE 通配符不得被当成模式：% 查询应无匹配
    let wild = state.with_conn(|c| suggest(c, "%", 8)).unwrap();
    assert!(wild.is_empty(), "% 应被转义为字面量");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backfill_extracts_from_historical_emails_once() {
    let (state, dir) = temp_state("backfill");
    // 先写入历史邮件（不经过联系人采集）
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    cc_addrs, created_at)
                 VALUES ('in','inbox','received','boss@x.cn','领导','demo@x.cn','cc@x.cn',1)",
                [],
            )
            .unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    // 回填（独立连接，与启动路径一致）
    let mut conn = crate::db::open(&state.data_dir().join("corpmail.db")).unwrap();
    backfill(&mut conn).unwrap();
    let addrs: Vec<String> =
        crate::db::collect_rows(&conn, "SELECT addr FROM contacts ORDER BY addr", &[], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    assert!(addrs.contains(&"boss@x.cn".to_string()));
    assert!(addrs.contains(&"demo@x.cn".to_string()));
    assert!(addrs.contains(&"cc@x.cn".to_string()));
    // 回填时间取邮件实际时间（created_at=1），不是回填时刻
    let at: i64 = conn
        .query_row(
            "SELECT updated_at FROM contacts WHERE addr = 'boss@x.cn'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(at, 1, "回填应使用邮件实际时间而非回填时刻");
    // 幂等：再次回填不新增
    backfill(&mut conn).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM contacts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n as usize, addrs.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 统计口径（v12）：收件只计 From；群发 To/Cc 仅采集不计数；
/// 单封重复地址只计一次；自身排除。发件 To/Cc/Bcc 均为直接往来
#[test]
fn participants_split_direct_and_visible() {
    let self_l = "me@x.cn";
    let (direct, visible) = participants(
        "in",
        "boss@x.cn",
        "领导",
        "me@x.cn, peer@x.cn",
        "peer@x.cn, cc@x.cn",
        "",
        self_l,
    );
    assert_eq!(direct, vec![("boss@x.cn".into(), "领导".into())]);
    assert_eq!(
        visible.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(),
        ["peer@x.cn", "cc@x.cn"],
        "群发抄送去重后仅采集"
    );
    let (direct, visible) = participants(
        "out",
        "me@x.cn",
        "",
        "peer@x.cn, me@x.cn",
        "peer@x.cn",
        "bcc@x.cn",
        self_l,
    );
    assert_eq!(
        direct.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(),
        ["peer@x.cn", "bcc@x.cn"],
        "发件收件人/密送均计直接往来且去重"
    );
    assert!(visible.is_empty());
}

/// 存量统计修复 v12：只计直接往来、群发抄送不计；删除自身条目；
/// 旧 v11 标记不阻止重算（升级即修）
#[test]
fn repair_stats_v12_counts_only_direct_and_drops_self() {
    let (state, dir) = temp_state("repair-v12");
    state
        .set_batch(&[
            (crate::state::keys::ACCOUNT_PREFIX, "demo"),
            (crate::state::keys::ACCOUNT_DOMAIN, "x.cn"),
        ])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, cc_addrs,
                                    created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','demo@x.cn','peer@x.cn',100,100);
                 INSERT INTO emails(direction, folder, status, from_addr, to_addrs, cc_addrs,
                                    created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','demo@x.cn','peer@x.cn',200,200);
                 INSERT INTO emails(direction, folder, status, from_addr, to_addrs, cc_addrs,
                                    created_at, sent_at)
                 VALUES ('out','sent','sent','demo@x.cn','peer@x.cn','',300,300);",
            )
            .unwrap();
            // 旧版膨胀数据：boss 836、peer 900、自己 2299；v11 标记存在也应重算
            c.execute_batch(
                "INSERT INTO contacts(addr,name,updated_at,hits) VALUES ('boss@x.cn','领导',1,836);
                 INSERT INTO contacts(addr,name,updated_at,hits) VALUES ('peer@x.cn','同事',1,900);
                 INSERT INTO contacts(addr,name,updated_at,hits) VALUES ('demo@x.cn','自己',1,2299);
                 INSERT INTO settings(k,v) VALUES ('contacts_stats_fixed','1')
                 ON CONFLICT(k) DO UPDATE SET v='1';",
            )
            .unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    let mut conn = crate::db::open(&state.data_dir().join("corpmail.db")).unwrap();
    assert_eq!(
        self_addr_conn(&conn),
        "demo@x.cn",
        "自身地址应能从事 settings 读出"
    );
    repair_stats(&mut conn).unwrap();
    let (boss_at, boss_hits): (i64, i64) = conn
        .query_row(
            "SELECT updated_at, hits FROM contacts WHERE addr='boss@x.cn'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(boss_hits, 2, "收件只计发件人（两封）");
    assert_eq!(boss_at, 200, "最近联系取最大实际时间");
    let peer_hits: i64 = conn
        .query_row(
            "SELECT hits FROM contacts WHERE addr='peer@x.cn'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(peer_hits, 1, "群发抄送不计数，仅自己发的那 1 封计入");
    let self_n: i64 = conn
        .query_row(
            "SELECT count(*) FROM contacts WHERE addr='demo@x.cn'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(self_n, 0, "自身条目应被删除");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 存量统计修复：按历史邮件实际时间重算「最近联系/往来次数」，
/// 保留纯手动联系人；完成后写标记幂等（不再重复扫描）
#[test]
fn repair_stats_recomputes_times_and_preserves_manual() {
    let (state, dir) = temp_state("repair-stats");
    // 两封历史邮件：实际时间 1000 / 3000，入库时间 9000
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name,
                                    to_addrs, cc_addrs, created_at, received_at)
                 VALUES ('in','inbox','received','a@x.cn','甲','demo@x.cn','',9000,1000);
                 INSERT INTO emails(direction, folder, status, from_addr, from_name,
                                    to_addrs, cc_addrs, created_at, received_at)
                 VALUES ('in','inbox','received','a@x.cn','甲','demo@x.cn','',9000,3000);",
            )
            .unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    // 模拟旧版错误数据：自动采集联系人时间被写成入库时间 9000；另有一条纯手动联系人
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO contacts(addr,name,updated_at,hits) VALUES ('a@x.cn','甲',9000,1);
                 INSERT INTO contacts(addr,name,updated_at,hits) VALUES ('manual@x.cn','手动',9000,1);",
            )
            .unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    let mut conn = crate::db::open(&state.data_dir().join("corpmail.db")).unwrap();
    repair_stats(&mut conn).unwrap();
    let (at, hits): (i64, i64) = conn
        .query_row(
            "SELECT updated_at, hits FROM contacts WHERE addr = 'a@x.cn'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(at, 3000, "应修正为邮件实际最近时间");
    assert_eq!(hits, 2, "往来次数按直接往来邮件数重算");
    let manual_at: i64 = conn
        .query_row(
            "SELECT updated_at FROM contacts WHERE addr = 'manual@x.cn'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(manual_at, 9000, "纯手动联系人不应被改动");
    // 幂等：标记已写，再次修复不覆盖（人为改值验证）
    conn.execute(
        "UPDATE contacts SET updated_at = 9999 WHERE addr = 'a@x.cn'",
        [],
    )
    .unwrap();
    repair_stats(&mut conn).unwrap();
    let at2: i64 = conn
        .query_row(
            "SELECT updated_at FROM contacts WHERE addr = 'a@x.cn'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(at2, 9999, "修复完成后应幂等跳过");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 管理页列表（M3-3）：q 空返回全部按最近联系倒序；模糊按姓名/地址；
/// remove 删除后列表消失，重复删除返回 false
#[test]
fn list_rows_filters_and_remove_deletes() {
    let (state, dir) = temp_state("list-rows");
    let now = super::super::now_epoch();
    // 直接 INSERT（upsert_conn 的 hits 固定累加语义，这里要各异的采样值）
    state
        .with_conn(|c| {
            c.execute_batch(&format!(
                "INSERT INTO contacts(addr, name, updated_at, hits) VALUES
                 ('a@x.cn', '王副总', {a}, 3),
                 ('b@x.cn', '李静', {b}, 9),
                 ('wang.p@partner.cn', '合作方小王', {c}, 1)",
                a = now,
                b = now - 3600,
                c = now - 7200
            ))
            .map_err(|e| e.to_string())
        })
        .unwrap();
    // 全部：按 updated_at 倒序（a → b → wang.p）
    let all = state
        .with_conn(|c| list_rows(c, "", 500, ContactsSort::Recent))
        .unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].addr, "a@x.cn");
    assert_eq!(all[0].name, "王副总");
    assert_eq!(all[0].hits, 3);
    // 模糊"王"：命中姓名王副总 + 地址含 wang（两条，按最近联系排序）
    let w = state
        .with_conn(|c| list_rows(c, "王", 500, ContactsSort::Recent))
        .unwrap();
    assert_eq!(
        w.iter().map(|r| r.addr.as_str()).collect::<Vec<_>>(),
        ["a@x.cn", "wang.p@partner.cn"]
    );
    // 模糊"静静"：无命中
    assert!(
        state
            .with_conn(|c| list_rows(c, "静静", 500, ContactsSort::Recent))
            .unwrap()
            .is_empty()
    );
    // limit 生效
    assert_eq!(
        state
            .with_conn(|c| list_rows(c, "", 2, ContactsSort::Recent))
            .unwrap()
            .len(),
        2
    );
    // 删除：真删 → 列表缩，重复删 false
    assert!(state.with_conn(|c| remove(c, "b@x.cn")).unwrap());
    assert!(!state.with_conn(|c| remove(c, "b@x.cn")).unwrap());
    let after = state
        .with_conn(|c| list_rows(c, "", 500, ContactsSort::Recent))
        .unwrap();
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|r| r.addr != "b@x.cn"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 自动采集地址入库判定（安全修复③）：控制字符/空白/分隔符/尖括号/超长/缺 @ 拒绝
#[test]
fn upsert_rejects_dirty_addresses_and_names() {
    let (state, dir) = temp_state("dirty-addr");
    state
        .with_conn(|c| {
            // 正常地址入库
            upsert_conn(c, "ok@x.cn", "正常", 100).unwrap();
            // CRLF 注入形态（邮件头拆行攻击）
            upsert_conn(c, "evil@x.cn\r\nBcc: victim@x.cn", "注入", 100).unwrap();
            // 空白/尖括号/引号/逗号（分隔符残留）
            upsert_conn(c, "sp ace@x.cn", "a", 100).unwrap();
            upsert_conn(c, "<angle@x.cn>", "a", 100).unwrap();
            upsert_conn(c, "quote\"@x.cn", "a", 100).unwrap();
            // 缺 @ / 空段
            upsert_conn(c, "no-at-sign", "a", 100).unwrap();
            upsert_conn(c, "@x.cn", "a", 100).unwrap();
            upsert_conn(c, "a@", "a", 100).unwrap();
            // 超长（> 254）
            let long = format!("{}@x.cn", "a".repeat(260));
            upsert_conn(c, &long, "a", 100).unwrap();
            // 姓名控制字符清洗（地址合法）
            upsert_conn(c, "name@x.cn", "张\r\n三", 100).unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    let rows: Vec<(String, String)> = state
        .with_conn(|c| {
            Ok::<Vec<(String, String)>, rusqlite::Error>(
                list_rows(c, "", 500, ContactsSort::Recent)
                    .unwrap()
                    .into_iter()
                    .map(|r| (r.addr, r.name))
                    .collect(),
            )
        })
        .unwrap();
    let addrs: Vec<&str> = rows.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(rows.len(), 2, "仅两条合法地址应入库：{rows:?}");
    assert!(addrs.contains(&"ok@x.cn"), "{rows:?}");
    assert!(addrs.contains(&"name@x.cn"), "{rows:?}");
    assert!(
        !addrs.iter().any(|a| a.contains('\n') || a.contains('\r')),
        "CR/LF 地址不得入库：{rows:?}"
    );
    // 姓名清洗：CR/LF 替换为空格，不残留控制字符
    let name = rows
        .iter()
        .find(|(a, _)| a == "name@x.cn")
        .map(|(_, n)| n)
        .unwrap();
    assert_eq!(name, "张 三", "姓名控制字符应替换为空格：{name:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
