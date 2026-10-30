//! ui_router 模块单元测试（自 src/ui/router.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::secrets::GetResult;
use std::collections::HashMap;

/// 假资源表（路径 → (MIME, 内容)）
struct MapAssets(HashMap<&'static str, (&'static str, &'static str)>);

impl Assets for MapAssets {
    fn get(&self, path: &str) -> Option<(String, Vec<u8>)> {
        self.0
            .get(path)
            .map(|(mime, body)| ((*mime).to_string(), body.as_bytes().to_vec()))
    }
}

fn assets() -> MapAssets {
    let mut map = HashMap::new();
    map.insert("app.css", ("text/css; charset=utf-8", "body{color:#333}"));
    map.insert("htmx.min.js", ("text/javascript", "var htmx={}"));
    map.insert("../Cargo.toml", ("text/plain", "[package]")); // 验证防穿越守卫
    MapAssets(map)
}

/// 假连接测试器（避免单测走网络）
struct FakeTester {
    smtp_ok: bool,
    imap_ok: bool,
}

impl ConnTester for FakeTester {
    fn smtp(&self, _host: &str, _port: u16, _account: &str, _password: &str) -> TestOutcome {
        TestOutcome {
            ok: self.smtp_ok,
            elapsed_ms: 3,
            detail: if self.smtp_ok {
                String::new()
            } else {
                "模拟失败".into()
            },
        }
    }

    fn imap(&self, _host: &str, _port: u16, _account: &str, _password: &str) -> TestOutcome {
        TestOutcome {
            ok: self.imap_ok,
            elapsed_ms: 4,
            detail: if self.imap_ok {
                String::new()
            } else {
                "模拟失败".into()
            },
        }
    }
}

fn temp_state(tag: &str) -> (AppState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("mbx-router-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (AppState::open(&dir), dir)
}

fn cleanup(dir: &std::path::PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

fn req(method: &str, path: &str, body: &[u8]) -> Req {
    Req {
        method: method.to_string(),
        path: path.to_string(),
        query: String::new(),
        is_htmx_request: false,
        body: body.to_vec(),
    }
}

/// 带查询串的 GET 构造（M2 列表 folder/page/unread 取参）
fn req_q(method: &str, path: &str, query: &str, body: &[u8]) -> Req {
    Req {
        query: query.to_string(),
        ..req(method, path, body)
    }
}

fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish().into_bytes()
}

fn body(resp: &Resp) -> String {
    String::from_utf8(resp.body.clone()).expect("响应为 UTF-8 HTML")
}

const WIZARD_FORM: &[(&str, &str)] = &[
    ("account_prefix", "unit.router"),
    ("password", "pw-test"),
    ("display_name", "单测用户"),
    ("smtp_host", "smtp.test.cn"),
    ("smtp_port", "465"),
    ("recv_host", "imap.test.cn"),
    ("recv_port", "993"),
];

#[test]
fn fresh_root_shows_welcome() {
    let (state, dir) = temp_state("root-fresh");
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 200);
    let html = body(&resp);
    assert!(html.contains("欢迎使用 CorpMail"), "未配置首启应渲染欢迎页");
    assert!(html.contains("开始配置"), "欢迎页应有强制配置入口");
    cleanup(&dir);
}

#[test]
fn configured_root_shows_main_frame() {
    let (state, dir) = temp_state("root-cfg");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(html.contains("企业邮箱客户端"), "已配置应渲染主界面框架");
    assert!(html.contains("收件箱"), "主界面应展示当前文件夹标题");
    assert!(html.contains("这里还没有邮件"), "无邮件时应渲染空态");
    assert!(html.contains("demo@chinatelecom.cn"), "侧栏应展示完整账号");
    assert!(html.contains("撰写邮件"), "侧栏撰写入口应可用");
    cleanup(&dir);
}

#[test]
fn list_page_shows_drafts_with_sidebar_counts() {
    let (state, dir) = temp_state("m2-list");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let draft = |s: &str| mail::DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        subject: s.into(),
        body_text: "配案正文预览".into(),
        in_reply_to: None,
    };
    mail::save_draft(&state, &draft("第一封草稿主题")).unwrap();
    mail::save_draft(&state, &draft("第二封草稿主题")).unwrap();
    let resp = route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=draft", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(html.contains("第一封草稿主题"), "列表应含草稿主题");
    assert!(html.contains("第二封草稿主题"), "列表应含第二封");
    assert!(html.contains("配案正文预览"), "列表应含正文预览");
    assert!(html.contains("草稿"), "列表应渲染 folder 标题与草稿徽标");
    assert!(!html.contains("这里还没有邮件"), "有数据不应空态");
    cleanup(&dir);
}

#[test]
fn root_search_highlights_and_falls_back_on_stopwords() {
    let (state, dir) = temp_state("m5-search");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let draft = |s: &str| mail::DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        subject: s.into(),
        body_text: "配案正文预览".into(),
        in_reply_to: None,
    };
    mail::save_draft(&state, &draft("激励配案通知")).unwrap();
    mail::save_draft(&state, &draft("无关主题邮件")).unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 检索命中：命中词高亮、未命中不出列表、分页基址携带 q
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=all&q=%E6%BF%80%E5%8A%B1", b""),
        &tester,
    ));
    assert!(html.contains("<mark>激励</mark>"), "命中词应高亮：{html}");
    assert!(!html.contains("无关主题"), "未命中邮件不应出现在列表");
    assert!(
        html.contains("q=%E6%BF%80%E5%8A%B1"),
        "分页基址应携带检索词"
    );
    // 全停用词：回退普通列表（两封都在）
    let html2 = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=all&q=%E7%9A%84%E4%BA%86", b""),
        &tester,
    ));
    assert!(
        html2.contains("无关主题邮件"),
        "无效词应回退普通列表：{html2}"
    );
    // 无命中：空态为检索文案
    let html3 = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=all&q=zzqqxy", b""),
        &tester,
    ));
    assert!(html3.contains("没有匹配的邮件"), "搜索空态文案：{html3}");
    cleanup(&dir);
}

#[test]
fn unread_toggle_restores_full_list() {
    let (state, dir) = temp_state("m5-unread");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    // inbox：未读 + 已读各一封（SQL 直插，绕开收件链路）
    state.with_conn(|c| {
        for unread in [1, 0] {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in', 'inbox', 'received', 'a@x.cn', 'b@x.cn', '主题', '正文',
                         '主题', '正文', ?1, 1, 1)",
                [unread],
            )
            .unwrap();
        }
    });
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 未开态：开关进入链接带 unread=1
    let plain = body(&route(&state, &assets(), &req("GET", "/", b""), &tester));
    assert!(
        plain.contains("unread=1&page=1"),
        "未开态开关链接应带 unread=1：{plain}"
    );
    assert!(plain.contains("只看未读"));
    // 开启态：开关为恢复链接（不带 unread 参数）+ 明确文字
    let unread = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "unread=1", b""),
        &tester,
    ));
    assert!(unread.contains("显示全部"), "开启态应有恢复文字：{unread}");
    // 恢复链接不带 unread 参数（minijinja 按规范将 / 转义为 &#x2f;，浏览器解码后
    // 即 /?page=1；断言按渲染原文精确匹配）
    assert!(
        unread.contains(r#"href="&#x2f;?page=1""#),
        "恢复链接不得再携带 unread 参数：{unread}"
    );
    assert!(
        !unread.contains("unread=1"),
        "开启态整页不应再出现 unread=1 链接：{unread}"
    );
    // 恢复链接可达且回到全部
    let restored = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "page=1", b""),
        &tester,
    ));
    assert!(
        restored.matches("mail-row").count() >= 2,
        "恢复后应回到全部列表"
    );
    cleanup(&dir);
}

