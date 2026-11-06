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
    // 幂等：再次回填不新增
    backfill(&mut conn).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM contacts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n as usize, addrs.len());
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
