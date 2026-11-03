//! mail 模块单元测试（自 src/mail/mod.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::state::AppState;

#[test]
fn sanitize_html_removes_xss_vectors() {
    let raw = r#"<p style="color:red">hi<script>alert(1)</script><img src="x" onerror="alert(1)"><a href="javascript:alert(1)">link</a></p>"#;
    let out = sanitize_html(raw);
    let lower = out.to_lowercase();
    assert!(!lower.contains("<script"), "应移除 script：{out}");
    assert!(!lower.contains("onerror"), "应移除事件属性：{out}");
    assert!(!lower.contains("javascript:"), "应移除危险 URL：{out}");
    assert!(out.contains("hi"), "正文内容应保留：{out}");
    assert!(out.contains("color:red"), "内联样式应保留：{out}");
}

#[test]
fn sanitize_html_rewrites_remote_images_to_proxy() {
    let out = sanitize_html(r#"<p><img src="https://img.189.cn/a.png" alt="x"></p>"#);
    assert!(
        out.contains("/proxy/image?url=https%3A%2F%2Fimg.189.cn%2Fa.png"),
        "远程图片应改写为代理地址：{out}"
    );
    // 非 http(s) 地址不改写（cid 内联等由净化器自行处理）
    let local = sanitize_html(r#"<img src="cid:inline1">"#);
    assert!(!local.contains("/proxy/image"), "内联图片不改写：{local}");
}

#[test]
fn sanitize_html_allows_inline_data_image_but_blocks_other_data() {
    // 编辑器插入的本地图片（data:image/*）应保留
    let ok =
        sanitize_html(r##"<p>你好</p><img src="data:image/png;base64,iVBORw0KGgo=" alt="图">"##);
    assert!(
        ok.contains("data:image/png;base64,iVBORw0KGgo="),
        "data:image/* 内嵌图片应放行：{ok}"
    );
    // 危险 URI：data:text/html（可执行）在链接与图片上一律删除
    let evil = sanitize_html(
        r#"<a href="data:text/html,<script>alert(1)</script>">x</a><img src="data:text/html,evil">"#,
    );
    assert!(
        !evil.contains("data:text/html"),
        "危险 data: 应全部移除：{evil}"
    );
}

#[test]
fn unwrap_top_div_expands_wrapper_and_keeps_structures() {
    // 单一顶层 div 包裹（企业邮件常见全文包一层）：应展开内部
    let wrapped = r#"<div style="font-family:sans-serif"><h2>标题</h2><table><tr><td>格</td></tr></table></div>"#;
    let out = unwrap_top_div(wrapped);
    assert!(out.starts_with("<h2>"), "顶层 div 应剥除：{out}");
    assert!(out.ends_with("</table>"), "内部结构应保留：{out}");
    // 多重包裹：循环剥壳
    let nested = "<div><div><p>内文</p></div></div>";
    assert_eq!(unwrap_top_div(nested), "<p>内文</p>");
    // 多顶层节点混排：原样（不误剥）
    let mixed = r#"<div>a</div><p>b</p>"#;
    assert_eq!(unwrap_top_div(mixed), mixed);
    // 顶层非 div：原样
    let plain = r#"<p>仅段落</p><table><tr><td>x</td></tr></table>"#;
    assert_eq!(unwrap_top_div(plain), plain);
    // 嵌套子 div 且顶层为包裹 div：只剥外层，内层保留
    let inner = r#"<div><p>a</p><div>b</div></div>"#;
    assert_eq!(unwrap_top_div(inner), r#"<p>a</p><div>b</div>"#);
}

#[test]
fn sanitize_html_keeps_table_border_attributes() {
    // 企业邮件（Outlook/Foxmail）表格边框普遍用属性而非 CSS：
    // border/cellpadding/cellspacing 不在白名单会被剥掉，表格失去边框
    let out = sanitize_html(
        r#"<table border="1" cellpadding="6" cellspacing="0"><tr><th>客户</th><td>200M</td></tr></table>"#,
    );
    assert!(out.contains(r#"border="1""#), "border 属性应保留：{out}");
    assert!(
        out.contains("cellpadding=\"6\""),
        "cellpadding 应保留：{out}"
    );
    assert!(
        out.contains("cellspacing=\"0\""),
        "cellspacing 应保留：{out}"
    );
}

fn temp_state(tag: &str) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("corpmail-m2-{tag}-{}", std::process::id()));
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
    // SVG 拒绝内嵌（顶层渲染可执行脚本；MIME 与扩展名两条路径都不得归入 img）
    assert_eq!(preview_kind_of("image/svg+xml", "图.svg"), "");
    assert_eq!(preview_kind_of("", "图.svg"), "");
    assert_eq!(preview_kind_of("application/octet-stream", "图.svg"), "");
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
        external_only: false,
        internal_domain: "chinatelecom.cn".into(),
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
        external_only: false,
        internal_domain: "chinatelecom.cn".into(),
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
            external_only: false,
            internal_domain: "chinatelecom.cn".into(),
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
            external_only: false,
            internal_domain: "chinatelecom.cn".into(),
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
            external_only: false,
            internal_domain: "chinatelecom.cn".into(),
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
            external_only: false,
            internal_domain: "chinatelecom.cn".into(),
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
    save_attachment_to(&state, id, aid, dest.to_str().unwrap()).unwrap();
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

#[test]
fn local_tz_cache_returns_fresh_zone() {
    // 两次取时区（缓存命中/未命中路径）：换算出的带时区时间必须一致
    let z1 = zoned_of(1_760_000_000).map(|z| z.to_string());
    let z2 = zoned_of(1_760_000_000).map(|z| z.to_string());
    assert_eq!(z1, z2, "两次取时区对同一时刻的换算应一致");
    assert!(z1.is_some(), "合法 epoch 应可换算");
}

/// 0 字节文件跳过并回报，其余附件照常入库（旧实现整单失败，白填整封信）
#[test]
fn add_attachments_skips_empty_file() {
    let (state, dir) = temp_state("attach-empty");
    let id = save_draft(&state, &draft("空附件", "体")).unwrap();
    let empty = dir.join("empty.txt");
    std::fs::write(&empty, b"").unwrap();
    let good = dir.join("good.txt");
    std::fs::write(&good, b"data").unwrap();
    let (added, skipped) = add_attachments(
        &state,
        id,
        &[
            empty.to_string_lossy().into_owned(),
            good.to_string_lossy().into_owned(),
        ],
    )
    .unwrap();
    assert_eq!(added, 1, "非空文件应正常入库");
    assert_eq!(
        skipped,
        vec!["empty.txt".to_string()],
        "空文件应回报而非报错"
    );
    let d = get(&state, id).unwrap();
    assert_eq!(d.attach_cnt, 1);
    assert_eq!(d.attachments[0].filename, "good.txt");
    cleanup(&dir);
}

/// 独立文件序号不复用：删中间附件后再加同名文件不得覆盖既有文件
/// （旧实现按 count+1 命名，两条 DB 行会指向同一路径）
#[test]
fn attachment_file_names_never_collide() {
    let (state, dir) = temp_state("attach-collide");
    // 上限设为 0 → 所有附件都走独立文件（无需造 20MB 文件）
    state
        .set_batch(&[(crate::state::keys::INLINE_ATTACH_MAX_MB, "0")])
        .unwrap();
    let id = save_draft(&state, &draft("同名附件", "体")).unwrap();
    let src = dir.join("a.txt");
    std::fs::write(&src, b"first").unwrap();
    add_attachments(&state, id, &[src.to_string_lossy().into_owned()]).unwrap();
    add_attachments(&state, id, &[src.to_string_lossy().into_owned()]).unwrap();

    let d = get(&state, id).unwrap();
    assert_eq!(d.attach_cnt, 2);
    // 删掉第一条（模拟用户在草稿里移除中间附件）
    remove_attachments_except(&state, id, &[d.attachments[1].id]).unwrap();
    std::fs::write(&src, b"third").unwrap();
    add_attachments(&state, id, &[src.to_string_lossy().into_owned()]).unwrap();

    let paths: Vec<String> = state.with_conn(|c| {
        crate::db::collect_rows(
            c,
            "SELECT file_path FROM attachments WHERE email_id = ?1 ORDER BY id",
            &[&id],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    });
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1], "两条附件行不得指向同一文件");
    for rel in &paths {
        assert!(
            dir.join("attachments").join(rel).is_file(),
            "附件文件应实际存在：{rel}"
        );
    }
    cleanup(&dir);
}

#[test]
fn copy_attachments_duplicates_files_and_skips_detached() {
    let (state, dir) = temp_state("attach-copy");
    let src = save_draft(&state, &draft("转发源", "原文")).unwrap();
    let a = dir.join("a.txt");
    std::fs::write(&a, b"alpha").unwrap();
    let b = dir.join("b.bin");
    std::fs::write(&b, b"bravo").unwrap();
    add_attachments(
        &state,
        src,
        &[a.to_str().unwrap().into(), b.to_str().unwrap().into()],
    )
    .unwrap();
    // 模拟清理器已删除 b 的落盘文件：detached 应从复制中跳过而非整体失败
    state.with_conn(|c| {
        c.execute(
            "UPDATE attachments SET detached = 1 WHERE filename = 'b.bin'",
            [],
        )
        .unwrap()
    });

    let dst = save_draft(&state, &draft("转发目标", "新文")).unwrap();
    let (copied, skipped) = copy_attachments(&state, src, dst).unwrap();
    assert_eq!((copied, skipped), (1, 1), "可读附件复制、detached 跳过");

    // 目标邮件独立成行：文件重新落盘，字节与源一致
    let d = get(&state, dst).unwrap();
    assert_eq!(d.attach_cnt, 1);
    assert_eq!(d.attachments[0].filename, "a.txt");
    let out = dir.join("copied.txt");
    save_attachment_to(&state, dst, d.attachments[0].id, out.to_str().unwrap()).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"alpha");

    // 源邮件附件不受影响（复制而非移动）
    assert_eq!(get(&state, src).unwrap().attach_cnt, 2);
    assert_eq!(get(&state, src).unwrap().attachments[0].filename, "a.txt");

    cleanup(&dir);
}

/// 敏感路径守卫（审计 M1/M2）：附件添加与另存为不得指向库/密钥/配置；
/// `..` 逃逸与符号链接形态同样拦截；普通用户路径放行
#[test]
fn attachment_paths_guard_sensitive_targets() {
    let (state, dir) = temp_state("attach-guard");
    let id = save_draft(&state, &draft("守卫", "体")).unwrap();
    // temp_state 中锚点目录与数据目录同为 dir；corpmail.db 已随 open 存在
    let db = dir.join("corpmail.db").to_string_lossy().into_owned();
    let key = dir.join("corpmail.key").to_string_lossy().into_owned();
    let toml = dir.join("corpmail.toml").to_string_lossy().into_owned();
    let dotdot = dir
        .join("sub/../corpmail.db")
        .to_string_lossy()
        .into_owned();

    let assert_rejected = |path: &str| {
        let err = add_attachments(&state, id, &[path.to_string()]);
        assert!(err.is_err(), "添加附件路径应被拒：{path}");
        assert!(
            err.unwrap_err().contains("已拒绝"),
            "拒绝原因应为敏感路径守卫：{path}"
        );
    };
    assert_rejected(&db);
    assert_rejected(&key);
    assert_rejected(&toml);
    assert_rejected(&dotdot);

    // 符号链接指向敏感文件：canonicalize 穿透后仍属同一路径（unixonly；windows 无符号权限）
    #[cfg(unix)]
    {
        let link = dir.join("db-link");
        std::os::unix::fs::symlink(dir.join("corpmail.db"), &link).unwrap();
        assert_rejected(&link.to_string_lossy());
    }

    // 另存方向（M1）：dest 指向库文件应被拒，且不触碰原库内容
    let before = std::fs::read(dir.join("corpmail.db")).unwrap_or_default();
    assert!(
        save_attachment_to(&state, id, 1, &db).is_err(),
        "另存到库路径应被拒"
    );
    let after = std::fs::read(dir.join("corpmail.db")).unwrap_or_default();
    assert_eq!(before, after, "被拒的另存不得写坏库文件");

    // 普通路径放行
    let ok = dir.join("普通附件.txt");
    std::fs::write(&ok, b"normal").unwrap();
    assert!(add_attachments(&state, id, &[ok.to_str().unwrap().into()]).is_ok());
    cleanup(&dir);
}

/// 附件归属校验（审计 L1）：URL 里的 email_id 必须是附件真实属主，
/// 跨邮件 id 取附件（预览/另存）应 404/失败
#[test]
fn attachment_ops_require_owner_email() {
    let (state, dir) = temp_state("attach-owner");
    let one = save_draft(&state, &draft("有附件", "体")).unwrap();
    let two = save_draft(&state, &draft("无附件", "体")).unwrap();
    let f = dir.join("a.txt");
    std::fs::write(&f, b"hello").unwrap();
    add_attachments(&state, one, &[f.to_str().unwrap().into()]).unwrap();
    let aid = get(&state, one).unwrap().attachments[0].id;

    // 正确属主：可读可另存
    assert!(attachment_bytes(&state, one, aid).is_some());
    let out = dir.join("ok.txt");
    save_attachment_to(&state, one, aid, out.to_str().unwrap()).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"hello");

    // 错误属主（他邮件 id + 本附件 id）：一律拒绝
    assert!(
        attachment_bytes(&state, two, aid).is_none(),
        "跨邮件取附件应返回 None"
    );
    assert!(
        save_attachment_to(&state, two, aid, out.to_str().unwrap()).is_err(),
        "跨邮件另存应失败"
    );
    cleanup(&dir);
}