#[test]
fn attachment_view_serves_bytes_with_safe_mime() {
    let (state, dir) = temp_state("m5-preview");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let id = mail::save_draft(
        &state,
        &mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            subject: "预览用例".into(),
            body_text: "见附件".into(),
            in_reply_to: None,
        },
    )
    .unwrap();
    let png = dir.join("截图.png");
    std::fs::write(&png, b"fake-png-bytes").unwrap();
    let txt = dir.join("说明.txt");
    std::fs::write(&txt, "纯文本内容").unwrap();
    let zip = dir.join("打包.zip");
    std::fs::write(&zip, b"zip-bytes").unwrap();
    mail::add_attachments(
        &state,
        id,
        &[
            png.to_str().unwrap().into(),
            txt.to_str().unwrap().into(),
            zip.to_str().unwrap().into(),
        ],
    )
    .unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 详情页：png/txt 有预览按钮，zip 无
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}"), b""),
        &tester,
    ));
    assert!(
        html.contains("attachments/1/view"),
        "png 应有预览按钮：{html}"
    );
    assert!(
        html.contains("attachments/2/view"),
        "txt 应有预览按钮：{html}"
    );
    assert!(
        html.contains("attachments/3/view"),
        "zip 也应有预览按钮（点击提示不支持）：{html}"
    );
    // 模板重复块回归防护（email.html 曾出现 size/另存为双渲染）：
    // 详情页每个附件恰好一个"另存为"按钮与一个大小标签
    assert_eq!(
        html.matches("另存为").count(),
        3,
        "三个附件应恰有 3 个另存为按钮（不得重复渲染）：{html}"
    );
    assert_eq!(
        html.matches("class=\"att-size\"").count(),
        3,
        "三个附件应恰有 3 个大小标签（不得重复渲染）：{html}"
    );
    // GET 预览：原始字节 + 原 MIME + nosniff
    let resp = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}/attachments/1/view"), b""),
        &tester,
    );
    assert_eq!(resp.status, 200);
    assert!(
        resp.content_type.contains("image/png"),
        "{}",
        resp.content_type
    );
    assert_eq!(resp.body, b"fake-png-bytes");
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "X-Content-Type-Options" && v == "nosniff"),
        "应有 nosniff 头"
    );
    // 文本类强制 text/plain（防 text/html 内嵌脚本）
    let resp2 = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}/attachments/2/view"), b""),
        &tester,
    );
    assert_eq!(resp2.content_type, "text/plain; charset=utf-8");
    assert_eq!(resp2.body, "纯文本内容".as_bytes());
    // 不支持内嵌的类型（zip）→ octet-stream + 强制下载，防伪造 MIME 内嵌渲染
    let resp_zip = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}/attachments/3/view"), b""),
        &tester,
    );
    assert_eq!(resp_zip.content_type, "application/octet-stream");
    assert!(
        resp_zip
            .extra_headers
            .iter()
            .any(|(k, v)| k == "Content-Disposition" && v == "attachment"),
        "非白名单类型应强制下载"
    );
    // 不存在的附件 → 404
    let resp3 = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}/attachments/99/view"), b""),
        &tester,
    );
    assert_eq!(resp3.status, 404);
    cleanup(&dir);
}

#[test]
fn storage_pane_shows_estimate_and_clean_runs() {
    let (state, dir) = temp_state("m5-clean");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    // 过期 inbox 邮件（30 天策略，40 天前收到）
    let now = mail::now_epoch();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails (direction, folder, status, subject, from_addr, to_addrs,
                                 subject_tokens, body_tokens, created_at)
             VALUES ('in', 'inbox', 'received', '过期主题', 'a@x.cn', 'b@x.cn',
                     '过期 主题', '正文', ?1)",
            [now - 40 * 86_400],
        )
        .unwrap();
    });
    state.set_batch(&[(keys::KEEP_INBOX_DAYS, "30")]).unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };

    // pane：预估区展示待清理明细
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/panes/storage", b""),
        &tester,
    ));
    assert!(html.contains("预计可释放"), "应有预估区：{html}");
    assert!(html.contains("过期邮件 1 封"), "预估应含过期邮件数：{html}");
    assert!(html.contains("立即清理"), "应有清理按钮：{html}");

    // 手动清理：报告回显 + 预估归零
    let html2 = body(&route(
        &state,
        &assets(),
        &req("POST", "/settings/storage/clean", b""),
        &tester,
    ));
    assert!(html2.contains("清理完成"), "应展示清理报告：{html2}");
    assert!(html2.contains("过期邮件 1"), "报告应含删除数：{html2}");
    assert!(
        !html2.contains("过期邮件 1 封"),
        "清理后预估应归零：{html2}"
    );
    let left: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(left, 0, "过期邮件应已被清理");
    cleanup(&dir);
}

#[test]
fn storage_move_returns_pane_with_pending_dir() {
    let (state, dir) = temp_state("m5-move");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let dest = std::env::temp_dir().join(format!("mbx-router-move-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // POST 迁移成功 → 回 pane 片段：待生效目录行 + 新路径
    let html = body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/storage/move",
            &form(&[("dest", dest.to_str().unwrap())]),
        ),
        &tester,
    ));
    assert!(
        html.contains("待生效目录"),
        "成功后 pane 应显示待生效目录行：{html}"
    );
    // minijinja 按规范将 / 转义为 &#x2f;（浏览器解码后即原路径），断言按渲染原文比较
    let dest_escaped = dest.to_str().unwrap().replace('/', "&#x2f;");
    assert!(html.contains(&dest_escaped), "待生效目录路径应展示：{html}");
    assert!(html.contains("重启应用后启用"), "应提示重启生效：{html}");
    // 指针已写锚点库，启动解析定位新目录
    assert_eq!(
        AppState::resolve_start_dir(&dir),
        dest,
        "重启后应定位新目录"
    );
    // 失败路径：目标已占用（已有 corpmail.db）→ pane 顶部错误区
    let html2 = body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/storage/move",
            &form(&[("dest", dest.to_str().unwrap())]),
        ),
        &tester,
    ));
    assert!(
        html2.contains("corpmail.db") && html2.contains("compose-note err"),
        "失败应显示错误原因：{html2}"
    );
    let _ = std::fs::remove_dir_all(&dest);
    cleanup(&dir);
}

#[test]
fn compose_save_and_edit_roundtrip() {
    let (state, dir) = temp_state("m2-compose");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    // 空表单
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/compose", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("撰写新邮件"), "无参数应渲染空白撰写页");
    assert!(html.contains("存草稿"), "应有存草稿按钮");
    // 存草稿
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("subject", "草稿主题甲"),
                ("body_text", "草稿正文"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v.starts_with("/compose?draft_id=1&saved=1")),
        "存草稿应跳转草稿编辑页：{:?}",
        resp.extra_headers
    );
    // 回显编辑
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "draft_id=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("编辑草稿"), "草稿 id 应进入编辑态");
    assert!(html.contains("草稿主题甲"), "主题应回显");
    assert!(html.contains("草稿正文"), "正文应回显");
    // 覆盖（幂等：同 id 更新不新增）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("draft_id", "1"),
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("subject", "改题"),
                ("body_text", "改文"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/compose?draft_id=1&saved=1"),
        "覆盖保存应保持同一草稿 id"
    );
    let total: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(total, 1, "覆盖不应新增行");
    cleanup(&dir);
}

