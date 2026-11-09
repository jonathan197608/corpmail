//! MD 导出单元测试（白盒：use super::* 继承 md 模块作用域）

use super::*;
use crate::state::AppState;
use std::path::PathBuf;

fn temp_state(tag: &str) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("corpmail-md-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (AppState::open(&dir), dir)
}

fn cleanup(dir: &PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

/// 段落不粘连：p 结束补换行；内联标签剥除；实体解码复用 strip_html 口径
#[test]
fn html_to_md_blocks_and_entities() {
    let got = html_to_md_text("<p>第一段</p><p>第二段</p><div>a<b>加粗</b>c</div>");
    assert_eq!(got, "第一段\n\n第二段\n\na加粗c");
    // 实体：命名 + 数字（十进制/十六进制）
    let got = html_to_md_text("<p>&nbsp;A&nbsp;&amp;&nbsp;B</p><p>&#20320;&#x597D;</p>");
    assert_eq!(got, "A & B\n\n你好");
}

/// 列表项、标题层级、表格近似结构：段落/列表间空行分隔，列表项/表格行紧凑
#[test]
fn html_to_md_lists_headings_table() {
    let got = html_to_md_text("<p>前言</p><ul><li>苹果</li><li>香蕉</li></ul>");
    assert!(
        got.contains("前言\n\n- 苹果\n- 香蕉"),
        "段落与列表空行分隔、项间单换行：{got}"
    );
    let got = html_to_md_text("<h1>标题一</h1><h2>标题二</h2>");
    assert!(
        got.contains("## 标题一") && got.contains("### 标题二"),
        "{got}"
    );
    let got = html_to_md_text(
        "<table><tr><td>客户</td><td>数量</td></tr><tr><td>A</td><td>200</td></tr></table>",
    );
    // 标准 GFM 表格：首行表头 + 分隔行 + 数据行
    assert_eq!(
        got, "| 客户 | 数量 |\n| --- | --- |\n| A | 200 |",
        "表格应转为标准 GFM：{got}"
    );
}

/// 标准 GFM 表格的边界：thead/th、单元格内联标签与实体、管道转义、
/// 列数不齐补齐、嵌套表格退化、未闭合表格尽力输出
#[test]
fn html_to_md_gfm_table_edges() {
    let got = html_to_md_text(
        "<table><thead><tr><th>名称</th><th>值</th></tr></thead>\
         <tbody><tr><td>a|b</td><td><b>1</b>&amp;2</td></tr>\
         <tr><td>仅一列</td></tr></tbody></table>",
    );
    assert_eq!(
        got, "| 名称 | 值 |\n| --- | --- |\n| a\\|b | 1&2 |\n| 仅一列 |  |",
        "单元格清洗与列补齐：{got}"
    );
    // 单元格内换行折叠为空格（不破坏行结构）
    let got = html_to_md_text("<table><tr><td>x<br>y</td></tr><tr><td>z</td></tr></table>");
    assert_eq!(got, "| x y |\n| --- |\n| z |", "{got}");
    // 嵌套表格：外层结构保留，内层文字并入单元格
    let got = html_to_md_text(
        "<table><tr><td>外<table><tr><td>内</td></tr></table></td><td>b</td></tr></table>",
    );
    assert!(
        got.starts_with("| 外内 | b |"),
        "嵌套应退化不破坏外层：{got}"
    );
    assert_eq!(got.matches("| --- |").count(), 1, "仅一个表头分隔行：{got}");
    // 未闭合表格：已收集的行尽力输出
    let got = html_to_md_text("<table><tr><td>甲</td><td>乙</td></tr>");
    assert_eq!(got, "| 甲 | 乙 |\n| --- | --- |", "{got}");
}

/// 未闭合 `<` 不吞正文；连续空行压缩；首尾修剪
#[test]
fn html_to_md_tolerates_malformed() {
    let got = html_to_md_text("<p>正常</p>未闭合<残留文本");
    assert!(got.contains("未闭合<残留文本"), "{got}");
    let got = html_to_md_text("<p>a</p><p></p><p></p><p></p><p>b</p>");
    assert_eq!(got, "a\n\nb");
}

/// 值转义是合法 YAML：主题含冒号/引号/换行时 front matter 结构不被破坏
#[test]
fn front_matter_escapes_yaml_specials() {
    let (state, dir) = temp_state("front-matter");
    // 主题含 YAML 危险字符（冒号+引号+换行）
    let input = crate::mail::DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: "c@x.cn".into(),
        bcc_addrs: String::new(),
        subject: "主题: 带\"引号\"\n换行".into(),
        body_text: "正文内容".into(),
        in_reply_to: None,
        scheduled_at: 0,
    };
    let id = crate::mail::save_draft(&state, &input).unwrap();
    let md = build_md(&state, id).unwrap();
    let head = md.split("---").nth(1).unwrap_or("");
    // 冒号必须被引号包裹（值整体是 JSON 字符串），换行转义为 \n 字面
    assert!(
        head.contains("subject: \"主题: 带\\\"引号\\\"\\n换行\""),
        "{head}"
    );
    // 正文段落保留
    assert!(md.contains("## 正文"), "{md}");
    assert!(md.contains("正文内容"), "{md}");
    assert!(!md.contains("folder: \"\""), "草稿 folder 应为 draft：{md}");
    cleanup(&dir);
}

