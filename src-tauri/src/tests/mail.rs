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

/// style 属性内 CSS url() 远程资源改写（审计 L3）：与 img src 同口径走代理，
/// data:image/* 与相对路径保留，畸形 CSS 原样
#[test]
fn sanitize_html_rewrites_css_urls_to_proxy() {
    // 裸 URL 形态（Outlook 常见）
    let out = sanitize_html(
        r##"<div style="background-image:url(http://t.189.cn/pixel.gif);color:red">x</div>"##,
    );
    assert!(
        // 属性值内的双引号被 HTML 转义为 &quot;，浏览器解析 style 时还原
        out.contains("url(&quot;/proxy/image?url=http%3A%2F%2Ft.189.cn%2Fpixel.gif&quot;)"),
        "裸 URL 应改写为代理（属性值内引号转义形态）：{out}"
    );
    assert!(out.contains("color:red"), "其余 CSS 声明保留：{out}");
    // 引号形态 + 多条 URL 混合
    let q = sanitize_html(
        r##"<div style="background:url('https://a.189.cn/x.png'),url( http://b.189.cn/y.png )">y</div>"##,
    );
    let proxy_cnt = q.matches("/proxy/image?url=").count();
    assert_eq!(proxy_cnt, 2, "两条远程 URL 均应改写：{q}");
    // data:image/* 内嵌保留
    let d = sanitize_html(
        r##"<div style="background:url(data:image/png;base64,iVBORw0KGgo=)">z</div>"##,
    );
    assert!(
        d.contains("url(data:image/png;base64,iVBORw0KGgo=)"),
        "data:image 应保留：{d}"
    );
    // 相对路径 / 非 url() 样式原样
    let r = sanitize_html(r##"<div style="width:100px;background:url(/local/x.png)">w</div>"##);
    assert!(r.contains("url(/local/x.png)"), "相对路径保留：{r}");
    assert!(r.contains("width:100px"), "非 url 样式原样：{r}");
    // 畸形（无闭括号）：整段原样吐出，不得 panic/截断
    let m = sanitize_html(r##"<div style="background:url(http://x.189.cn/a.png">m</div>"##);
    assert!(m.contains("http://x.189.cn/a.png"), "畸形 CSS 原样：{m}");
    assert!(m.contains(">m<"), "畸形 CSS 之后的正文保留：{m}");
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
        bcc_addrs: String::new(),
        subject: subject.into(),
        body_text: body.into(),
        in_reply_to: None,
        scheduled_at: 0,
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
    assert_eq!(build_query("的了 the"), None, "全停用词无有效词");
    assert_eq!(build_query("   "), None, "纯空白无有效词");
    // R29：单字加前缀通配（中文单字在 FTS5 里是独立 token，精确词搜不到长词；
    // 用非停用单字取样，停用字仍在分词阶段就被滤掉）
    assert_eq!(build_query("税"), Some("\"税\"*".to_string()));
    assert_eq!(build_query("会议"), Some("\"会议\"".to_string()));
}

#[test]
fn preview_tip_truncates_long_body_with_ellipsis() {
    assert_eq!(preview_tip("简短 正文", ""), "简短 正文", "短文原样保留");
    assert_eq!(preview_tip("a\r\n\t b", ""), "a b", "压缩空白");
    let long: String = "字".repeat(400);
    let tip = preview_tip(&long, "");
    assert_eq!(tip.chars().count(), 301, "300 字 + 省略号");
    assert!(tip.ends_with('…'), "超长以省略号收尾");
    assert_eq!(
        preview_tip("", ""),
        "",
        "空正文返回空串（空 data-tip 不触发气泡）"
    );
}

/// 列表预览乱码修复：命名实体（弯引号/省略号/破折号）与数字实体都要解码；
/// 未知实体原样保留，避免误吞正文
#[test]
fn preview_decodes_named_entities() {
    assert_eq!(
        decode_entities("&ldquo;你好&rdquo;&hellip;&mdash;"),
        "“你好”…—"
    );
    assert_eq!(decode_entities("&#20320;&#x597d;"), "你好");
    assert_eq!(decode_entities("&nbsp;&amp;&lt;"), " &<");
    assert_eq!(decode_entities("&unknown;"), "&unknown;");
    // preview 走同一解码：中文邮件正文里的 &hellip; 不再原样显示
    assert_eq!(preview_text("进度&hellip;", ""), "进度…");
}

/// 列表预览与详情页同源：body_text 乱码（存量）而 body_html 正常时，
/// 列表用 HTML 派生文本；正常正文不被改写；无 HTML 时不丢正文。
#[test]
fn preview_uses_html_source_when_plain_text_is_mojibake() {
    let good = "会议纪要：请于本周五前提交季度报表与预算说明";
    // 构造历史乱码：UTF-8 字节被按 GBK 解码（与 e5abf2f 用户截图同源）
    let (garbled, _, _) = encoding_rs::GBK.decode(good.as_bytes());
    let garbled = garbled.into_owned();
    assert!(
        looks_mojibake(&garbled),
        "构造样本必须被判为乱码：{garbled}"
    );
    let html = format!("<div><p>{good}</p></div>");
    assert_eq!(
        preview_text(&garbled, &html),
        good,
        "乱码应改用同源 HTML 派生文本（详情页显示的内容）"
    );
    assert_eq!(preview_tip(&garbled, &html), good, "悬停预览同源");
    // 正常正文 + HTML：仍然一律同源（不依赖乱码判定，杜绝漏判复发）
    assert_eq!(preview_text("纯文本正文", "<p>HTML 正文</p>"), "HTML 正文");
    // 无 HTML：退回原文本 + 实体解码，不丢正文
    assert_eq!(preview_text("进度&hellip;", ""), "进度…");
    // 老邮件把 HTML 源码当纯文本入库：标签/CSS 不该出现在摘要里
    assert_eq!(
        preview_text("<style>.x{color:red}</style><div>季度报表</div>", ""),
        "季度报表"
    );
    assert_eq!(preview_text("正常 <b>加粗</b> 文本", ""), "正常 加粗 文本");
    // 乱码判定护栏：正常中文/西文/短文本不得误判
    assert!(!looks_mojibake(good), "正常中文不误判");
    assert!(
        !looks_mojibake("Réunion déjà prévue à l'été"),
        "西文重音不误判"
    );
    assert!(!looks_mojibake("中文"), "短文本不判乱码");
    assert!(looks_mojibake("解码\u{FFFD}失败\u{FFFD}占位"));
    // UTF-8 字节被按 latin1 逐字节显示（含 C1 控制字符）
    let latin1_sample: String = good.bytes().map(|b| b as char).collect();
    assert!(looks_mojibake(&latin1_sample), "latin1 误标乱码应命中");
}

/// C18：「高位占绝对多数」分支必须带 latin1 特征序列护栏。
/// 纯重音短西文（短显示名很常见）高位占比可达 100% 却无任何误码特征，
/// 旧实现判成乱码后短显示名会被 clean_display_name 拒绝、正文被 HTML 派生覆盖。
#[test]
fn mojibake_high_ratio_branch_needs_a_latin1_marker() {
    assert!(!looks_mojibake("ÉÉÉÉÉ"), "密集重音短西文不得误判");
    assert!(!looks_mojibake("ÁÁÁÁÉÉÉÉ"), "两种重音拼的短姓名不得误判");
    // seqs 数的是「特征串种类」而非出现次数：只有一种特征却重复多次的
    // 真实误码仍需被这条分支兜住，不能简单把分支删光
    assert!(
        looks_mojibake("Ã©Ã©Ã©"),
        "单一特征重复的 latin1 误码仍应识别"
    );
    // 主判据与 C1 控制字符判据不受影响
    let good = "会议纪要：请于本周五前提交季度报表与预算说明";
    let latin1_sample: String = good.bytes().map(|b| b as char).collect();
    assert!(looks_mojibake(&latin1_sample));
    assert!(!looks_mojibake(good), "正常中文不误判");
}

/// 存量回填：乱码正文用同源 body_html 派生文本写回，FTS 分词同步重算；
/// 正常正文不误改；marker 生效后二次调用不再扫表。
#[test]
fn repair_mojibake_bodies_rewrites_from_html_and_runs_once() {
    let dir = std::env::temp_dir().join(format!("corpmail-repair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut conn = crate::db::open(&dir.join("corpmail.db")).unwrap();
    let good = "会议纪要：请于本周五前提交季度报表与预算说明";
    let (garbled, _, _) = encoding_rs::GB18030.decode(good.as_bytes());
    let garbled = garbled.into_owned();
    assert!(looks_mojibake(&garbled), "构造的存量乱码应被识别");
    let insert = "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                      body_text, body_html, subject_tokens, body_tokens, unread, created_at)
                  VALUES ('in','inbox','received','a@x.cn','b@x.cn','会议', ?1, ?2, '会 议', ?3, 0, 1)";
    conn.execute(
        insert,
        rusqlite::params![garbled, format!("<p>{good}</p>"), "乱 码"],
    )
    .unwrap();
    conn.execute(insert, rusqlite::params!["正常正文", "", "正 常"])
        .unwrap();

    repair_mojibake_bodies(&mut conn).unwrap();
    let (bt, tokens): (String, String) = conn
        .query_row(
            "SELECT body_text, body_tokens FROM emails WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(bt, good, "乱码正文应被同源 HTML 派生文本替换");
    assert_eq!(tokens, tokenize(good), "FTS 分词需同步重算");
    let normal: String = conn
        .query_row("SELECT body_text FROM emails WHERE id = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(normal, "正常正文", "正常正文不得误改");

    // marker：回填后即便再出现乱码，二次启动也不重复扫表（幂等收敛）
    conn.execute(
        "UPDATE emails SET body_text = ?1 WHERE id = 1",
        [garbled.clone()],
    )
    .unwrap();
    repair_mojibake_bodies(&mut conn).unwrap();
    let after: String = conn
        .query_row("SELECT body_text FROM emails WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(after, garbled, "marker 生效后不应二次改写");
    drop(conn);
    cleanup(&dir);
}

/// strip_html 跳过 script/style 内容与 HTML 注释：邮件模板的 CSS 不该进摘要
#[test]
fn strip_html_skips_hidden_blocks_and_comments() {
    let raw = "<style>.x{color:red}</style><!-- outlook 条件注释 --><p>正常正文</p><script>var a=1;</script>";
    assert_eq!(strip_html(raw), "正常正文");
    assert_eq!(strip_html("<p>未闭合"), "未闭合", "未闭合标签后文本保留");
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
    // B8 Office 扩展名/MIME
    assert_eq!(
        preview_kind_of("application/octet-stream", "报告.docx"),
        "docx"
    );
    assert_eq!(
        preview_kind_of("application/octet-stream", "报表.xlsm"),
        "xlsx"
    );
    assert_eq!(
        preview_kind_of("application/octet-stream", "方案.pptm"),
        "pptx"
    );
    assert_eq!(
        preview_kind_of("application/vnd.ms-excel", "旧表.xls"),
        "xls"
    );
    assert_eq!(
        preview_kind_of(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "无扩展名"
        ),
        "docx"
    );
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
        folder: "draft".into(),
        page: 1,
        unread_only: false,
        external_only: false,
        internal_domain: "chinatelecom.cn".into(),
        per: 50,
        sort: SortKey::DateDesc,
        from_filter: String::new(),
        date_from: 0,
        date_to: 0,
        has_attach: false,
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
    // 输入全为停用词 → None（调用方按空结果处理，不再回落整表）
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
        per: 50,
        sort: SortKey::DateDesc,
        from_filter: String::new(),
        date_from: 0,
        date_to: 0,
        has_attach: false,
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

/// 端到端钉死用户症状：DB 里 body_text 是历史乱码、body_html 正常时，
/// 列表行 body_preview / preview_tip 必须与详情页同源（不迁移、不重收）。
#[test]
fn list_preview_uses_html_source_for_mojibake_rows() {
    let (state, dir) = temp_state("list-mojibake");
    let good = "会议纪要：请于本周五前提交季度报表与预算说明";
    let (garbled, _, _) = encoding_rs::GB18030.decode(good.as_bytes());
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    body_text, body_html, subject_tokens, body_tokens, unread, created_at)
                 VALUES ('in','inbox','received','a@x.cn','b@x.cn','会议', ?1, ?2,
                         '会 议', '乱 码', 0, 1)",
                rusqlite::params![garbled.as_ref(), format!("<p>{good}</p>")],
            )
            .unwrap();
        });
    let (items, total) = list(
        &state,
        &ListQuery {
            folder: "inbox".into(),
            page: 1,
            unread_only: false,
            external_only: false,
            internal_domain: "chinatelecom.cn".into(),
            per: 10,
            sort: SortKey::DateDesc,
            from_filter: String::new(),
            date_from: 0,
            date_to: 0,
            has_attach: false,
        },
    )
    .unwrap();
    assert_eq!(total, 1);
    assert_eq!(items[0].body_preview, good, "列表预览不得显示乱码");
    assert_eq!(items[0].preview_tip, good, "悬停预览同源");
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
            per: 10,
            sort: SortKey::DateDesc,
            from_filter: String::new(),
            date_from: 0,
            date_to: 0,
            has_attach: false,
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
            per: 10,
            sort: SortKey::DateDesc,
            from_filter: String::new(),
            date_from: 0,
            date_to: 0,
            has_attach: false,
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
            per: 10,
            sort: SortKey::DateDesc,
            from_filter: String::new(),
            date_from: 0,
            date_to: 0,
            has_attach: false,
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
            per: 10,
            sort: SortKey::DateDesc,
            from_filter: String::new(),
            date_from: 0,
            date_to: 0,
            has_attach: false,
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
    assert!(mark_read(&state, in_id).unwrap());
    assert!(!get(&state, in_id).unwrap().unread);
    assert!(!mark_read(&state, in_id).unwrap(), "重复标记应无效果");
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

/// 后缀推断（2026-11-11）：附件名后缀 > MIME 映射 > .bin；纯数字尾巴不算后缀；
/// 用户已填后缀（含自定义）一律尊重；文件名取 basename 防目录穿越
#[test]
fn attachment_suffix_inference_rules() {
    assert_eq!(suffix_of("a.pdf"), Some("pdf"));
    assert_eq!(suffix_of("报表.XLSX"), Some("XLSX"));
    assert_eq!(suffix_of("报表.2026"), None, "纯数字尾巴不应当后缀");
    assert_eq!(suffix_of("无后缀"), None);
    assert_eq!(suffix_of("x."), None);
    assert_eq!(suffix_of("a.verylongext"), None, "超 8 位不认");
    assert_eq!(ext_for_mime("application/pdf; name=x.pdf"), Some("pdf"));
    assert_eq!(
        ext_for_mime("APPLICATION/VND.OPENXMLFORMATS-OFFICEDOCUMENT.SPREADSHEETML.SHEET"),
        Some("xlsx")
    );
    assert_eq!(ext_for_mime("application/octet-stream"), None);

    assert_eq!(
        save_name_of("清单.xlsx", "application/octet-stream"),
        "清单.xlsx"
    );
    assert_eq!(save_name_of("清单", "application/pdf"), "清单.pdf");
    assert_eq!(save_name_of("清单", "application/octet-stream"), "清单.bin");
    assert_eq!(save_name_of("", "image/png"), "attachment.png");
    assert_eq!(save_name_of("../../etc/passwd", "text/plain"), "passwd.txt");

    assert_eq!(
        with_inferred_ext("/tmp/a", "x.pdf", "application/pdf"),
        "/tmp/a.pdf"
    );
    assert_eq!(
        with_inferred_ext("/tmp/a.pdf", "x.docx", "x/y"),
        "/tmp/a.pdf",
        "路径已有后缀时尊重用户命名"
    );
    assert_eq!(
        with_inferred_ext("/tmp/a.2026", "x", "application/pdf"),
        "/tmp/a.2026.pdf",
        "纯数字尾巴不算后缀，仍补 MIME 后缀"
    );
    assert_eq!(
        with_inferred_ext("/tmp/a", "x", "application/octet-stream"),
        "/tmp/a.bin"
    );
    // Windows 文件名清洗（参数外置，可在任意平台验证两种分支）
    assert_eq!(os_safe_name("季度:汇总?.xlsx", true), "季度 汇总 .xlsx");
    assert_eq!(os_safe_name("报表.", true), "报表");
    assert_eq!(os_safe_name("报表.", false), "报表.");
    assert_eq!(os_safe_name("??", true), "attachment");
}

/// 保存落点白名单（审计 R35）：前端可伪造 dest，必须拒绝系统目录、相对路径
/// 与上跳；用户目录/临时目录/外部卷照常放行（另存到桌面/移动硬盘仍可用）
#[cfg(not(windows))]
#[test]
fn save_dest_whitelist_blocks_system_paths() {
    let (state, dir) = temp_state("dest-guard");
    for bad in [
        "/etc/corpmail-guard-test.eml",
        "/usr/local/bin/x.eml",
        "relative/x.eml",
        "/tmp/../etc/x.eml",
    ] {
        assert!(
            guard_save_dest(&state, bad).is_err(),
            "应拒绝不安全落点：{bad}"
        );
    }
    // 应用自身数据文件优先报更具体的错
    let db = state.data_dir().join("corpmail.db");
    let err = guard_save_dest(&state, &db.to_string_lossy()).unwrap_err();
    assert!(err.contains("应用数据文件"), "{err}");
    // 临时目录（测试数据目录）与用户主目录放行
    let ok = dir.join("导出.eml");
    assert!(guard_save_dest(&state, &ok.to_string_lossy()).is_ok());
    if let Some(home) = std::env::var_os("HOME") {
        let in_home = Path::new(&home).join("corpmail-guard-test.eml");
        assert!(
            guard_save_dest(&state, &in_home.to_string_lossy()).is_ok(),
            "用户主目录应放行"
        );
    }
    cleanup(&dir);
}

/// C7：目录形态的 dest_dir 也必须过白名单。旧实现 guard_save_dest 在
/// `md.is_dir()` 处提前 return Ok(())，而 save_all_attachments 先判 is_dir
/// 再调它 —— R35 白名单对「全部另存」100% 失效（注释与实现相反）。
#[cfg(not(windows))]
#[test]
fn save_dest_whitelist_covers_directory_targets() {
    let (state, dir) = temp_state("dest-dir-guard");
    for bad in ["/usr", "/usr/local/bin", "/etc"] {
        assert!(
            guard_save_dest(&state, bad).is_err(),
            "目录目标也必须过白名单：{bad}"
        );
    }
    let allowed = dir.join("exports");
    std::fs::create_dir_all(&allowed).unwrap();
    assert!(
        guard_save_dest(&state, &allowed.to_string_lossy()).is_ok(),
        "临时目录内新建导出目录应照常放行（正常另存体验不变）"
    );
    cleanup(&dir);
}

/// 另存无后缀路径时按附件名补后缀，并返回实际落盘路径（提示文案据此显示）
#[test]
fn save_attachment_infers_missing_extension() {
    let (state, dir) = temp_state("attach-ext");
    let id = save_draft(&state, &draft("附件后缀", "附件体")).unwrap();
    let src = dir.join("报表.txt");
    std::fs::write(&src, b"data").unwrap();
    add_attachments(&state, id, &[src.to_str().unwrap().into()]).unwrap();
    let aid = get(&state, id).unwrap().attachments[0].id;
    let dest = dir.join("另存无后缀");
    let saved = save_attachment_to(&state, id, aid, dest.to_str().unwrap()).unwrap();
    assert_eq!(saved, format!("{}.txt", dest.display()));
    assert_eq!(std::fs::read(&saved).unwrap(), b"data");
    assert!(!dest.exists(), "不应写到无后缀路径");
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

/// strip_html 数字字符引用解码（2026-11-03 Bug 修复）：中文邮件常把正文
/// 编码为数字实体（GB2312 网关/老客户端），此前原样输出残串——列表预览
/// "乱码"而详情页（浏览器解码 body_html）正常
#[test]
fn strip_html_decodes_numeric_char_refs() {
    // 十进制：&#20320;&#22909; = "你好"
    assert_eq!(strip_html("<p>&#20320;&#22909;</p>"), "你好");
    // 十六进制（小写 x / 大写 X）
    assert_eq!(strip_html("&#x4f60;&#X597D;"), "你好");
    // 与命名实体混排：你 + & + 好；`&nbsp;` 在尾部属于空白噪声，剥标签后统一 trim
    assert_eq!(strip_html("&#20320;&amp;&#22909;&nbsp;"), "你&好");
    // 块级标签补的空格同样受 trim 约束（R26 补空格 + 首尾裁剪）
    assert_eq!(strip_html("<p>金额</p><p>查收</p>"), "金额 查收");
    // 非法码位：0（NUL 不可成 char）、代理区 D800、超 0x10FFFF → 原样保留
    assert_eq!(strip_html("&#0;"), "&#0;");
    assert_eq!(strip_html("&#xD800;"), "&#xD800;");
    assert_eq!(strip_html("&#x110000;"), "&#x110000;");
    // 纯数字但超出 8 字符实体扫描窗口 → 原文输出（含 & 与 ;）
    assert_eq!(
        strip_html("&#99999999999999999999;"),
        "&#99999999999999999999;"
    );
    // 未闭合实体 '&' 原样
    assert_eq!(strip_html("a&b"), "a&b");
    // 截断实体（超过 8 字符扫描窗口）按字面输出
    assert_eq!(strip_html("&#12345678901;"), "&#12345678901;");
}

/// 会话归组键普通化（M2-2）：剥离回复/转发前缀链 + ASCII 小写折叠 + 去空白
#[test]
fn thread_key_of_strips_prefixes_and_normalizes() {
    // 英文前缀（大小写不敏感）+ 冒号变体 + 序号
    assert_eq!(thread_key_of("Re: 开会讨论"), "开会讨论");
    assert_eq!(thread_key_of("RE: meeting"), "meeting");
    assert_eq!(thread_key_of("Fwd: Budget"), "budget");
    assert_eq!(thread_key_of("Re[2]: 报价"), "报价");
    assert_eq!(thread_key_of("re：明天开会"), "明天开会");
    assert_eq!(thread_key_of("FW: 通知"), "通知");
    // 中文前缀（含无冒号/中冒号变体）
    assert_eq!(thread_key_of("回复：明天开会"), "明天开会");
    assert_eq!(thread_key_of("转发: 会议"), "会议");
    assert_eq!(thread_key_of("答复 请查收"), "请查收");
    // 前缀链递归剥离
    assert_eq!(thread_key_of("Re: Fwd: 会议 纪要"), "会议纪要");
    // 无前缀：仅去空白 + ASCII 小写
    assert_eq!(thread_key_of(" 无前缀 主题 "), "无前缀主题");
    assert_eq!(thread_key_of("Meeting Notes"), "meetingnotes");
    // 空主题 / 剥空 → 空串（归组查询排除空串行，不串组）
    assert_eq!(thread_key_of(""), "");
    assert_eq!(thread_key_of("Re:"), "");
    assert_eq!(thread_key_of("   "), "");
}

/// 会话归组键存量回填（M2-2）：只补 NULL 行，已有键不被覆盖；空主题 → 空串
#[test]
fn backfill_thread_keys_fills_null_keys() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE emails(
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           subject TEXT NOT NULL DEFAULT '',
           thread_key TEXT)",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO emails(subject, thread_key) VALUES
         ('Re: 会议', NULL),
         ('复盘', 'meeting'),
         ('', NULL)",
        [],
    )
    .unwrap();

    backfill_thread_keys(&mut conn).unwrap();

    let keys: Vec<(String, Option<String>)> = conn
        .prepare("SELECT subject, thread_key FROM emails ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        keys,
        vec![
            ("Re: 会议".to_string(), Some("会议".to_string())),
            ("复盘".to_string(), Some("meeting".to_string())), // 已有键不覆盖
            ("".to_string(), Some("".to_string())),            // 空主题 → 空串
        ]
    );
}

/// 会话详情（M2-2）：按 id 定位 thread_key，正序串列组内往来；空主题 id → None
#[test]
fn thread_mails_returns_chronological_order() {
    let (state, dir) = temp_state("thread");
    state.with_conn(|c| {
        c.execute_batch(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                subject_tokens, body_tokens, thread_key, unread, created_at) VALUES
            ('in','inbox','received','a@x.cn','b@x.cn','会议','会议','会议','会议',1,1),
            ('out','sent','sent','b@x.cn','a@x.cn','Re: 会议','会议','内容','会议',0,2),
            ('out','draft','draft','a@x.cn','b@x.cn','会议','会议','内容','会议',0,3),
            ('in','inbox','received','a@x.cn','b@x.cn','','','','',0,4)",
        )
        .unwrap();
    });
    // 用组内任一 id（1 或 2）定位
    let detail = thread_mails(&state, 2, "chinatelecom.cn").unwrap().unwrap();
    assert_eq!(
        detail.subject, "Re: 会议",
        "主题取定位行原始主题（最新一封）"
    );
    assert_eq!(detail.thread_key, "会议");
    assert_eq!(detail.items.len(), 2, "草稿不入会");
    assert_eq!(detail.items[0].id, 1, "时间正序");
    assert_eq!(detail.items[1].id, 2);
    assert!(detail.items[0].unread, "未读保留在摘要上");
    // 空主题（id=4）不属于任何会话组
    let none = thread_mails(&state, 4, "chinatelecom.cn").unwrap();
    assert!(none.is_none(), "空主题组不提供会话详情");
    cleanup(&dir);
}

/// A2 列表排序：四种排序键的 ORDER BY 行为 + 星标只在默认态置顶
#[test]
fn list_orders_by_sort_key() {
    let (state, dir) = temp_state("sort");
    state.with_conn(|c| {
        c.execute_batch(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, subject_tokens, body_tokens, unread,
                                created_at, received_at, starred, thread_key)
             VALUES
             ('in','inbox','received','zeta@x.cn','Zoe','demo@chinatelecom.cn',
              'Bravo 主题','正文一','Bravo 主题','正文 一',1,100,100,1,'Bravo 主题'),
             ('in','inbox','received','alpha@x.cn','Alice','demo@chinatelecom.cn',
              'Alpha 主题','正文二','Alpha 主题','正文 二',1,200,200,0,'Alpha 主题'),
             ('in','inbox','received','beta@x.cn','Bob','demo@chinatelecom.cn',
              'Charlie 主题','正文三','Charlie 主题','正文 三',1,300,300,0,'Charlie 主题')",
        )
        .unwrap();
    });
    let base = |sort| ListQuery {
        folder: "inbox".into(),
        page: 1,
        unread_only: false,
        external_only: false,
        internal_domain: "chinatelecom.cn".into(),
        per: 10,
        sort,
        from_filter: String::new(),
        date_from: 0,
        date_to: 0,
        has_attach: false,
    };
    // 默认：星标置顶（id1），其余时间倒序（id3 → id2）
    let (items, total) = list(&state, &base(SortKey::DateDesc)).unwrap();
    assert_eq!(total, 3);
    assert_eq!(
        items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![1, 3, 2],
        "默认排序应星标置顶 + 时间倒序"
    );
    // 时间正序：放弃置顶，id1 → id2 → id3
    let (items, _) = list(&state, &base(SortKey::DateAsc)).unwrap();
    assert_eq!(
        items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "时间正序应按时间升序且不置顶"
    );
    // 发件人升序：Alice(id2) → Bob(id3) → Zoe(id1)
    let (items, _) = list(&state, &base(SortKey::FromAsc)).unwrap();
    assert_eq!(
        items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![2, 3, 1],
        "发件人排序应按姓名升序"
    );
    // 主题升序：Alpha(id2) → Bravo(id1) → Charlie(id3)
    let (items, _) = list(&state, &base(SortKey::SubjectAsc)).unwrap();
    assert_eq!(
        items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![2, 1, 3],
        "主题排序应按主题升序"
    );
    // 搜索 + 非默认排序：按所选排序而非 BM25 相关度
    let (items, _) = search(&state, "主题", &base(SortKey::DateAsc))
        .unwrap()
        .unwrap();
    assert_eq!(
        items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "检索 + 时间正序应按时间排序"
    );
    // 搜索 + 默认排序：保持 BM25（此处无显式断言顺序，只验证有结果）
    let (items, _) = search(&state, "主题", &base(SortKey::DateDesc))
        .unwrap()
        .unwrap();
    assert_eq!(items.len(), 3);
    cleanup(&dir);
}

/// A2 排序键解析：未知/缺省回退默认，大小写与空白容错
#[test]
fn sort_key_parse_falls_back_to_default() {
    assert_eq!(SortKey::parse(None), SortKey::DateDesc);
    assert_eq!(SortKey::parse(Some("")), SortKey::DateDesc);
    assert_eq!(SortKey::parse(Some("bogus")), SortKey::DateDesc);
    assert_eq!(SortKey::parse(Some(" date_asc ")), SortKey::DateAsc);
    assert_eq!(SortKey::parse(Some("from_asc")), SortKey::FromAsc);
    assert_eq!(SortKey::parse(Some("subject_asc")), SortKey::SubjectAsc);
    assert_eq!(SortKey::DateDesc.as_str(), "date_desc");
}

/// A4 全部另存：多附件落盘、磁盘同名不覆盖（自动加序号）、detached 跳过
#[test]
fn save_all_attachments_writes_dir_without_overwrite() {
    let (state, dir) = temp_state("save-all");
    let id = save_draft(&state, &draft("附件汇总", "正文")).unwrap();
    let a = dir.join("src-a");
    let b = dir.join("src-b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    // 两个同名「报告.pdf」（来自不同目录）+ 一个「数据.xlsx」
    std::fs::write(a.join("报告.pdf"), b"AAA").unwrap();
    std::fs::write(b.join("报告.pdf"), b"BBB").unwrap();
    std::fs::write(a.join("数据.xlsx"), b"CCC").unwrap();
    add_attachments(
        &state,
        id,
        &[
            a.join("报告.pdf").to_str().unwrap().into(),
            b.join("报告.pdf").to_str().unwrap().into(),
            a.join("数据.xlsx").to_str().unwrap().into(),
        ],
    )
    .unwrap();

    let dest = dir.join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    // 目标目录已有同名文件：不得覆盖，新文件改为「报告 (2).pdf」
    std::fs::write(dest.join("报告.pdf"), b"EXIST").unwrap();

    let (saved, skipped) = save_all_attachments(&state, id, dest.to_str().unwrap()).unwrap();
    assert_eq!((saved, skipped), (3, 0), "三个附件应全部保存、无跳过");
    assert_eq!(
        std::fs::read(dest.join("报告.pdf")).unwrap(),
        b"EXIST",
        "已有文件不得覆盖"
    );
    assert_eq!(std::fs::read(dest.join("报告 (2).pdf")).unwrap(), b"AAA");
    assert_eq!(std::fs::read(dest.join("报告 (3).pdf")).unwrap(), b"BBB");
    assert_eq!(std::fs::read(dest.join("数据.xlsx")).unwrap(), b"CCC");

    // 已清理（detached）的附件跳过，不阻断其他文件
    state.with_conn(|c| {
        c.execute(
            "UPDATE attachments SET detached = 1 WHERE filename = '数据.xlsx'",
            [],
        )
        .unwrap();
    });
    let (saved2, skipped2) = save_all_attachments(&state, id, dest.to_str().unwrap()).unwrap();
    assert_eq!((saved2, skipped2), (2, 1), "detached 应计入 skipped");
    cleanup(&dir);
}

/// A4 全部另存：目录不存在 / 无附件 的错误路径
#[test]
fn save_all_attachments_rejects_missing_dir_and_empty_mail() {
    let (state, dir) = temp_state("save-all-err");
    let id = save_draft(&state, &draft("无附件", "正文")).unwrap();
    let err = save_all_attachments(&state, id, dir.join("missing").to_str().unwrap()).unwrap_err();
    assert!(err.contains("目录不存在"), "{err}");
    let err = save_all_attachments(&state, id, dir.to_str().unwrap()).unwrap_err();
    assert!(err.contains("没有附件"), "{err}");
    cleanup(&dir);
}

/// B1 高级检索：发件人 / 日期区间 / 仅附件，三项可组合
#[test]
fn list_filters_by_sender_date_and_attachment() {
    let (state, dir) = temp_state("adv-filter");
    state.with_conn(|c| {
        c.execute_batch(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, subject_tokens, body_tokens, unread,
                                created_at, received_at, attach_cnt, thread_key)
             VALUES
             ('in','inbox','received','boss@chinatelecom.cn','王总','demo@chinatelecom.cn',
              '甲','x','甲','x',1,100,100,0,'甲'),
             ('in','inbox','received','market@partner.cn','合作方','demo@chinatelecom.cn',
              '乙','y','乙','y',1,200,200,2,'乙'),
             ('in','inbox','received','ops@partner.cn','运维','demo@chinatelecom.cn',
              '丙','z','丙','z',1,300,300,0,'丙')",
        )
        .unwrap();
    });
    let q = |from: &str, df: i64, dt: i64, att: bool| ListQuery {
        folder: "inbox".into(),
        page: 1,
        unread_only: false,
        external_only: false,
        internal_domain: "chinatelecom.cn".into(),
        per: 10,
        sort: SortKey::DateDesc,
        from_filter: from.into(),
        date_from: df,
        date_to: dt,
        has_attach: att,
    };
    let ids = |q: &ListQuery| {
        list(&state, q)
            .unwrap()
            .0
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>()
    };
    // 发件人包含 partner → id2 + id3
    assert_eq!(ids(&q("partner", 0, 0, false)), vec![3, 2]);
    // 姓名也参与匹配（「王总」）
    assert_eq!(ids(&q("王总", 0, 0, false)), vec![1]);
    // 日期区间 150..250 → id2
    assert_eq!(ids(&q("", 150, 250, false)), vec![2]);
    // 仅附件 → id2
    assert_eq!(ids(&q("", 0, 0, true)), vec![2]);
    // 组合：partner + 有附件 → id2
    assert_eq!(ids(&q("partner", 0, 0, true)), vec![2]);
    // 无命中
    assert!(ids(&q("nobody", 0, 0, false)).is_empty());
    cleanup(&dir);
}

/// B1 日期解析：YYYY-MM-DD → 本地当天 0 点 / 23:59:59；非法输入 None
#[test]
fn date_epoch_of_parses_day_bounds() {
    let start = date_epoch_of("2026-11-04", false).expect("合法日期");
    let end = date_epoch_of("2026-11-04", true).expect("合法日期");
    assert_eq!(end - start, 86_399, "结束应到当天 23:59:59");
    assert!(
        date_epoch_of("2026-02-30", false).is_none(),
        "非法日期应 None"
    );
    assert!(date_epoch_of("not-a-date", false).is_none());
    assert!(date_epoch_of("", false).is_none());
    assert!(
        date_epoch_of("2026-11-05", false).unwrap() > start,
        "后一天应更晚"
    );
}

/// B2 邮件模板 CRUD：新增/更新/删除、校验与预览摘录
#[test]
fn template_crud_and_validation() {
    use crate::mail::templates;
    let (state, dir) = temp_state("tpl");
    assert!(templates::list(&state).is_empty(), "初始无模板");
    let id = templates::save(&state, None, "  收到请回复  ", "收到，我核对后回复您。", "").unwrap();
    let list = templates::list(&state);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "收到请回复", "名称应 trim");
    assert!(list[0].preview.contains("收到"));
    // 更新
    templates::save(&state, Some(id), "改名", "新内容", "<p>新内容</p>").unwrap();
    let t = templates::get(&state, id).unwrap();
    assert_eq!(t.name, "改名");
    assert_eq!(t.body_html, "<p>新内容</p>");
    // 校验
    assert!(
        templates::save(&state, None, "  ", "x", "").is_err(),
        "名称必填"
    );
    assert!(
        templates::save(&state, None, "空内容", "", "").is_err(),
        "内容必填"
    );
    assert!(
        templates::save(&state, None, &"名".repeat(61), "x", "").is_err(),
        "名称超长"
    );
    assert!(
        templates::save(&state, Some(9999), "不存在", "x", "").is_err(),
        "更新不存在的 id 应报错"
    );
    // 删除幂等
    templates::delete(&state, id).unwrap();
    assert!(templates::get(&state, id).is_none());
    templates::delete(&state, id).unwrap();
    cleanup(&dir);
}

/// 模板 HTML 存储型 XSS（审计 R9）：入库前必须与草稿/收件同口径净化。
/// 模板 body_html 会被原样注入编辑器与该设置 pane，事件属性/脚本一旦落库
/// 就是持久化执行面；净化放在 `templates::save` 内部，任何调用方都绕不过。
#[test]
fn template_html_is_sanitized_on_save() {
    use crate::mail::templates;
    let (state, dir) = temp_state("tpl-xss");
    let id = templates::save(
        &state,
        None,
        "恶意模板",
        "正文",
        "<p onclick=\"alert(1)\">hi</p><script>alert(2)</script><img src=x onerror=alert(3)>",
    )
    .unwrap();
    let t = templates::get(&state, id).unwrap();
    assert!(!t.body_html.contains("onerror"), "{}", t.body_html);
    assert!(!t.body_html.contains("<script"), "{}", t.body_html);
    assert!(!t.body_html.contains("onclick"), "{}", t.body_html);
    assert!(
        t.body_html.contains("hi"),
        "正文文本应保留：{}",
        t.body_html
    );
    cleanup(&dir);
}

/// B2 模板内容上限：单字段 100KB
#[test]
fn template_rejects_oversized_body() {
    use crate::mail::templates;
    let (state, dir) = temp_state("tpl-big");
    let big = "x".repeat(100 * 1024 + 1);
    let err = templates::save(&state, None, "大", &big, "").unwrap_err();
    assert!(err.contains("过长"), "{err}");
    cleanup(&dir);
}

/// B4 .eml 重建：头字段（RFC 2047 主题）、alternative/mixed 结构、附件 base64 与 CRLF
#[test]
fn build_eml_rebuilds_headers_body_and_attachments() {
    use crate::mail::eml;
    let (state, dir) = temp_state("b4-eml");
    let id = save_draft_with_html(
        &state,
        &draft("季度报表 Q3", "纯文本版"),
        "<p>HTML 版正文</p>",
    )
    .unwrap();
    let f = dir.join("明细.txt");
    std::fs::write(&f, b"alpha").unwrap();
    add_attachments(&state, id, &[f.to_str().unwrap().into()]).unwrap();

    let bytes = eml::build_eml(&state, id).unwrap();
    let raw = String::from_utf8(bytes).unwrap();
    assert!(
        raw.contains("From: =?UTF-8?B?") && raw.contains("<demo@chinatelecom.cn>\r\n"),
        "非 ASCII 显示名应 RFC 2047 编码：{raw}"
    );
    assert!(raw.contains("To: peer@x.cn\r\n"), "{raw}");
    assert!(
        raw.contains("Subject: =?UTF-8?B?") && raw.contains("?=\r\n"),
        "非 ASCII 主题应 RFC 2047 编码：{raw}"
    );
    assert!(raw.contains("MIME-Version: 1.0\r\n"), "{raw}");
    assert!(
        raw.contains("multipart/alternative"),
        "有 HTML 应双版本：{raw}"
    );
    assert!(
        raw.contains("multipart/mixed"),
        "带附件应 mixed 包裹：{raw}"
    );
    assert!(raw.contains("Content-Disposition: attachment"), "{raw}");
    assert!(
        raw.contains("filename*=UTF-8''%E6%98%8E%E7%BB%86.txt"),
        "中文文件名应用 RFC 2231 编码：{raw}"
    );
    assert!(
        raw.contains("filename=\"__.txt\""),
        "应带 ASCII 回退名：{raw}"
    );
    // base64("alpha") = YWxwaGE=
    assert!(raw.contains("YWxwaGE="), "附件正文应 base64：{raw}");
    assert!(raw.contains("text/html"), "HTML 部分应保留：{raw}");
    assert!(
        raw.split("\r\n").all(|l| l.len() <= 998),
        "各行不得超过 RFC 5322 行宽上限"
    );
    assert!(
        raw.ends_with("--\r\n") || raw.ends_with("--"),
        "应以结束边界收尾"
    );
    cleanup(&dir);
}

/// B4 导出文件名清洗：非法字符替换、空主题回落 id、超长截断
#[test]
fn eml_suggested_filename_sanitizes() {
    use crate::mail::eml;
    assert_eq!(
        eml::suggested_filename("报表/2026:Q3?", 7),
        "报表 2026 Q3.eml"
    );
    assert_eq!(eml::suggested_filename("   ", 7), "corpmail-7.eml");
    let long = eml::suggested_filename(&"主".repeat(100), 7);
    assert_eq!(long.chars().count(), 64, "60 字主题 + .eml");
}

/// B6 作为附件转发：首次保存生成 message/rfc822 附件，重存不重复添加
#[test]
fn attach_eml_added_once_on_first_save() {
    use crate::mail::eml;
    let (state, dir) = temp_state("b6-attach-eml");
    let src = save_draft(&state, &draft("原始邮件", "原正文")).unwrap();
    let dst = save_draft(&state, &draft("Fwd: 原始邮件", "")).unwrap();
    let name = eml::suggested_filename("原始邮件", src);
    let bytes = eml::build_eml(&state, src).unwrap();
    add_attachment_bytes(&state, dst, &name, "message/rfc822", &bytes).unwrap();

    let d = get(&state, dst).unwrap();
    assert_eq!(d.attach_cnt, 1);
    assert_eq!(d.attachments[0].filename, name);
    assert_eq!(d.attachments[0].mime, "message/rfc822");

    // 重复附加前先做同名判定（与路由同口径），不得出现第二份
    let already = get(&state, dst)
        .unwrap()
        .attachments
        .iter()
        .any(|a| a.mime == "message/rfc822" && a.filename == name);
    assert!(already, "重存应能识别已附加");
    cleanup(&dir);
}

/// 安全：附件文件名净化（收件 MIME 头 filename 可控）——控制字符替换、
/// 路径/保留字符替换、按字节截断 200、空名回落 attachment
#[test]
fn safe_filename_strips_control_chars_and_bounds_length() {
    assert_eq!(safe_filename("a\u{0}b\r\nc.pdf"), "a_b__c.pdf");
    assert_eq!(safe_filename("../../etc/passwd"), ".._.._etc_passwd");
    assert_eq!(safe_filename("正常 报告.xlsx"), "正常 报告.xlsx");
    // 超长：中文 3 字节/字，按字节截到 ≤200
    let long = "长".repeat(100);
    let got = safe_filename(&long);
    assert!(got.len() <= 200, "应截断到 200 字节内：{}", got.len());
    assert!(got.chars().all(|c| c == '长'));
    // 空输入 → attachment
    assert_eq!(safe_filename(""), "attachment");
}

#[test]
fn html_escape_also_escapes_single_quote() {
    // 单引号属性上下文（如 hx-vals='...'）不能只靠 autoescape 二次转义
    assert_eq!(
        html_escape("<a href='x'>&\"</a>"),
        "&lt;a href=&#39;x&#39;&gt;&amp;&quot;&lt;/a&gt;"
    );
}

/// 分块修复的游标语义（审计 R52）：跨多批全部处理完、游标落在最大 id、
/// 再跑只处理新增行（不重扫历史）。用 1200 行覆盖 3 批（REPAIR_CHUNK=500）。
#[test]
fn chunked_repair_covers_all_batches_and_resumes_from_cursor() {
    let dir = std::env::temp_dir().join(format!("corpmail-repair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut conn = crate::db::open(&dir.join("corpmail.db")).unwrap();
    // GBK「会议通知」按字节当 latin1 读入 = 典型 mojibake 主题
    let mojibake: String = [0xBBu8, 0xE1, 0xD2, 0xE9, 0xCD, 0xA8, 0xD6, 0xAA]
        .iter()
        .map(|b| *b as char)
        .collect();
    let insert = |conn: &rusqlite::Connection, n: usize| {
        for i in 0..n {
            conn.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, created_at)
                 VALUES ('in','inbox','received',?1,'me@x.cn',?2,'','',1)",
                rusqlite::params![format!("m{i}@x.cn"), mojibake],
            )
            .unwrap();
        }
    };
    insert(&conn, 1200);
    repair_gbk_subjects(&mut conn).unwrap();
    let fixed: i64 = conn
        .query_row(
            "SELECT count(*) FROM emails WHERE subject = '会议通知'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fixed, 1200, "三轮批处理后应全部修好");
    let cursor = repair_mark_get(&conn, "gbk_subjects");
    assert_eq!(cursor, 1200, "游标应停在已处理的最大 id");
    // 新增一封后再跑：只处理新增（游标续跑，不重扫历史）
    insert(&conn, 1);
    repair_gbk_subjects(&mut conn).unwrap();
    let fixed: i64 = conn
        .query_row(
            "SELECT count(*) FROM emails WHERE subject = '会议通知'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fixed, 1201, "新增行也应被修复");
    assert_eq!(repair_mark_get(&conn, "gbk_subjects"), 1201);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 附件读取上限（审计 R55）：超大文件在读之前就被 metadata 挡住
/// （用稀疏文件造大小，不真占磁盘，也不进内存）
#[test]
fn oversized_attachment_file_is_rejected_before_read() {
    let dir = std::env::temp_dir().join(format!("corpmail-attcap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("huge.gz");
    let f = std::fs::File::create(&big).unwrap();
    f.set_len(300 * 1024 * 1024).unwrap(); // 300 MB 稀疏文件
    drop(f);
    assert!(
        read_attach_file(&big).is_none(),
        "超过上限的附件文件应在读取前被判失败"
    );
    // 正常小文件照常读得到
    let small = dir.join("small.gz");
    std::fs::write(&small, b"abc").unwrap();
    assert_eq!(read_attach_file(&small).as_deref(), Some(&b"abc"[..]));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 读不了 ≠ 已清理（审计 R58）：权限不足必须与「文件不存在」区分开，
/// 否则 UI 报「附件已被清理」，用户会去重新收件而不是修权限。
/// 只在 Unix 跑：Windows 的 chmod 语义不同（只读位不阻止读取）
#[cfg(unix)]
#[test]
fn attachment_read_distinguishes_permission_from_missing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("corpmail-attperm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let locked = dir.join("locked.gz");
    std::fs::write(&locked, b"data").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    match read_attach_gz(&locked) {
        Err(AttachIo::Unreadable(why)) => {
            assert!(why.contains("权限"), "应说明权限问题而不是笼统失败：{why}")
        }
        other => panic!("权限不足应判 Unreadable，实际：{:?}", other.is_ok()),
    }
    // 不存在 → Missing
    assert!(matches!(
        read_attach_gz(&dir.join("nope.gz")),
        Err(AttachIo::Missing)
    ));
    // 还原权限后清理（否则 remove_dir_all 会失败）
    let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导出（审计 R61）：base64 逐块编码后仍按 76 列折行，且拼回来解码与原字节一致。
/// 旧实现把整段编码串拼成 String 再 join，单个大附件要占 4 份内存；改为按
/// 57 字节（= 76 字符、无 padding）切块直接追加，此用例钉住编码正确性
#[test]
fn eml_attachment_base64_wraps_at_76_cols_and_round_trips() {
    use crate::mail::eml;
    use base64::Engine as _;
    let (state, dir) = temp_state("r61-eml-b64");
    let id = save_draft(&state, &draft("多行附件", "正文")).unwrap();
    // 300 字节：跨 5 个 57 字节块，末块带 padding
    let payload: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
    add_attachment_bytes(&state, id, "blob.bin", "application/octet-stream", &payload).unwrap();

    let raw = String::from_utf8(eml::build_eml(&state, id).unwrap()).unwrap();
    let after = raw
        .split("Content-Disposition: attachment")
        .nth(1)
        .expect("应写出附件头");
    let block = after.split("\r\n\r\n").nth(1).expect("附件头后应有空行");
    let mut lines: Vec<&str> = Vec::new();
    for line in block.split("\r\n") {
        if line.starts_with("--") {
            break;
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    assert!(lines.len() > 1, "300 字节必须折成多行：{lines:?}");
    assert!(
        lines.iter().all(|l| l.len() <= 76),
        "单行超 76 列：{lines:?}"
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(lines.concat())
        .unwrap();
    assert_eq!(decoded, payload, "逐块编码后拼回必须与原字节一致");
    cleanup(&dir);
}

/// 导出（审计 R61）：附件已被清理时导出照样成功，且不再声明 multipart/mixed
/// （预检只查元数据/存在性，不读字节；写阶段读不到就跳过该附件）
#[test]
fn eml_export_skips_cleaned_attachment() {
    use crate::mail::eml;
    let (state, dir) = temp_state("r61-eml-detached");
    let id = save_draft(&state, &draft("带清理附件", "正文")).unwrap();
    add_attachment_bytes(&state, id, "big.bin", "application/octet-stream", b"abc").unwrap();
    state
        .with_conn(|conn| {
            conn.execute(
                "UPDATE attachments SET detached = 1 WHERE email_id = ?1",
                [id],
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();

    let raw = String::from_utf8(eml::build_eml(&state, id).unwrap()).unwrap();
    assert!(raw.starts_with("From: "), "导出应成功：{raw}");
    assert!(
        !raw.contains("multipart/mixed"),
        "无可用附件时不该声明 mixed：{raw}"
    );
    assert!(
        !raw.contains("Content-Disposition: attachment"),
        "已清理的附件不应出现在导出里"
    );
    cleanup(&dir);
}

/// 审计 R62：收件重试与已读回写共用同一指数退避口径
#[test]
fn backoff_secs_is_exponential_and_capped() {
    // 收件轮内重试：15s 起翻倍，120s 上限（旧实现 15·n 线性）
    assert_eq!(backoff_secs(1, 15, 120), 15);
    assert_eq!(backoff_secs(2, 15, 120), 30);
    assert_eq!(backoff_secs(3, 15, 120), 60);
    assert_eq!(backoff_secs(4, 15, 120), 120);
    assert_eq!(backoff_secs(30, 15, 120), 120, "必须封顶，不能无限翻倍");
    // 已读回写：60s 起 / 600s 上限（与 seen::backoff 序列一致）
    assert_eq!(backoff_secs(1, 60, 600), 60);
    assert_eq!(backoff_secs(3, 60, 600), 240);
    assert_eq!(backoff_secs(5, 60, 600), 600);
}