#[test]
fn reply_prefills_and_in_reply_to_persisted() {
    let (state, dir) = temp_state("m2-reply");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in', 'inbox', 'received', 'boss@x.cn', '领导', 'demo@chinatelecom.cn',
                         '激励清单', '请按清单执行。', '激励 清单', '请 按 清单 执行', 1, 1, 1)",
            [],
        )
        .unwrap();
    });
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "reply_to=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("回复邮件"), "回复应进入回复态标题");
    assert!(html.contains("Re: 激励清单"), "主题应加 Re: 前缀");
    assert!(html.contains("boss@x.cn"), "收件人应预填原发件人");
    assert!(html.contains("----- 原文 -----"), "正文应引用原文");
    assert!(
        html.contains("name=\"in_reply_to\" value=\"1\""),
        "回复链应写入隐藏域"
    );
    cleanup(&dir);
}

#[test]
fn detail_marks_read_and_delete_redirects() {
    let (state, dir) = temp_state("m2-detail");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in', 'inbox', 'received', 'a@x.cn', '', 'demo@chinatelecom.cn',
                         '来件主题', '来件正文。', '来件 主题', '来件 正文', 1, 1, 1)",
            [],
        )
        .unwrap();
    });
    // 打开详情：未读清零 + 页面渲染
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(html.contains("来件主题"), "详情页应渲染主题");
    assert!(html.contains("a@x.cn"), "详情页应渲染发件人");
    let unread: i64 = state.with_conn(|c| {
        c.query_row("SELECT unread FROM emails WHERE id=1", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(unread, 0, "打开详情应本地置已读");
    // 删除 → HX-Redirect 列表
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/delete", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/?folder=inbox"),
        "删除应整页回列表：{:?}",
        resp.extra_headers
    );
    // 二次访问 404
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 404, "删除后详情应 404");
    cleanup(&dir);
}

#[test]
fn attachment_save_via_post_writes_file() {
    let (state, dir) = temp_state("m2-attach");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let id = mail::save_draft(
        &state,
        &mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            subject: "带附件草稿".into(),
            body_text: "见附件".into(),
            in_reply_to: None,
        },
    )
    .unwrap();
    let src = dir.join("源数据.txt");
    std::fs::write(&src, "attachment-content-123").unwrap();
    mail::add_attachments(&state, id, &[src.to_str().unwrap().into()]).unwrap();
    // 详情页渲染附件行
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}"), b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("源数据.txt"), "详情页应渲染附件名");
    assert!(html.contains("另存为"), "详情页应提供另存按钮");
    // POST 另存
    let dest = dir.join("另存结果.txt");
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            &format!("/emails/{id}/attachments/1"),
            &form(&[("dest", dest.to_str().unwrap())]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let frag = body(&resp);
    assert!(frag.contains("toast ok"), "另存成功应回 toast：{frag}");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"attachment-content-123",
        "另存内容应解压回原文"
    );
    cleanup(&dir);
}

#[test]
fn compose_draft_with_attachments_and_keep_ids() {
    let (state, dir) = temp_state("m2-draft-att");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let f1 = dir.join("甲.txt");
    std::fs::write(&f1, b"one").unwrap();
    // 存草稿 + 新附件（attach_paths JSON）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("subject", "带附件"),
                ("body_text", "正文"),
                ("keep_ids", "[]"),
                (
                    "attach_paths",
                    &serde_json::to_string(&[f1.to_str().unwrap()]).unwrap(),
                ),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/compose?draft_id=1&saved=1"),
        "附件随草稿保存成功"
    );
    let detail = mail::get(&state, 1).unwrap();
    assert_eq!(detail.attach_cnt, 1, "附件应入库");
    assert_eq!(detail.attachments[0].filename, "甲.txt");
    // 再次保存：keep_ids 空 → 裁剪既有附件
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("draft_id", "1"),
                ("to_addrs", "peer@x.cn"),
                ("subject", "带附件"),
                ("body_text", "正文"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let detail = mail::get(&state, 1).unwrap();
    assert_eq!(detail.attach_cnt, 0, "keep_ids 空应裁剪全部既有附件");
    cleanup(&dir);
}

#[test]
fn compose_send_enqueues_and_redirects() {
    let (state, dir) = temp_state("m3-send");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    // 直接发送（无草稿 id：先建行再排队）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("subject", "发信主题"),
                ("body_text", "发信正文"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/emails/1"),
        "发送应跳转详情页轮询：{:?}",
        resp.extra_headers
    );
    let (status, folder): (String, String) = state.with_conn(|c| {
        c.query_row("SELECT status, folder FROM emails WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    });
    assert_eq!(
        status, "queued",
        "发送应将草稿置排队（worker 侧未启动，状态保持）"
    );
    assert_eq!(folder, "draft", "排队态仍在草稿箱，sent 终态才迁移");
    // 详情页（排队中）应带 2s 轮询属性与状态条
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("hx-trigger=\"every 2s\""),
        "排队态详情页应轮询"
    );
    assert!(html.contains("排队中"), "排队态应显示状态条");
    cleanup(&dir);
}

#[test]
fn compose_send_is_idempotent_on_repeat() {
    let (state, dir) = temp_state("m3-send-2");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    // 第一轮：不带 draft_id（新建后入队）；第二轮：带 draft_id=1（覆盖后重复入队被拒）
    for i in 0..2 {
        let mut pairs = vec![
            ("to_addrs", "peer@x.cn"),
            ("cc_addrs", ""),
            ("subject", "重发主题"),
            ("body_text", "重发正文"),
            ("keep_ids", "[]"),
            ("attach_paths", "[]"),
        ];
        if i > 0 {
            pairs.insert(0, ("draft_id", "1"));
        }
        let resp = route(
            &state,
            &assets(),
            &req("POST", "/compose/send", &form(&pairs)),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(
            resp.extra_headers
                .iter()
                .any(|(k, v)| k == "HX-Redirect" && v == "/emails/1"),
            "重复提交仍应跳转详情页"
        );
    }
    let total: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(total, 1, "重复发送不应新增行（先存草稿覆盖同一封）");
    let (status, error): (String, Option<String>) = state.with_conn(|c| {
        c.query_row("SELECT status, error FROM emails WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    });
    assert_eq!(
        status, "queued",
        "第二次提交不应把 queued 拉回 draft 后再入队失败"
    );
    assert!(error.is_none());
    cleanup(&dir);
}

#[test]
fn resend_failed_returns_to_compose_and_rejects_others() {
    let (state, dir) = temp_state("m3-resend");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    // 造一封 failed 邮件
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, created_at, error)
                 VALUES ('out', 'sent', 'failed', 'demo@chinatelecom.cn', 'peer@x.cn', '失败件',
                         '失败 件', '体', 1, '服务器拒收')",
            [],
        )
        .unwrap();
    });
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/resend", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/compose?draft_id=1"),
        "failed 重发应回撰写页：{:?}",
        resp.extra_headers
    );
    let (status, folder): (String, String) = state.with_conn(|c| {
        c.query_row("SELECT status, folder FROM emails WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    });
    assert_eq!((status.as_str(), folder.as_str()), ("draft", "draft"));
    // 非 failed（已是 draft）再重发 → 带错误回详情页
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/resend", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v.starts_with("/emails/1?error=")),
        "非 failed 重发应被拒并提示"
    );
    cleanup(&dir);
}

#[test]
fn fetch_flow_without_credential_shows_error_and_refresh() {
    let (state, dir) = temp_state("m4-fetch");
    // 配置账号但凭据缺失：start_fetch 同步置失败态（无网络、无线程泄漏到断言之后）
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::DISPLAY_NAME, "张伟"),
            (keys::RECV_HOST, "imap.test.cn"),
            (keys::RECV_PORT, "993"),
        ])
        .unwrap();
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/fetch", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let frag = body(&resp);
    assert!(frag.contains("凭据缺失"), "凭据缺失应在片段中提示：{frag}");
    assert!(!frag.contains("every 2s"), "失败态不应轮询");
    // 状态轮询（consume）：首次读到 just_finished → HX-Refresh 整页刷新
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/fetch/status", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Refresh" && v == "true"),
        "刚完成应触发整页刷新：{:?}",
        resp.extra_headers
    );
    // 再次轮询：just_finished 已消费，不再刷新
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/fetch/status", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        !resp.extra_headers.iter().any(|(k, _)| k == "HX-Refresh"),
        "完成标记消费后不应重复刷新"
    );
    // 设置页同步 tab 展示真实收取时间线
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/sync", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("尚未收取过邮件"), "未收取过应展示占位");
    state
        .set_batch(&[(keys::LAST_SYNC_AT, "1774500000")])
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/sync", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("上次收取："), "有记录应展示上次收取时间");
    cleanup(&dir);
}