/// HTML 邮件正文经块级转换导出；纯文本邮件原样；附件进 front matter 清单
#[test]
fn build_md_body_kinds_and_attachments() {
    let (state, dir) = temp_state("md-kinds");
    // HTML 正文：段落 + 列表
    let input = crate::mail::DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        bcc_addrs: String::new(),
        subject: "HTML 邮件".into(),
        body_text: String::new(),
        in_reply_to: None,
        scheduled_at: 0,
    };
    let html = "<p>各位领导：</p><ul><li>清单见附件</li><li>请查收</li></ul>";
    let id = crate::mail::save_draft_with_html(&state, &input, html).unwrap();
    let md = build_md(&state, id).unwrap();
    assert!(
        md.contains("各位领导：\n\n- 清单见附件\n- 请查收"),
        "HTML 应转块级结构：{md}"
    );
    // 附件清单：手动字节入库（.eml 附件形态）
    crate::mail::add_attachment_bytes(&state, id, "报告.pdf", "application/pdf", &[1, 2, 3])
        .unwrap();
    let md = build_md(&state, id).unwrap();
    assert!(md.contains("attachments:\n  - \"报告.pdf\""), "{md}");
    // 纯文本邮件：正文原样、无 attachments 键
    let plain = crate::mail::DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        bcc_addrs: String::new(),
        subject: "纯文本".into(),
        body_text: "第一行\n第二行".into(),
        in_reply_to: None,
        scheduled_at: 0,
    };
    let id2 = crate::mail::save_draft(&state, &plain).unwrap();
    let md2 = build_md(&state, id2).unwrap();
    assert!(md2.contains("第一行\n第二行"), "{md2}");
    assert!(!md2.contains("attachments:"), "无附件不应有该键：{md2}");
    // 不存在的邮件
    assert!(build_md(&state, 9999).is_err());
    cleanup(&dir);
}

/// 推荐文件名：与 .eml 同源清洗，仅扩展名不同
#[test]
fn suggested_filename_matches_eml_base() {
    assert_eq!(suggested_filename("会议通知", 7), "会议通知.md");
    assert_eq!(suggested_filename("", 7), "corpmail-7.md");
    let got = suggested_filename("a/b:c?d", 9);
    assert!(got.ends_with(".md"), "{got}");
    assert!(
        !got.contains('/') && !got.contains(':'),
        "路径/保留字符应清洗：{got}"
    );
}