#[test]
fn read_endpoint_marks_local_unread() {
    let (state, dir) = temp_state("m4-read");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::RECV_HOST, "imap.invalid.local"),
        ])
        .unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, unread, created_at,
                                    remote_uid, received_at)
                 VALUES ('in', 'inbox', 'received', 'a@x.cn', 'demo@chinatelecom.cn', '来件',
                         '来件', '体', 1, 1, '42:5', 1)",
            [],
        )
        .unwrap();
    });
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/read", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 200);
    let unread: i64 = state.with_conn(|c| {
        c.query_row("SELECT unread FROM emails WHERE id=1", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(unread, 0, "已读接口应本地置 0");
    // 未配置时拒绝（配置强制入口原则）
    cleanup(&dir);
}

#[test]
fn compose_page_shows_send_button_and_failed_detail_shows_resend() {
    let (state, dir) = temp_state("m3-ui");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/compose", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("hx-post=\"/compose/send\""),
        "撰写页应有发送动作"
    );
    assert!(html.contains("存草稿"), "存草稿按钮应保留");
    // failed 详情页：状态条 + 重发按钮；sent 详情页：已发送徽标且无轮询
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, created_at, error, sent_at)
                 VALUES ('out', 'sent', 'failed', 'demo@chinatelecom.cn', 'peer@x.cn', '失败件',
                         '失败 件', '体', 1, '连接超时', 1)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, created_at, sent_at)
                 VALUES ('out', 'sent', 'sent', 'demo@chinatelecom.cn', 'peer@x.cn', '成功件',
                         '成功 件', '体', 1, 1)",
            [],
        )
        .unwrap();
    });
    let failed_html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        failed_html.contains("发送失败：连接超时"),
        "失败详情应展示原因"
    );
    assert!(
        failed_html.contains("hx-post=\"/emails/1/resend\""),
        "失败详情应有重发按钮"
    );
    let sent_html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/2", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(sent_html.contains("已发送"), "sent 详情应显示终态徽标");
    assert!(!sent_html.contains("every 2s"), "终态不应再轮询");
    cleanup(&dir);
}

#[test]
fn wizard_setup_form_prefills_defaults() {
    let (state, dir) = temp_state("setup");
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/welcome/setup", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(html.contains("smtp.chinatelecom.cn"), "SMTP 主机应预填");
    assert!(html.contains("value=\"465\""), "SMTP 端口应预填 465");
    assert!(html.contains("imap.chinatelecom.cn"), "IMAP 主机应预填");
    assert!(html.contains("@chinatelecom.cn"), "后缀应固定展示");
    cleanup(&dir);
}

#[test]
fn wizard_save_requires_account_and_password() {
    let (state, dir) = temp_state("wiz-empty");
    for pairs in [
        vec![("account_prefix", ""), ("password", "x")],
        vec![("account_prefix", "demo"), ("password", "")],
    ] {
        let resp = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(&pairs)),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(body(&resp).contains("请填写邮箱账号与密码"), "空字段应拦截");
    }
    assert!(!state.configured(), "不应有副作用");
    cleanup(&dir);
}

#[test]
fn wizard_save_success_redirects_and_persists() {
    let (state, dir) = temp_state("wiz-ok");
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
        "成功保存应整页跳转完成页"
    );
    assert!(state.configured(), "配置应已落库");
    let d = defaults();
    assert_eq!(
        state.get_or(keys::SMTP_HOST, &d).as_deref(),
        Some("smtp.test.cn")
    );
    // 凭据走 keychain/keyfile，不落 settings 明文
    assert!(matches!(
        state.retrieve_password("unit.router@chinatelecom.cn"),
        GetResult::Keychain(_) | GetResult::Keyfile(_)
    ));
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn wizard_save_aborts_when_any_channel_fails() {
    let (state, dir) = temp_state("wiz-fail");
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
        &FakeTester {
            smtp_ok: false,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(html.contains("未保存"), "失败应提示未保存");
    assert!(html.contains("发信通道"), "应展示发信通道结果");
    assert!(html.contains("模拟失败"), "应展示失败原因");
    assert!(!state.configured(), "测试未全过不得落库");
    cleanup(&dir);
}

#[test]
fn wizard_done_shows_summary_and_guards_unconfigured() {
    let (state, dir) = temp_state("done");
    // 未配置直接访问 → 回欢迎页
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/welcome/done", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(body(&resp).contains("欢迎使用 CorpMail"));
    // 配置后展示摘要
    let _ = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/welcome/done", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(
        html.contains("unit.router@chinatelecom.cn"),
        "完成页应展示完整账号"
    );
    assert!(html.contains("smtp.test.cn:465"), "完成页应展示发信服务器");
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn settings_page_renders_account_pane() {
    let (state, dir) = temp_state("set-page");
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/settings", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(
        html.contains("id=\"settings-pane\""),
        "设置页应含 pane 容器"
    );
    assert!(html.contains("发信（SMTP）"), "默认打开账号 tab");
    assert!(html.contains("收件同步"), "tab 条应完整");
    // 直接 URL 指定 tab
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/settings/storage", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(body(&resp).contains("数据与附件"), "URL 直达存储 tab");
    cleanup(&dir);
}

#[test]
fn settings_pane_fragment_switch_without_layout() {
    let (state, dir) = temp_state("set-frag");
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/settings/panes/sync", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(!html.contains("<html"), "片段不含完整布局");
    assert!(html.contains("启动时自动收取"), "应渲染同步 pane 内容");
    // 非法 tab → 404
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/settings/panes/nope", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 404, "非法 tab 应 404");
    cleanup(&dir);
}

#[test]
fn sync_interval_clamped_to_7_days() {
    let (state, dir) = temp_state("sync-clamp");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[
                ("sync_on_start", "true"),
                ("sync_interval_min", "100000"),
                ("first_fetch_days", "30"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let d = defaults();
    assert_eq!(
        state.get_or(keys::SYNC_INTERVAL_MIN, &d).as_deref(),
        Some("10080"),
        "巨型间隔应被钳到 7 天，防调度线程沉睡"
    );
    cleanup(&dir);
}

#[test]
fn huge_page_number_does_not_panic() {
    let (state, dir) = temp_state("huge-page");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let resp = route(
        &state,
        &assets(),
        &req_q("GET", "/", "page=9223372036854775807", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 200);
    cleanup(&dir);
}

#[test]
fn settings_sync_save_roundtrip() {
    let (state, dir) = temp_state("sync-save");
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[
                ("sync_on_start", "true"),
                ("sync_interval_min", "15"),
                ("first_fetch_days", "90"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(body(&resp).contains("✓ 已保存"), "保存后片段应回显已保存");
    let d = defaults();
    assert_eq!(
        state.get_or(keys::SYNC_INTERVAL_MIN, &d).as_deref(),
        Some("15")
    );
    assert_eq!(
        state.get_or(keys::FIRST_FETCH_DAYS, &d).as_deref(),
        Some("90")
    );
    assert_eq!(
        state.get_or(keys::SYNC_ON_START, &d).as_deref(),
        Some("true")
    );
    // 非法输入保持现值
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[
                ("sync_on_start", "false"),
                ("sync_interval_min", "abc"),
                ("first_fetch_days", "x"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(
        state.get_or(keys::SYNC_INTERVAL_MIN, &d).as_deref(),
        Some("15"),
        "非法间隔应保持现值"
    );
    assert_eq!(
        state.get_or(keys::FIRST_FETCH_DAYS, &d).as_deref(),
        Some("90"),
        "非法范围应保持现值"
    );
    assert_eq!(
        state.get_or(keys::SYNC_ON_START, &d).as_deref(),
        Some("false"),
        "开关本身可关"
    );
    cleanup(&dir);
}

#[test]
fn settings_sync_refetch_resets_cursor_and_keeps_range() {
    let (state, dir) = temp_state("sync-refetch");
    // 预置：已拉过 90 天并落游标（模拟真实首拉后的状态）
    state
        .set_batch(&[
            (keys::FIRST_FETCH_DAYS, "90"),
            (keys::LAST_UID, "9503"),
            (keys::LAST_UIDVALIDITY, "1"),
        ])
        .unwrap();
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/refetch",
            &form(&[("first_fetch_days", "0")]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let d = defaults();
    assert_eq!(
        state.get_or(keys::FIRST_FETCH_DAYS, &d).as_deref(),
        Some("0"),
        "重拉应采纳所选范围(全部)"
    );
    assert_eq!(
        state.get_or(keys::LAST_UID, &d).as_deref(),
        Some("0"),
        "游标须清空以触发首拉"
    );
    assert_eq!(
        state.get_or(keys::LAST_UIDVALIDITY, &d).as_deref(),
        Some("0"),
        "uidvalidity 须清空"
    );
    assert!(
        body(&resp).contains("触发重新拉取"),
        "pane 应提示已触发重拉"
    );
    cleanup(&dir);
}

#[test]
fn sync_refetch_rejected_while_fetch_running() {
    // C5：收件进行中点「重新拉取」必须拒绝——运行轮每批把游标写回，
    // 叠加 start_fetch 防抖，若不拒绝则重拉意图静默丢失（既不清游标也不拉）
    let (state, dir) = temp_state("refetch-busy");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::FIRST_FETCH_DAYS, "90"),
            (keys::LAST_UID, "9503"),
            (keys::LAST_UIDVALIDITY, "1"),
        ])
        .unwrap();
    // 白盒注入：模拟收件进行中（started_at 必须同步置当前时刻，
    // 否则 fetch_snapshot 的看门狗会当作挂起轮立刻复位 running）
    {
        let mut s = state.fetch_state.lock().unwrap();
        s.running = true;
        s.started_at = mail::now_epoch();
    }

    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/refetch",
            &form(&[("first_fetch_days", "0")]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let d = defaults();
    let html = body(&resp);
    assert!(html.contains("收件进行中"), "进行中应拒绝并给出明确提示");
    assert!(!html.contains("触发重新拉取"), "不得同时出现成功触发提示");
    assert_eq!(
        state.get_or(keys::LAST_UID, &d).as_deref(),
        Some("9503"),
        "拒绝路径不得清游标"
    );
    assert_eq!(
        state.get_or(keys::FIRST_FETCH_DAYS, &d).as_deref(),
        Some("90"),
        "拒绝路径不得改动范围设置"
    );
    // 收件结束后（running=false）重试即可正常触发（路径由既有测试覆盖）
    cleanup(&dir);
}

#[test]
fn settings_storage_save_roundtrip_with_blank_semantics() {
    let (state, dir) = temp_state("sto-save");
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/storage/save",
            &form(&[
                ("inline_attach_max_mb", "50"),
                ("clean_sent_bulk_days", ""),
                ("keep_inbox_days", "400"),
                ("keep_sent_days", "x"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(body(&resp).contains("✓ 已保存"));
    assert!(body(&resp).contains("50"), "片段应回显新上限");
    let d = defaults();
    assert_eq!(
        state.get_or(keys::INLINE_ATTACH_MAX_MB, &d).as_deref(),
        Some("50")
    );
    assert_eq!(
        state.get_or(keys::CLEAN_SENT_BULK_DAYS, &d).as_deref(),
        Some(""),
        "留空=永久保留"
    );
    assert_eq!(
        state.get_or(keys::KEEP_INBOX_DAYS, &d).as_deref(),
        Some("400")
    );
    assert_eq!(
        state.get_or(keys::KEEP_SENT_DAYS, &d).as_deref(),
        Some("180"),
        "非法值保持默认"
    );
    cleanup(&dir);
}

#[test]
fn settings_account_save_keeps_credential_when_blank() {
    let (state, dir) = temp_state("acc-save");
    // 先经向导写入凭据
    let _ = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    // 同账号、密码留空保存：凭据不动
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/account/save",
            &form(&[
                ("account_prefix", "unit.router"),
                ("password", ""),
                ("display_name", "新名字"),
                ("smtp_host", "smtp.new.cn"),
                ("smtp_port", "465"),
                ("recv_host", "imap.test.cn"),
                ("recv_port", "993"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(body(&resp).contains("✓ 已保存"));
    assert!(
        matches!(
            state.retrieve_password("unit.router@chinatelecom.cn"),
            GetResult::Keychain(p) | GetResult::Keyfile(p)
                if p == "pw-test"
        ),
        "密码留空不应改动凭据"
    );
    let d = defaults();
    assert_eq!(
        state.get_or(keys::DISPLAY_NAME, &d).as_deref(),
        Some("新名字")
    );
    assert_eq!(
        state.get_or(keys::SMTP_HOST, &d).as_deref(),
        Some("smtp.new.cn")
    );
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn wizard_save_with_custom_domain() {
    let (state, dir) = temp_state("wiz-domain");
    let mut pairs: Vec<(&str, &str)> = WIZARD_FORM.to_vec();
    pairs.push(("account_domain", "@custom.cn"));
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(&pairs)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
        "自定义域名保存应跳转完成页"
    );
    let d = defaults();
    assert_eq!(
        state.get_or(keys::ACCOUNT_DOMAIN, &d).as_deref(),
        Some("@custom.cn"),
        "域名应落库"
    );
    assert!(
        matches!(
            state.retrieve_password("unit.router@custom.cn"),
            GetResult::Keychain(_) | GetResult::Keyfile(_)
        ),
        "凭据应存到自定义域名账号下"
    );
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/welcome/done", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        body(&resp).contains("unit.router@custom.cn"),
        "完成页应展示自定义域名账号"
    );
    state.clear_password("unit.router@custom.cn");
    cleanup(&dir);
}

#[test]
fn wizard_save_persists_first_fetch_range_and_migrates_data_dir() {
    // 需求：首次配置向导新增「首次拉取范围」与「数据保存目录」两项配置
    let (state, dir) = temp_state("wiz-range-dir");
    let target = std::env::temp_dir().join(format!("mbx-wiz-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);

    // 1) 合法范围 7 + 指定新目录 → 保存成功、范围落库、done 页提示待生效目录
    let mut pairs: Vec<(&str, &str)> = WIZARD_FORM.to_vec();
    pairs.push(("first_fetch_days", "7"));
    pairs.push(("data_dir", target.to_str().unwrap()));
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(&pairs)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
        "含新增配置项的向导保存应跳转完成页"
    );
    assert!(state.configured(), "应已落库");
    let d = defaults();
    assert_eq!(
        state.get_or(keys::FIRST_FETCH_DAYS, &d).as_deref(),
        Some("7"),
        "向导应保存所选首拉范围"
    );
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/welcome/done", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        body(&resp).contains("数据目录已迁移"),
        "done 页应提示数据目录已迁移待重启生效"
    );
    assert!(target.join("corpmail.db").exists(), "目标目录应产生快照库");

    // 1b) C2 回归——重启视角必须保留向导保存的首拉范围：
    //     first_fetch_days 存 DB（非 corpmail.toml 键），旧实现"先迁移后保存"
    //     时 VACUUM 快照不含该值，重启后回退默认 30 天
    {
        let state3 = AppState::open_with_anchor(&target, &dir);
        assert_eq!(
            state3
                .get_or(keys::FIRST_FETCH_DAYS, &defaults())
                .as_deref(),
            Some("7"),
            "重启视角（新目录库）应保留向导保存的首拉范围，不得回退默认 30"
        );
        assert!(state3.configured(), "重启视角账号配置完好（锚点 toml）");
    }

    // 1c) C6——迁移已完成待重启，重跑向导同目录：幂等跳过重复迁移，正常跳转
    let resp_retry = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(&pairs)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp_retry
            .extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
        "已迁移待重启状态重提向导应幂等跳过迁移并正常跳转"
    );

    // 2) 非法范围回落默认 30；未指定目录则沿用当前目录、done 页无迁移提示
    let (state2, dir2) = temp_state("wiz-range2");
    let mut pairs2: Vec<(&str, &str)> = WIZARD_FORM.to_vec();
    pairs2.push(("first_fetch_days", "x"));
    let resp2 = route(
        &state2,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(&pairs2)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp2
            .extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
        "非法范围应仍可保存（回落默认）"
    );
    assert_eq!(
        state2
            .get_or(keys::FIRST_FETCH_DAYS, &defaults())
            .as_deref(),
        Some("30"),
        "非法范围应回落默认 30"
    );
    let resp2 = route(
        &state2,
        &assets(),
        &req("GET", "/welcome/done", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        !body(&resp2).contains("数据目录已迁移"),
        "未指定目录时不应出现迁移提示"
    );
    state.clear_password("unit.router@chinatelecom.cn");
    state2.clear_password("unit.router@chinatelecom.cn");
    let _ = std::fs::remove_dir_all(&target);
    cleanup(&dir);
    cleanup(&dir2);
}

#[test]
fn settings_domain_change_migrates_credential() {
    let (state, dir) = temp_state("acc-domain-chg");
    // 先经向导写入凭据（默认 @chinatelecom.cn）
    let _ = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    // 设置页仅改域名、密码留空 → 凭据随迁
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/account/save",
            &form(&[
                ("account_prefix", "unit.router"),
                ("account_domain", "@custom.cn"),
                ("password", ""),
                ("display_name", "单测用户"),
                ("smtp_host", "smtp.test.cn"),
                ("smtp_port", "465"),
                ("recv_host", "imap.test.cn"),
                ("recv_port", "993"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(body(&resp).contains("✓ 已保存"));
    assert!(
        matches!(
            state.retrieve_password("unit.router@custom.cn"),
            GetResult::Keychain(p) | GetResult::Keyfile(p)
                if p == "pw-test"
        ),
        "旧凭据应迁移到新域名账号"
    );
    assert!(
        matches!(
            state.retrieve_password("unit.router@chinatelecom.cn"),
            GetResult::Missing
        ),
        "旧账号凭据应清除"
    );
    let d = defaults();
    assert_eq!(
        state.get_or(keys::ACCOUNT_DOMAIN, &d).as_deref(),
        Some("@custom.cn")
    );
    state.clear_password("unit.router@custom.cn");
    cleanup(&dir);
}

#[test]
fn settings_account_change_requires_password_when_no_credential() {
    let (state, dir) = temp_state("acc-chg");
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/account/save",
            &form(&[
                ("account_prefix", "someone.new"),
                ("password", ""),
                ("smtp_host", "smtp.test.cn"),
                ("smtp_port", "465"),
                ("recv_host", "imap.test.cn"),
                ("recv_port", "993"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        body(&resp).contains("请输入密码后保存"),
        "无凭据可迁时应要求输入密码"
    );
    let d = defaults();
    assert_eq!(state.get_or(keys::ACCOUNT_PREFIX, &d), None, "不应有副作用");
    cleanup(&dir);
}

#[test]
fn settings_account_test_uses_saved_credential() {
    let (state, dir) = temp_state("acc-test");
    let _ = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    // 密码留空 → 读已保存凭据测试
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/account/test-imap",
            &form(&[
                ("account_prefix", "unit.router"),
                ("password", ""),
                ("recv_host", "imap.test.cn"),
                ("recv_port", "993"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let html = body(&resp);
    assert!(html.contains("收信通道"), "应返回收信通道结果");
    assert!(html.contains("✓"), "假测试器应成功");
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn static_assets_served_with_mime() {
    let (state, dir) = temp_state("assets");
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/app.css", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type, "text/css; charset=utf-8");
    assert_eq!(body(&resp), "body{color:#333}");

    // 非 GET 不走静态
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/app.css", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 404);
    cleanup(&dir);
}

#[test]
fn error_page_escapes_dynamic_text() {
    let html = error_page("404", "<script>alert(1)</script>");
    assert!(!html.contains("<script"), "动态文案必须转义：{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
}

#[test]
fn traversal_and_unknown_paths_are_404() {
    let (state, dir) = temp_state("404");
    for path in ["/../Cargo.toml", "/nope", "/emails/abc", "/emails/0"] {
        let resp = route(
            &state,
            &assets(),
            &req("GET", path, b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert_eq!(resp.status, 404, "{path} 应 404");
    }
    cleanup(&dir);
}

/// 视觉冒烟素材导出：真实路由渲染全部 M1 页面到 .temp/smoke-html/，
/// 静态资源路径改写为相对引用，供浏览器加载与截图检查。
/// 用法：cargo test -- --ignored （不参与常规测试套件）
#[test]
#[ignore = "仅视觉冒烟素材导出：cargo test dump_smoke_html -- --ignored"]
fn dump_smoke_html() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.temp/smoke-html");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db_dir = root.join("db");
    std::fs::create_dir_all(&db_dir).unwrap();
    let state = AppState::open(&db_dir);
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let a = assets();

    let get = |path: &str| body(&route(&state, &a, &req("GET", path, b""), &t));
    // 带 query 的 GET（拆 path/query 两段后走 req_q）
    let get_url = |url: &str| {
        let (p, q) = url.split_once('?').unwrap_or((url, ""));
        body(&route(&state, &a, &req_q("GET", p, q, b""), &t))
    };
    let dump = |name: &str, html: String| {
        let html = html
            .replace("href=\"/app.css\"", "href=\"app.css\"")
            .replace("src=\"/htmx.min.js\"", "src=\"htmx.min.js\"")
            .replace("src=\"/app.js\"", "src=\"app.js\"")
            .replace("href=\"/favicon.svg\"", "href=\"favicon.svg\"");
        std::fs::write(root.join(name), html).unwrap();
    };

    // 未配置：欢迎页 + 向导表单
    dump("welcome.html", get("/"));
    dump("setup.html", get("/welcome/setup"));

    // 直接种子配置（不走向导路由，避免单测环境写 keychain）
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::DISPLAY_NAME, "张伟"),
            (keys::SMTP_HOST, "smtp.chinatelecom.cn"),
            (keys::SMTP_PORT, "465"),
            (keys::RECV_HOST, "imap.chinatelecom.cn"),
            (keys::RECV_PORT, "993"),
        ])
        .unwrap();

    // 已配置：完成页 + 主界面 + 设置四 tab
    dump("done.html", get("/welcome/done"));

    // M2 种子数据：草稿 ×2 + 收件 ×1（列表/详情/撰写冒烟用）
    let d1 = mail::save_draft(
        &state,
        &mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            subject: "关于宽带提速配案的意见反馈".into(),
            body_text: "各位领导：\n请查阅本月配案清单，重点客户需在月底前完成提速交付。".into(),
            in_reply_to: None,
        },
    )
    .unwrap();
    let d2 = mail::save_draft(
        &state,
        &mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "ops@x.cn".into(),
            cc_addrs: String::new(),
            subject: "Re: 月度激励汇总表".into(),
            body_text: "收到，我这边同步核对一遍数据口径。".into(),
            in_reply_to: Some(d1),
        },
    )
    .unwrap();
    // 商店截图种子：近 3 天多封收件（真实感：未读点/附件夹/发件人多样/近期时间戳）。
    // 时间基准 = 相对 now 的偏移（避免 1970-1-1 这类测试痕迹出现在截图里）
    let now = mail::now_epoch();
    /// 商店截图种子行（from, name, subject, subject_tokens, body, body_tokens, day_ago, unread, attach_cnt）
    type Seed = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        i64,
        bool,
        i64,
    );
    let seeds: [Seed; 6] = [
        // (from_addr, from_name, subject, subject_tokens, body, body_tokens, 天偏移, unread, 附件数提示列内嵌)
        (
            "boss@chinatelecom.cn",
            "王副总经理",
            "激励清单已批复，请按方案执行",
            "激励 清单 已 批复 请 按 方案 执行",
            "各位同事：本月激励清单已经批复，请各部门按既定方案推进，月底前完成发放。\n如有口径疑问随时找我核对。",
            "各位 同事 本 月 激励 清单 已经 批复 请 各 部门 按 既定 方案 推进 月底 前 完成 发放 如 有 口径 疑问 随时 找 我 核对",
            0,
            true,
            0,
        ),
        (
            "finance@chinatelecom.cn",
            "财务部-李静",
            "十月专线佣金结算表（含分局明细）",
            "十月 专线 佣金 结算 表 含 分局 明细",
            "附件是十月的专线佣金结算表，含各分局明细与合计，请查收核对。\n差异项请于本周五前反馈至我邮箱。",
            "附件 是 十 月 的 专线 佣金 结算 表 含 各 分局 明细 与 合计 请 查收 核对 差异 项 请 于 本 周五 前 反馈 至 我 邮箱",
            0,
            true,
            1,
        ),
        (
            "it-ops@chinatelecom.cn",
            "IT运维组",
            "邮件系统本周六凌晨例行维护通知",
            "邮件 系统 本 周六 凌晨 例行 维护 通知",
            "为提升服务稳定性，本周六 00:00-04:00 对邮件系统进行例行维护，期间收发可能短暂中断，请提前安排。",
            "为 提升 服务 稳定性 本 周六 0000-0400 对 邮件 系统 进行 例行 维护 期间 收发 可能 短暂 中断 请 提前 安排",
            1,
            false,
            0,
        ),
        (
            "hr@chinatelecom.cn",
            "人力资源部",
            "关于开展四季度劳动竞赛的通知",
            "关于 开展 四 季度 劳动 竞赛 的 通知",
            "各部门：四季度劳动竞赛方案已印发，请组织学习并落实申报。\n申报截止 11 月 15 日。",
            "各 部门 四 季度 劳动 竞赛 方案 已 印发 请 组织 学习 并 落实 申报 申报 截止 11 月 15 日",
            1,
            true,
            0,
        ),
        (
            "market@partner.cn",
            "合作方-智慧园区项目组",
            "智慧园区方案 v3 评审会议纪要",
            "智慧 园区 方案 v3 评审 会议 纪要",
            "纪要要点：\n1. 网络接入按双专线冗余设计；\n2. 安全设备清单请见附件核对；\n3. 下次评审 11 月 08 日 14:00。",
            "纪要 要点 网络 接入 按 双 专线 冗余 设计 安全 设备 清单 请 见 附件 核对 下次 评审 11 月 08 日 1400",
            2,
            false,
            2,
        ),
        (
            "noreply-notice@chinatelecom.cn",
            "系统通知",
            "您的 VPN 证书将于 7 天后到期",
            "您 的 vpn 证书 将 于 7 天 后 到期",
            "检测到您正在使用的 VPN 客户端证书将于 7 天后到期，请及时在门户系统自助更新，避免影响内网访问。",
            "检测 到 您 正在 使用 的 vpn 客户端 证书 将 于 7 天 后 到期 请 及时 在 门户 系统 自助 更新 避免 影响 内网 访问",
            3,
            false,
            0,
        ),
    ];
    let mut inbox_first_id = 0;
    for (i, (faddr, fname, subj, stok, body, btok, day_ago, unread, att)) in
        seeds.iter().enumerate()
    {
        let t = now - day_ago * 86_400 - (i as i64) * 3600;
        state.with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                        subject, body_text, subject_tokens, body_tokens, attach_cnt,
                                        unread, created_at, received_at)
                     VALUES ('in', 'inbox', 'received', ?1, ?2, 'demo@chinatelecom.cn',
                             ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                rusqlite::params![faddr, fname, subj, body, stok, btok, att, *unread as i64, t],
            )
            .unwrap();
        });
        let id = d2 + 1 + i as i64;
        if i == 0 {
            inbox_first_id = id;
        }
    }
    let inbox_id = inbox_first_id;

    dump("main.html", get("/"));
    dump("main-inbox-unread.html", get_url("/?unread=1"));
    dump("main-draft.html", get_url("/?folder=draft"));
    dump("main-all.html", get_url("/?folder=all"));
    dump("email-inbox.html", get(&format!("/emails/{inbox_id}")));
    dump("email-draft.html", get(&format!("/emails/{d1}")));
    dump("compose.html", get("/compose"));
    dump(
        "compose-reply.html",
        get_url(&format!("/compose?reply_to={inbox_id}")),
    );
    dump(
        "compose-draft.html",
        get_url(&format!("/compose?draft_id={d2}")),
    );

    // M3 发送状态素材：sending 轮询态 / failed 重发态 / sent 终态 + 已发送列表
    let sending_id = d2 + 2;
    let failed_id = d2 + 3;
    let sent_id = d2 + 4;
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, created_at)
                 VALUES ('out', 'draft', 'sending', 'demo@chinatelecom.cn', '张伟', 'ops@x.cn',
                         '发送中的月度报表', '正在投递，请稍候。', '发送 中 的 月度 报表',
                         '正在 投递 请 稍候', 2)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, created_at,
                                    error, sent_at)
                 VALUES ('out', 'sent', 'failed', 'demo@chinatelecom.cn', '张伟', 'ops@x.cn',
                         '投递失败的巡检报告', '重试一次仍未送达。', '投递 失败 的 巡检 报告',
                         '重试 一次 仍未 送达', 2, '服务器 535 拒绝：发信认证失败', 2)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, created_at,
                                    sent_at)
                 VALUES ('out', 'sent', 'sent', 'demo@chinatelecom.cn', '张伟', 'ops@x.cn',
                         '已送达的周报', '本周完成情况详见正文。', '已 送达 的 周报',
                         '本周 完成 情况 详见 正文', 2, 2)",
            [],
        )
        .unwrap();
    });
    dump("email-sending.html", get(&format!("/emails/{sending_id}")));
    dump("email-failed.html", get(&format!("/emails/{failed_id}")));
    dump("email-sent.html", get(&format!("/emails/{sent_id}")));
    dump("main-sent.html", get_url("/?folder=sent"));

    // M5 附件预览素材：给草稿 d1 加三类附件（png/txt/zip）→ 详情页统一预览按钮
    {
        let src_png = root.join("素材截图.png");
        std::fs::write(&src_png, b"fake-png").unwrap();
        let src_txt = root.join("说明.txt");
        std::fs::write(&src_txt, "附件文本内容样例".as_bytes()).unwrap();
        let src_zip = root.join("打包.zip");
        std::fs::write(&src_zip, b"zip-bytes").unwrap();
        mail::add_attachments(
            &state,
            d1,
            &[
                src_png.to_str().unwrap().into(),
                src_txt.to_str().unwrap().into(),
                src_zip.to_str().unwrap().into(),
            ],
        )
        .unwrap();
    }
    dump("email-attachments.html", get(&format!("/emails/{d1}")));

    dump("settings-account.html", get("/settings"));
    dump("settings-sync.html", get("/settings/sync"));
    dump("settings-storage.html", get("/settings/storage"));
    dump("settings-about.html", get("/settings/about"));

    // M5 检索素材："激励"命中收件与草稿（标题 <mark> 高亮）/ 无命中空态
    dump("main-search.html", get_url("/?q=%E6%BF%80%E5%8A%B1"));
    dump("main-search-none.html", get_url("/?q=zzqqxy"));

    // M5 清理素材：40 天前收件 + 30 天保留策略 → 存储页预估非零
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in', 'inbox', 'received', 'old@x.cn', '历史归档', 'demo@chinatelecom.cn',
                         '40天前的存量通知', '已超期的旧内容。', '40 天 前 的 存量 通知',
                         '已 超期 的 旧 内容', 0, ?1, ?1)",
            [mail::now_epoch() - 40 * 86_400],
        )
        .unwrap();
    });
    state.set_batch(&[(keys::KEEP_INBOX_DAYS, "30")]).unwrap();
    dump("settings-storage-pending.html", get("/settings/storage"));

    // M5 迁移素材：真迁移（快照 + 附件复制 + corpmail.toml 指针）→ pane 显示"待生效目录"行
    let pending_target = root.join("pending-target");
    state
        .move_data_dir(pending_target.to_str().unwrap())
        .unwrap();
    dump("settings-storage-moved.html", get("/settings/storage"));

    // M5 迁移后重启态：数据目录已在新位置、锚点（原目录）保留备份 → "删除备份"行
    {
        let state2 = AppState::open_with_anchor(&pending_target, &db_dir);
        let get2 = |path: &str| body(&route(&state2, &a, &req("GET", path, b""), &t));
        dump("settings-storage-backup.html", get2("/settings/storage"));
    }

    // 测试连接片段演示页（成功 + 失败双态 + 未保存提示）
    let frag = test_fragment(
        vec![
            test_view(
                "发信通道",
                &TestOutcome {
                    ok: true,
                    elapsed_ms: 342,
                    detail: String::new(),
                },
            ),
            test_view(
                "收信通道",
                &TestOutcome {
                    ok: false,
                    elapsed_ms: 1200,
                    detail: "连接超时".into(),
                },
            ),
        ],
        "未保存：两条通道需全部通过",
        false,
    );
    let wrapper = format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<link rel=\"stylesheet\" href=\"app.css\"></head>\
<body style=\"padding:48px;background:#f2f4f8\">\
<div class=\"setup-card\" style=\"max-width:560px;margin:0 auto\">\
<div class=\"test-row\">{}</div></div></body></html>",
        body(&frag)
    );
    dump("test-result-demo.html", wrapper);

    // 复制真实静态资源（与发行物同源）
    let ui_dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui-dist");
    for f in ["app.css", "htmx.min.js", "app.js", "favicon.svg"] {
        std::fs::copy(ui_dist.join(f), root.join(f)).unwrap();
    }
    let _ = std::fs::remove_dir_all(&db_dir);
}

#[test]
fn router_tolerates_malformed_requests() {
    // 协议层契约探针：畸形方法/路径/大小写/查询串不 panic，行为有界
    let (state, dir) = temp_state("fuzz-req");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let a = assets();
    // 1) 大小写方法名（非标准形式应 404/落空，不得 panic）
    for m in ["get", "GET ", "PATCH", "CONNECT"] {
        let resp = route(&state, &a, &req(m, "/", b""), &t);
        assert!(resp.status == 200 || resp.status == 404, "{m} 应有界响应");
    }
    // 2) 路径边界：编码点、空段、超长
    for p in [
        "/%2e%2e/etc",
        "//",
        "/emails/",
        "/emails/-1",
        "/emails/99999999999999999999",
        "/settings/panes/",
        "/settings/panes/unknown",
        &format!("/emails/{}", "9".repeat(40)),
    ] {
        let resp = route(&state, &a, &req("GET", p, b""), &t);
        assert!(resp.status == 200 || resp.status == 404, "{p} 应有界响应");
    }
    // 3) 未配置状态下的全部动态路由 → 欢迎页（强制入口），不 panic
    let (fresh, fdir) = temp_state("fuzz-req-fresh");
    for p in [
        "/",
        "/compose",
        "/emails/1",
        "/settings",
        "/welcome/done",
        "/emails/fetch",
    ] {
        let resp = route(&fresh, &a, &req("GET", p, b""), &t);
        assert_eq!(resp.status, 200, "{p} 未配置应渲染欢迎页");
    }
    // 4) 畸形表单体：非 UTF-8 字节 / 断半 percent 编码 / 巨参
    for body in [
        &b"subject=\xff\xfe"[..],
        b"a=%E4%B8",
        b"a=%zz&b=1",
        &vec![b'x'; 1_000_000][..],
    ] {
        let resp = route(&state, &a, &req("POST", "/settings/sync/save", body), &t);
        assert!(
            resp.status == 200 || resp.status == 404,
            "畸形表单应有界响应"
        );
    }
    std::fs::remove_dir_all(&fdir).unwrap();
    cleanup(&dir);
}
