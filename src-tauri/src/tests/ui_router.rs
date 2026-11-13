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
    fn smtp(
        &self,
        _host: &str,
        _port: u16,
        _mode: crate::net::TlsMode,
        _account: &str,
        _password: &str,
    ) -> TestOutcome {
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

    fn imap(
        &self,
        _host: &str,
        _port: u16,
        _mode: crate::net::TlsMode,
        _account: &str,
        _password: &str,
    ) -> TestOutcome {
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
        // 默认无来源头 = 应用内请求（协议层来源校验放行）
        origin: None,
        sec_fetch_site: None,
        body: body.to_vec(),
    }
}

/// 带来源头/htmx 标记的请求构造（R34 来源校验与副作用 GET 用例）
fn req_hdr(
    method: &str,
    path: &str,
    query: &str,
    origin: Option<&str>,
    site: Option<&str>,
    htmx: bool,
) -> Req {
    Req {
        origin: origin.map(str::to_string),
        sec_fetch_site: site.map(str::to_string),
        is_htmx_request: htmx,
        ..req_q(method, path, query, b"")
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

/// 向导表单（2026-11-11）：首设必须显式选择数据保存目录。
/// 默认给当前临时数据目录 = 用户点「选择…」选了默认位置；需要自定义目录的
/// 用例再 push 一个 data_dir 覆盖（form_urlencoded 重复键由后端 HashMap 取后者）。
fn wizard_pairs(dir: &str) -> Vec<(&str, &str)> {
    let mut v = WIZARD_FORM.to_vec();
    v.push(("data_dir", dir));
    v
}

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
        bcc_addrs: String::new(),
        subject: s.into(),
        body_text: "配案正文预览".into(),
        in_reply_to: None,
        scheduled_at: 0,
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

/// 「全部」folder 已移除（2026-11-09）：旧链接/畸形 folder 回落收件箱，
/// 不再跨箱全量；mode=thread 已随会话入口下沉废弃（携带也被忽略）
#[test]
fn removed_all_folder_falls_back_to_inbox() {
    let (state, dir) = temp_state("no-all-folder");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','领导','demo@chinatelecom.cn',
                         '收件箱邮件','正文','收件 箱邮件','正文',1,1,1),
                        ('out','sent','sent','demo@chinatelecom.cn','张伟','peer@x.cn',
                         '已发送邮件','正文','已 发送 邮件','正文',0,2,2)",
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 旧「全部」链接：回落收件箱，只显示收件箱邮件
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=all", b""),
        &t,
    ));
    assert!(html.contains("收件箱"), "未知 folder 应回落收件箱：{html}");
    assert!(html.contains("收件箱邮件"), "应显示收件箱邮件：{html}");
    assert!(!html.contains("已发送邮件"), "不得再跨箱全量：{html}");
    // 旧书签携带 mode=thread：一律忽略，按单封列表渲染（入口已下沉详情页）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=sent&mode=thread", b""),
        &t,
    ));
    assert!(
        !html.contains("/threads/"),
        "mode=thread 不应再产生会话列表：{html}"
    );
    cleanup(&dir);
}

/// 请求体上限（审计 R55）：正常表单远小于上限，超过即 413 且不进路由逻辑
#[test]
fn oversized_request_body_is_rejected() {
    let (state, dir) = temp_state("r55-body-cap");
    let body = vec![b'a'; MAX_REQUEST_BODY + 1];
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/compose/autosave", &body),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 413, "超限请求体应 413");
    // 边界内（刚好等于上限）不进 413 分支
    let body = vec![b'a'; MAX_REQUEST_BODY];
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/compose/autosave", &body),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_ne!(resp.status, 413, "恰好等于上限不应被拒");
    cleanup(&dir);
}

/// 列表行片段（审计 R41）：后台刷新（收取完成/置顶重排）走 GET /list/rows，
/// 只回邮件行 + 顶栏计数的 OOB 片段，**不回搜索框与高级面板**——否则用户
/// 正在输入的搜索词（未提交）与展开的筛选面板会被服务端状态覆盖。
#[test]
fn list_rows_fragment_keeps_search_state() {
    let (state, dir) = temp_state("r41-list-rows");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    body_text, subject_tokens, body_tokens, unread, created_at)
                 VALUES ('in','inbox','received','a@x.cn','demo@chinatelecom.cn','甲','正文','甲','正文',0,10),
                        ('in','inbox','received','b@x.cn','demo@chinatelecom.cn','乙','正文','乙','正文',0,20)",
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/list/rows", "folder=inbox", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    // 行片段本体：两封邮件 + data-total
    assert!(html.contains("data-mail-id="), "应回邮件行：{html}");
    assert!(html.contains("甲") && html.contains("乙"), "应含两封邮件");
    assert!(html.contains("data-total=\"2\""), "行片段应带总数：{html}");
    // 计数走 OOB（在 #mail-list 之外，随主片段一起提交）
    assert!(
        html.contains("id=\"mail-count\"") && html.contains("hx-swap-oob=\"outerHTML\""),
        "应通过 OOB 更新顶栏计数：{html}"
    );
    assert!(html.contains("共 2 封"), "OOB 片段应含总数文案");
    // 顶栏/搜索框/高级面板一律不出现（状态留在 DOM 里，不被覆盖）
    assert!(!html.contains("class=\"search\""), "不得回搜索框：{html}");
    assert!(!html.contains("id=\"adv-panel\""), "不得回高级面板：{html}");
    assert!(!html.contains("id=\"list-area\""), "不得回列表容器");
    assert!(!html.contains("class=\"topbar\""), "不得回顶栏");
    // 完整列表（htmx 片段）仍含搜索框与高级面板，两者行为分流
    let full = body(&route(
        &state,
        &assets(),
        &req_hdr("GET", "/", "folder=inbox", None, None, true),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        full.contains("class=\"search\"") && full.contains("id=\"adv-panel\""),
        "整段列表片段仍应含搜索框与高级面板"
    );
    // 未配置：不渲染空列表，直接拒绝（避免把「没账号」画成「没有邮件」）
    let (fresh, fresh_dir) = temp_state("r41-list-rows-empty");
    let resp = route(
        &fresh,
        &assets(),
        &req("GET", "/list/rows", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 400, "未配置时 /list/rows 应 400");
    cleanup(&fresh_dir);
    cleanup(&dir);
}

/// 会话入口下沉详情页（2026-11-11）：同主题往来 >1 封 → 详情页标题下显示
/// 「会话 · N 封往来」（/threads/{id}）；单封（无往来）不渲染入口；
/// 会话详情页返回链接指向来源详情页并携带列表上下文
#[test]
fn thread_entry_on_detail_page_only_when_multiple() {
    let (state, dir) = temp_state("thread-entry");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at, thread_key)
                 VALUES ('in','inbox','received','boss@x.cn','领导','demo@chinatelecom.cn',
                         '会议通知','正文','会议 通知','正文',1,1,1,'会议通知'),
                        ('out','sent','sent','demo@chinatelecom.cn','张伟','boss@x.cn',
                         'Re: 会议通知','回复','会议 通知','回复',0,2,2,'会议通知'),
                        ('in','inbox','received','peer@x.cn','同事','demo@chinatelecom.cn',
                         '独立邮件','正文','独立 邮件','正文',0,3,3,'独立邮件')",
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };

    // 有往来：收件方详情带入口，链接指向会话详情（/threads/{id}）
    let html = body(&route(&state, &assets(), &req("GET", "/emails/1", b""), &t));
    assert!(
        html.contains("会话 · 2 封往来"),
        "有往来应显示会话入口：{html}"
    );
    // 入口 href 写死在模板里（非变量插值），不经过自动转义
    assert!(
        html.contains(r#"href="/threads/1?folder=inbox""#),
        "入口应指向 /threads/1：{html}"
    );
    // 已发送的回复同样带入口（往来双向可见）
    let html = body(&route(&state, &assets(), &req("GET", "/emails/2", b""), &t));
    assert!(
        html.contains("会话 · 2 封往来"),
        "回复方详情同样显示入口：{html}"
    );

    // 无往来（组内仅 1 封）：不渲染入口
    let html = body(&route(&state, &assets(), &req("GET", "/emails/3", b""), &t));
    assert!(
        !html.contains("thread-entry"),
        "单封不应显示会话入口：{html}"
    );

    // 会话详情返回链接 = 来源详情页，携带列表上下文
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/threads/1", "unread=1", b""),
        &t,
    ));
    assert!(
        html.contains(r#"href="&#x2f;emails&#x2f;1?folder=inbox&amp;unread=1""#),
        "会话页应返回来源详情并携带列表上下文：{html}"
    );
    cleanup(&dir);
}

/// 发件箱/草稿箱（2026-11-09 需求）：①列表行「对方」显示收件人而非自己；
/// ②批量按钮组只保留「全选」（删除/批量操作/清空选择不渲染）；
/// ③顶栏 ⋯ 下拉（含「收取新邮件」）不渲染；收件箱视图不受影响
#[test]
fn sent_draft_rows_show_recipient_and_toolbar_minimized() {
    let (state, dir) = temp_state("toolbar-sent-draft");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, created_at, sent_at)
                 VALUES ('out','sent','sent','demo@chinatelecom.cn','张伟','peer@x.cn',
                         '季度汇报','已发送正文','季度 汇报','正文',1,1),
                        ('out','draft','draft','demo@chinatelecom.cn','张伟',
                         '李四 <lisi@x.cn>, wangwu@x.cn, zhao@x.cn, qian@x.cn',
                         '未完草稿','草稿正文','未完 草稿','正文',2,NULL),
                        ('in','inbox','received','boss@x.cn','部门领导','demo@chinatelecom.cn',
                         '月度激励清单','收件正文','月度 激励 清单','正文',3,NULL)",
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 发件箱：对方列显示收件人地址，不显示自己姓名；按钮精简
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=sent", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains(r#"class="mr-who">peer@x.cn</span>"#),
        "发件箱行「对方」位应渲染收件人地址：{html}"
    );
    assert!(
        !html.contains(r#"class="mr-who">张伟</span>"#),
        "发件箱行「对方」位不应显示发件人（自己）：{html}"
    );
    assert!(html.contains("全选"), "批量组应保留全选");
    assert!(
        html.contains(r#"id="ba-select-all""#),
        "发件箱应保留「跨页选择全部」入口：{html}"
    );
    assert!(
        !html.contains("data-batch=\"delete\"") && !html.contains("清空选择"),
        "发件箱不应渲染删除/清空选择按钮：{html}"
    );
    assert!(
        !html.contains("收取新邮件") && !html.contains("tb-more-btn"),
        "发件箱不应渲染 ⋯ 下拉（含收取新邮件）：{html}"
    );
    // 草稿箱：收件人姓名优先、超 3 个截断补「等 N 位」
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=draft", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("李四、wangwu@x.cn、zhao@x.cn 等 4 位"),
        "草稿行应显示收件人（姓名优先、多收件人截断）：{html}"
    );
    assert!(
        !html.contains("收取新邮件"),
        "草稿箱不应渲染收取新邮件：{html}"
    );
    // 收件箱：按钮组与 ⋯ 下拉保持原样
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("data-batch=\"delete\"") && html.contains("清空选择"),
        "收件箱应保留完整批量按钮组：{html}"
    );
    assert!(
        html.contains("收取新邮件") && html.contains("tb-more-btn"),
        "收件箱应保留 ⋯ 下拉与收取入口：{html}"
    );
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
        bcc_addrs: String::new(),
        subject: s.into(),
        body_text: "配案正文预览".into(),
        in_reply_to: None,
        scheduled_at: 0,
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
        &req_q("GET", "/", "folder=draft&q=%E6%BF%80%E5%8A%B1", b""),
        &tester,
    ));
    assert!(html.contains("<mark>激励</mark>"), "命中词应高亮：{html}");
    assert!(!html.contains("无关主题"), "未命中邮件不应出现在列表");
    assert!(
        html.contains("q=%E6%BF%80%E5%8A%B1"),
        "分页基址应携带检索词"
    );
    // 全停用词：无有效查询项 → 空结果（旧实现回落整表，看似搜索被忽略，R29）
    let html2 = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=draft&q=%E7%9A%84%E4%BA%86", b""),
        &tester,
    ));
    assert!(
        !html2.contains("无关主题邮件") && html2.contains("没有匹配的邮件"),
        "无效词应落到检索空态：{html2}"
    );
    // 无命中：空态为检索文案
    let html3 = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=draft&q=zzqqxy", b""),
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
    // URL 作为变量输出时 minijinja 会转义 & → &amp;（浏览器解析无差异）
    assert!(
        plain.contains("unread=1&amp;page=1"),
        "未开态开关链接应带 unread=1：{plain}"
    );
    assert!(plain.contains("只看未读"));
    assert!(plain.contains("只看外部"), "应同时提供外部筛选按钮");
    // 开启态：开关为恢复链接（不带 unread 参数）+ 明确文字
    let unread = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "unread=1", b""),
        &tester,
    ));
    // 开启态：开关保持自身语义（勾选图标 + 只看未读），不再出现两个「显示全部」
    assert!(
        unread.contains("只看未读") && unread.contains("class=\"ic\""),
        "开启态应保留自身筛选文案（SVG 勾）：{unread}"
    );
    assert!(
        !unread.contains("显示全部"),
        "两个筛选同时展示时不得出现重复「显示全部」：{unread}"
    );
    // 恢复链接不带 unread 参数（minijinja 按规范将 / 转义为 &#x2f;，浏览器解码后
    // 即 /?page=1；断言按渲染原文精确匹配）
    assert!(
        unread.contains(r#"class="tb-item on" href="&#x2f;?page=1""#),
        "恢复链接不得再携带 unread 参数：{unread}"
    );
    // 外部筛选按钮须保留未读筛选（两个开关互不吞掉对方）
    assert!(
        unread.contains("external=1&amp;unread=1&amp;page=1"),
        "外部按钮应保留未读筛选：{unread}"
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
            bcc_addrs: String::new(),
            subject: "预览用例".into(),
            body_text: "见附件".into(),
            in_reply_to: None,
            scheduled_at: 0,
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
fn storage_move_background_branch_carries_header_and_placeholder() {
    // R5（复查审计）：A1 的三段契约（settings 打头 → ui 写进 Response →
    // app.js 读头分支）此前零自动化验证——E2E 走同步分支永远拿不到该头。
    // 这里注入 no-op executor 走真实路由，钉住后端两段：标记头 + 等待占位片段
    let (state, dir) = temp_state("m5-move-bg-header");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.set_migration_executor(std::sync::Arc::new(|_dest: &str| Ok(())));
    let dest = std::env::temp_dir().join(format!("mbx-bg-header-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/storage/move",
            &form(&[("dest", dest.to_str().unwrap())]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 200, "后台分支是 200 + 自定义头，不是错误状态");
    assert!(
        resp.extra_headers.iter().any(|(k, v)| {
            k == crate::ui::MIGRATION_BACKGROUND_HEADER && v == "background"
        }),
        "缺头会让前端把占位 pane 渲染进页面并关模态，永久停在「准备迁移…」（A1 复现）：{:?}",
        resp.extra_headers
    );
    let text = body(&resp);
    assert!(text.contains("数据迁移正在进行中"), "应为等待占位：{text}");
    assert!(
        text.contains("id=\"settings-pane\""),
        "前端按 pane 片段替换进设置面板：{text}"
    );
    state.mark_migration_finished();
    let _ = std::fs::remove_dir_all(&dest);
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
    // POST 迁移成功 → 回 pane 片段：主数据目录行显示新路径 + 重启生效徽标
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
        html.contains("重启后生效"),
        "成功后 pane 应在数据目录旁显示重启生效徽标：{html}"
    );
    // minijinja 按规范将 / 转义为 &#x2f;（浏览器解码后即原路径），断言按渲染原文比较
    let dest_escaped = dest.to_str().unwrap().replace('/', "&#x2f;");
    assert!(
        html.contains(&dest_escaped),
        "配置的数据目录（待生效）路径应展示：{html}"
    );
    assert!(
        html.contains("当前使用"),
        "待生效时应注记仍在使用的旧目录：{html}"
    );
    assert!(html.contains("重启应用后切换"), "应提示重启切换：{html}");
    // 指针已写锚点库，启动解析定位新目录
    assert_eq!(
        AppState::resolve_start_dir(&dir),
        dest,
        "重启后应定位新目录"
    );
    // 迁移后进入冻结期：再次迁移被路由层挡下（409 纯文本），而不是渲染错误 pane
    // ——旧实现在这里靠"目标已存在 corpmail.db"报错，现在语义是"等你重启"
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/storage/move",
            &form(&[("dest", dest.to_str().unwrap())]),
        ),
        &tester,
    );
    assert_eq!(resp.status, 409, "冻结期二次迁移必须被拒");
    assert!(
        body(&resp).contains("已迁移"),
        "应提示重启：{}",
        body(&resp)
    );
    // 存储 pane：固定横幅 + 写操作按钮禁用
    let html2 = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/panes/storage", b""),
        &tester,
    ));
    assert!(
        html2.contains("重启应用后继续使用"),
        "冻结期应显示固定横幅：{html2}"
    );
    assert!(html2.contains("disabled"), "写操作按钮应禁用：{html2}");
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
    assert!(
        html.contains(r#"<textarea name="body_text" id="body-text" rows="14">"#),
        "纯文本原件回复应为纯文本模式"
    );
    assert!(
        html.contains(r#"<input type="hidden" name="body_mode" id="body-mode" value="plain">"#),
        "纯文本回复应下发 plain 模式"
    );
    assert!(
        html.contains("id=\"body-html-box\" hidden"),
        "纯文本回复的富文本区应隐藏（wangEditor 不挂载）"
    );
    cleanup(&dir);
}

/// HTML 原件的回复/转发应保持富文本模式（类型不降级）；纯文本原件保持纯文本
#[test]
fn reply_and_forward_keep_body_kind() {
    let (state, dir) = temp_state("m2-reply-html");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, body_html, subject_tokens, body_tokens,
                                    unread, created_at, received_at)
                 VALUES ('in', 'inbox', 'received', 'boss@x.cn', '领导', 'demo@chinatelecom.cn',
                         'HTML 来件', '表格正文', '<table><tr><td>表格正文</td></tr></table>',
                         'html 来件', '表格 正文', 1, 1, 1)",
            [],
        )
        .unwrap();
    });
    // 回复：HTML 原件 → 富文本模式（wangEditor 挂载点 + 初始 HTML 存 rt-initial）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "reply_to=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains(r#"<input type="hidden" name="body_mode" id="body-mode" value="html">"#),
        "HTML 原件回复应下发 html 模式"
    );
    assert!(
        html.contains("id=\"body-plain\" hidden"),
        "HTML 回复应隐藏纯文本区（单向不降级）"
    );
    assert!(
        !html.contains("id=\"body-html-box\" hidden"),
        "富文本区应可见（wangEditor 挂载）"
    );
    // 初始 HTML 经模板属性转义：< 变 &lt;进入 rt-initial，前端由浏览器解转义回填编辑器
    assert!(
        html.contains("&lt;table&gt;"),
        "回复应携带原 HTML 表格（表格保真，不包 blockquote）：{}",
        html.lines()
            .find(|l| l.contains("rt-initial"))
            .unwrap_or("(未找到 rt-initial)")
    );
    // 转发：HTML 原件 → 富文本模式 + 转发头（字段转义，value 中二次转义）
    let fwd = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "forward_from=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        fwd.contains("---------- 转发的邮件 ----------"),
        "转发头应存在"
    );
    assert!(
        fwd.contains("&lt;table&gt;"),
        "转发应携带原 HTML 表格（不包 blockquote）"
    );
    assert!(
        fwd.contains("发件人：领导 &amp;lt;boss@x.cn&amp;gt;"),
        "转发头发件人应转义（value 属性二次转义后为 &amp;lt; 形态）：{}",
        fwd.lines()
            .find(|l| l.contains("rt-initial"))
            .unwrap_or("(未找到 rt-initial)")
    );
    cleanup(&dir);
}

/// A5 源码页：html/text/headers 三视图 + 非法 part 回退 html
#[test]
fn source_page_renders_html_text_and_headers() {
    let (state, dir) = temp_state("a5-source");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, body_html, raw_headers, subject_tokens,
                                body_tokens, unread, created_at, received_at)
             VALUES ('in', 'inbox', 'received', 'a@x.cn', '', 'demo@chinatelecom.cn',
                     '乱码排查', '纯文本正文', '<p>HTML 正文</p>',
                     'Received: from mx.x.cn\r\nContent-Type: text/html; charset=gbk',
                     '乱码 排查', '纯 文本 正文', 1, 1, 1)",
            [],
        )
        .unwrap();
    });
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 默认 html：源码应转义展示（不能当 HTML 再渲染）
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1/source", b""),
        &tester,
    ));
    assert!(
        html.contains("&lt;p&gt;HTML 正文&lt;"),
        "HTML 源码应转义展示：{html}"
    );
    assert!(
        html.contains("HTML 源码") && html.contains("原始信头"),
        "应有三视图标签：{html}"
    );
    // text
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/emails/1/source", "part=text", b""),
        &tester,
    ));
    assert!(html.contains("纯文本正文"));
    // headers：原始信头原样展示
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/emails/1/source", "part=headers", b""),
        &tester,
    ));
    assert!(
        html.contains("Received: from mx.x.cn"),
        "原始信头应原样展示：{html}"
    );
    // 非法 part 回退 html
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/emails/1/source", "part=bogus", b""),
        &tester,
    ));
    assert!(html.contains("&lt;p&gt;HTML 正文&lt;"));
    cleanup(&dir);
}

/// A5 配套：详情页标未读 → unread=1 + 保持列表上下文跳回；重复点击幂等
#[test]
fn mark_unread_restores_flag_and_redirects() {
    let (state, dir) = temp_state("a5-unread");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, subject_tokens, body_tokens, unread,
                                created_at, received_at)
             VALUES ('in','inbox','received','a@x.cn','','demo@chinatelecom.cn',
                     '待办','正文','待办','正文',0,1,1)",
            [],
        )
        .unwrap();
    });
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let resp = route(
        &state,
        &assets(),
        &req_q("POST", "/emails/1/unread", "folder=inbox&page=2", b""),
        &tester,
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v.contains("folder=inbox") && v.contains("page=2")),
        "应保持列表上下文跳回：{:?}",
        resp.extra_headers
    );
    let unread: i64 = state.with_conn(|c| {
        c.query_row("SELECT unread FROM emails WHERE id=1", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(unread, 1, "标未读应置 unread=1");
    // 幂等：已是未读再点不报错、值不变
    route(
        &state,
        &assets(),
        &req("POST", "/emails/1/unread", b""),
        &tester,
    );
    let unread: i64 = state.with_conn(|c| {
        c.query_row("SELECT unread FROM emails WHERE id=1", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(unread, 1);
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
    assert_eq!(
        unread, 1,
        "GET 详情必须只读（R34）：带副作用的 GET 会被 img/预取/跨站导航利用"
    );
    // 页面加载后的 load 型 POST 才置已读，并派发侧栏刷新事件
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/read", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Trigger" && v.contains("mbx:sideRefresh")),
        "置已读后应刷新侧栏未读数：{:?}",
        resp.extra_headers
    );
    let unread: i64 = state.with_conn(|c| {
        c.query_row("SELECT unread FROM emails WHERE id=1", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(unread, 0, "POST /read 应本地置已读");
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
        "删除应整页回列表（收件箱显式带 folder）：{:?}",
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
fn delete_redirects_back_with_list_context() {
    let (state, dir) = temp_state("m2-delete-back");
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
    let resp = route(
        &state,
        &assets(),
        &req_q(
            "POST",
            "/emails/1/delete",
            "unread=1&q=%E6%BF%80%E5%8A%B1&page=2",
            b"",
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let loc = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Redirect")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert_eq!(
        loc, "/?folder=inbox&unread=1&q=%E6%BF%80%E5%8A%B1&page=2",
        "删除应保持列表过滤与翻页：{loc}"
    );
    cleanup(&dir);
}

#[test]
fn image_proxy_rejects_private_and_non_http() {
    let (state, dir) = temp_state("proxy-guard");
    // 远程图片默认阻止（R4/R36）：先开启，才能验到 SSRF/协议白名单这道闸
    state
        .set_batch(&[(keys::ALLOW_REMOTE_IMAGES, "1")])
        .unwrap();
    // 内网地址：不发网络请求，直接 403
    let resp = route(
        &state,
        &assets(),
        &req_q(
            "GET",
            "/proxy/image",
            "url=http%3A%2F%2F127.0.0.1%2Fx.png",
            b"",
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 403, "内网地址应拒绝");
    // 非 http(s) scheme
    let resp = route(
        &state,
        &assets(),
        &req_q(
            "GET",
            "/proxy/image",
            "url=file%3A%2F%2F%2Fetc%2Fpasswd",
            b"",
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 400, "非 http(s) 应拒绝");
    // 非法 URL
    let resp = route(
        &state,
        &assets(),
        &req_q("GET", "/proxy/image", "url=not-a-url", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 400, "非法 URL 应拒绝");
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
            bcc_addrs: String::new(),
            subject: "带附件草稿".into(),
            body_text: "见附件".into(),
            in_reply_to: None,
            scheduled_at: 0,
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
            .any(|(k, v)| k == "HX-Redirect" && v == "/?just_sent=1"),
        "发送应回来源列表页（无上下文 → 默认收件箱）且带 just_sent（M1-6）：{:?}",
        resp.extra_headers
    );
    let (status, folder): (String, String) = state.with_conn(|c| {
        c.query_row("SELECT status, folder FROM emails WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    });
    assert_eq!(status, "draft", "撤销窗口内保持草稿态，未立即入队（A1）");
    assert_eq!(folder, "draft", "撤销窗口仍在草稿箱");
    let at: i64 = state.with_conn(|c| {
        c.query_row("SELECT scheduled_at FROM emails WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    let now = mail::now_epoch();
    assert!(at > now, "撤销窗口的 scheduled_at 应在未来：{at} vs {now}");
    assert!(at <= now + 10, "撤销窗口应为 8 秒左右：{at} vs {now}");
    // 详情页：窗口内显示「即将发送」+ 撤销按钮，不做 2s 轮询
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("即将发送"), "应显示即将发送状态：{html}");
    assert!(html.contains("/undo-send"), "应提供撤销入口：{html}");
    assert!(
        !html.contains("hx-trigger=\"every 2s\""),
        "撤销窗口内不应轮询"
    );
    cleanup(&dir);
}

/// 富文本发送（回归 2026-11-03）：前端曾因发送按钮（click + hx-include）的
/// configRequest 注入条件失效，body_html 参数恒空——HTML 转发以空正文发出，
/// 详情页只剩空 pre。此测试锁定后端契约：body_mode=html + body_html 表单
/// 必须保存为 HTML 正文且详情页正常渲染。
#[test]
fn compose_send_html_mode_keeps_body() {
    let (state, dir) = temp_state("m3-send-html");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let html_body =
        "<p>转发正文段落</p><table><tbody><tr><td>客户A</td><td>200M</td></tr></tbody></table>";
    // 表单形态与前端注入后一致：body_mode=html + body_html（body_text 由后端从 HTML 生成）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("subject", "HTML 转发"),
                ("body_mode", "html"),
                ("body_html", html_body),
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
            .any(|(k, v)| k == "HX-Redirect" && v == "/?just_sent=1"),
        "发送应回来源列表页（默认收件箱）且带 just_sent：{:?}",
        resp.extra_headers
    );
    let detail = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(detail.contains("md-html"), "HTML 正文应走富文本渲染分支");
    assert!(
        detail.contains("转发正文段落"),
        "详情页应包含正文文字（不再是空 pre）：{}",
        detail
            .lines()
            .find(|l| l.contains("md-body"))
            .unwrap_or("(未找到正文容器)")
    );
    assert!(detail.contains("<table"), "表格应保留");
    // 库内形态：body_html 净化入库 + body_text 由 HTML 生成（发件箱列表预览/检索用）
    let (stored_html, stored_text): (String, String) = state.with_conn(|c| {
        c.query_row(
            "SELECT COALESCE(body_html,''), body_text FROM emails WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert!(stored_html.contains("<table>"), "HTML 入库应含表格");
    assert!(
        stored_text.contains("客户A") && stored_text.contains("200M"),
        "纯文本版应从 HTML 剥出（预览/检索/转发引用）：{stored_text}"
    );
    cleanup(&dir);
}

/// 发送跳转保持列表上下文（回归 2026-11-03）：
/// 转发/回复链接带过滤进入撰写页 → hidden list_query 随表单提交 →
/// 发送后跳详情页必须原样携带，否则「返回列表」过滤/翻页在此断裂。
/// 同时验证隐藏域值是客户端可控的：畸形值（folder 注入/页码越界）被重滤丢弃。
/// M1-6 发送状态端点（GET /emails/{id}/status）：列表页反馈 toast 轮询用，
/// 轻量 JSON 承载 status/error；终态（sent 带 error 空、failed 带原因）、
/// 发送中（queued）、不存在的 id（404）
#[test]
fn email_status_endpoint_returns_lightweight_json() {
    let (state, dir) = temp_state("m16-status");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let input = mail::DraftInput {
        id: None,
        from_addr: "demo@chinatelecom.cn".into(),
        from_name: "张伟".into(),
        to_addrs: "peer@x.cn".into(),
        cc_addrs: String::new(),
        bcc_addrs: String::new(),
        subject: "状态查询".into(),
        body_text: "正文".into(),
        in_reply_to: None,
        scheduled_at: 0,
    };
    let id = mail::save_draft(&state, &input).unwrap();
    // queued：发送中
    let resp = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}/status"), b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    // serde_json 的对象键序不稳定（serde_json 空间用 BTreeMap/随机序取决于 feature），
    // 做包含性断言而非全文比对
    assert!(
        body(&resp).contains(r#""status":"draft""#) && body(&resp).contains("error"),
        "发送中状态载荷：{}",
        body(&resp)
    );
    // sent：终态空 error
    state
        .with_conn(|c| {
            c.execute(
                "UPDATE emails SET status='sent', error='' WHERE id=?1",
                [id],
            )
        })
        .unwrap();
    let resp = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}/status"), b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        body(&resp).contains(r#""status":"sent""#) && body(&resp).contains(r#""error":"""#),
        "sent 终态载荷：{}",
        body(&resp)
    );
    // failed：带原因（含中文与引号转义）
    state
        .with_conn(|c| {
            c.execute(
                "UPDATE emails SET status='failed', error='SMTP 应答：550 \"unknown user\"' WHERE id=?1",
                [id],
            )
        })
        .unwrap();
    let resp = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}/status"), b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let json = body(&resp);
    assert!(json.contains(r#""status":"failed""#), "状态载荷：{json}");
    assert!(
        json.contains("550 \\\"unknown user\\\"") || json.contains("550"),
        "失败原因含引用内容（serde_json 转义）：{json}"
    );
    // 不存在的 id：404
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/99999/status", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 404);
    cleanup(&dir);
}

/// M2-1 星标：端点翻转（事件头携带新态）、列表置顶排序、再翻取消置顶、
/// 不存在 id 404、详情页按钮状态回显
#[test]
fn star_endpoint_toggles_and_pins_to_top() {
    let (state, dir) = temp_state("m21-star");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    // 3 封收件（时间递增：id1 最旧）。unread=0 先出已读，避免与未读筛选行为耦合
    state
        .with_conn(|c| {
            for i in 1..=3i64 {
                c.execute(
                    "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    body_text, subject_tokens, body_tokens, unread, created_at)
                     VALUES ('in','inbox','received',?1,'demo@chinatelecom.cn',?2,'正文','s','b',0,?3)",
                    rusqlite::params![format!("peer{i}@x.cn"), format!("第 {i} 封"), i * 100],
                )
                .unwrap();
            }
            Ok::<(), String>(())
        })
        .unwrap();
    let list_html = |q: &str| {
        body(&route(
            &state,
            &assets(),
            &req("GET", "/", q.as_bytes()),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        ))
    };
    // 基线：无星标按时间倒序（3 → 2 → 1）
    let first = |h: &str| {
        h.lines()
            .find(|l| l.contains("data-mail-id="))
            .unwrap()
            .to_string()
    };
    let h = list_html("");
    assert!(
        first(&h).contains("data-mail-id=\"3\""),
        "无星标按时间倒序：{}",
        first(&h)
    );
    // 星标最旧的 id=1 → 事件头携带 starred:true（htmx ev.detail 回传）
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/star", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let trigger = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Trigger")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(
        trigger.contains("mbx:starDone") && trigger.contains("\"starred\":true"),
        "事件头应携带翻转后状态：{trigger}"
    );
    // 列表：id=1 置顶且星标按钮 on（按钮自身带 data-mail-id，直接定位；
    // 行开标签与按钮分属不同行，不能在同一行内同时断言）
    let h = list_html("");
    let top = first(&h);
    assert!(
        top.contains("data-mail-id=\"1\""),
        "星标邮件应置顶：{}",
        top
    );
    // 行内星标按钮已移除（2026-11-04 设计变更）：星标行以 starred class
    // （金色脊线）表达，按钮仅存于详情页
    assert!(
        h.contains("mail-row starred"),
        "星标行应以 starred class（金脊线）标记"
    );
    assert!(
        !h.contains("class=\"star-btn on\""),
        "列表行不应再有星标按钮"
    );
    // 详情页：按钮显示已星标态
    let detail = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        detail.contains("已置顶") && !detail.contains('⭐'),
        "详情页置顶按钮应回显已置顶文案（SVG 星形，不再用 emoji）"
    );
    // 再翻一次（取消）→ 置顶解除
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/star", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let trigger = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Trigger")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(
        trigger.contains("\"starred\":false"),
        "再翻应回 false：{trigger}"
    );
    let h = list_html("");
    assert!(
        first(&h).contains("data-mail-id=\"3\""),
        "取消星标后恢复时间倒序"
    );
    // 不存在的 id：404
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/999/star", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 404);
    cleanup(&dir);
}

#[test]
fn compose_send_redirect_keeps_list_context() {
    let (state, dir) = temp_state("m3-send-ctx");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("subject", "上下文保持"),
                ("body_text", "正文"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
                (
                    "list_query",
                    "unread=1&q=%E6%BF%80%E5%8A%B1&page=2&folder=../evil",
                ),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let loc = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Redirect")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert_eq!(
        loc, "/?folder=inbox&unread=1&q=%E6%BF%80%E5%8A%B1&page=2&just_sent=1",
        "发送后回来源列表页（保持过滤/翻页/文件夹）且带 just_sent：{loc}"
    );
    // 详情页返回链接还原过滤（folder=inbox 显式携带）
    let detail = body(&route(
        &state,
        &assets(),
        &req_q(
            "GET",
            "/emails/1",
            "folder=inbox&unread=1&q=%E6%BF%80%E5%8A%B1&page=2",
            b"",
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        // minijinja 对属性内的 / 与 & 做实体转义（&#x2f; == /，&amp; == &），两种形态都认
        detail.contains("folder=inbox&amp;unread=1&amp;q=%E6%BF%80%E5%8A%B1&amp;page=2")
            || detail.contains("folder=inbox&unread=1&q=%E6%BF%80%E5%8A%B1&page=2"),
        "详情页返回链接应还原过滤/翻页（含 folder=inbox）：{}",
        detail
            .lines()
            .find(|l| l.contains("class=\"back\""))
            .unwrap_or("(未找到)")
    );
    cleanup(&dir);
}

/// M1-3 密送 UI 全链路：草稿保存 bcc → 编辑页密送框回填 → 详情页渲染密送行
#[test]
fn compose_and_detail_render_bcc_round_trip() {
    let (state, dir) = temp_state("m1-bcc-ui");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "chinatelecom.cn"),
            (keys::DISPLAY_NAME, "张伟"),
        ])
        .unwrap();
    // 存草稿（带密送）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("bcc_addrs", "hidden@x.cn, b2@x.cn"),
                ("subject", "密送 UI"),
                ("body_text", "正文"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
                ("list_query", ""),
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
    let id: i64 = state
        .with_conn(|c| {
            c.query_row(
                "SELECT id FROM emails WHERE bcc_addrs LIKE 'hidden%'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    // 编辑页：密送框存在且回填
    let page = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", &format!("draft_id={id}"), b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        page.contains(r#"name="bcc_addrs""#),
        "撰写/编辑页应有密送输入框"
    );
    assert!(
        page.contains(r#"value="hidden@x.cn, b2@x.cn""#),
        "编辑页密送名单应回填"
    );
    // 详情页：密送行渲染（草稿详情同 email.html 模板）
    let detail = body(&route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/{id}"), b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        detail.contains("密送") && detail.contains("hidden@x.cn"),
        "详情页应渲染密送行与名单"
    );
    cleanup(&dir);
}

/// M9：转发件发送应跳回来源信箱文件夹（folder=sent 保持），而非退化到收件箱
#[test]
fn forward_compose_redirect_keeps_source_folder() {
    let (state, dir) = temp_state("m3-fwd-ctx");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    // 发件箱一封邮件（folder=sent），作为转发来源
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, subject_tokens, body_tokens, created_at,
                                sent_at)
                 VALUES ('out','sent','sent','demo@chinatelecom.cn','张伟','peer@x.cn',
                         '原发邮件','正文','原 发件','正文',1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 转发撰写页：携带 folder=sent，hidden list_query 应含 folder=sent
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "forward_from=1&folder=sent", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    let lq = html
        .split("name=\"list_query\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap_or_default();
    assert_eq!(
        lq, "folder=sent",
        "转发页 hidden list_query 应保持来源文件夹：{lq}"
    );
    // 发送：list_query 随表单提交 → 跳详情页携带 folder=sent（返回列表=发件箱）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("subject", "新主题"),
                ("body_text", "正文"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
                ("list_query", lq),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let loc = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Redirect")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert_eq!(
        loc, "/?folder=sent&just_sent=2",
        "转发发送后应回来源文件夹列表（发件箱）且带 just_sent（种子占 id=1，新草稿=2）：{loc}"
    );
    cleanup(&dir);
}

/// M9 回归：从收件箱转发/回复时 folder 省略（inbox 为默认路由），list_query_of 会
/// 把 folder= 段省掉 → 发送后回跳详情页丢文件夹，详情页"返回列表"退到邮件自身
/// folder（发送中=draft）→ 落到草稿箱。修复：撰写页 hidden list_query 始终带 folder。
#[test]
fn inbox_compose_send_redirect_keeps_inbox_folder() {
    let (state, dir) = temp_state("m9-inbox-ctx");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, subject_tokens, body_tokens, unread,
                                created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','领导','demo@chinatelecom.cn',
                         '来件','正文','来 件','正文',1,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 回复撰写页：不显式 folder（inbox 默认），hidden list_query 仍应含 folder=inbox
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "reply_to=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    let lq = html
        .split("name=\"list_query\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap_or_default();
    assert!(
        lq.contains("folder=inbox"),
        "收件箱来源的撰写页 list_query 应始终携带 folder=inbox：{lq}"
    );
    // 发送 → 跳详情页带 folder=inbox（返回列表=收件箱，而非草稿箱）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "boss@x.com"),
                ("cc_addrs", ""),
                ("subject", "回复主题"),
                ("body_text", "正文"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
                ("list_query", lq),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let loc = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Redirect")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(
        loc.starts_with("/?folder=inbox"),
        "收件箱来源发送后应回收件箱列表：{loc}"
    );
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
        // A1 后语义：两轮都处于草稿态（无调度线程时窗口未到期），
        // 重复提交 = 重新确认发送 → 重置撤销窗口并再次带 just_sent
        let expected = "/?just_sent=1";
        assert!(
            resp.extra_headers
                .iter()
                .any(|(k, v)| k == "HX-Redirect" && v == expected),
            "第 {} 轮回跳失败（首轮带 just_sent / 幂等轮不带）：{:?}",
            i + 1,
            resp.extra_headers
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
    assert_eq!(status, "draft", "重复提交保持在撤销窗口（未入队）");
    let at: i64 = state.with_conn(|c| {
        c.query_row("SELECT scheduled_at FROM emails WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert!(at > mail::now_epoch(), "撤销窗口应被重置到未来");
    assert!(at <= mail::now_epoch() + 10, "窗口仍为 8 秒左右");
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
    // 手动收取是副作用 GET：真实前端由 htmx 发起（带 HX-Request，R34 强制）
    let resp = route(
        &state,
        &assets(),
        &req_hdr("GET", "/emails/fetch", "", None, None, true),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let frag = body(&resp);
    assert!(frag.contains("凭据缺失"), "凭据缺失应在片段中提示：{frag}");
    assert!(!frag.contains("every 2s"), "失败态不应轮询");
    // 状态轮询（consume）：首次读到 just_finished → 下发 mbx:fetchDone 事件，
    // 前端做定向局部刷新（不整页 reload：Tauri 自定义协议下 reload 不稳定）
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
            .any(|(k, v)| k == "HX-Trigger" && v.contains("mbx:fetchDone")),
        "刚完成应下发完成事件：{:?}",
        resp.extra_headers
    );
    assert!(
        !resp.extra_headers.iter().any(|(k, _)| k == "HX-Refresh"),
        "完成不应再依赖整页 reload"
    );
    // 再次轮询：just_finished 已消费，不再重复下发事件
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
        !resp.extra_headers.iter().any(|(k, _)| k == "HX-Trigger"),
        "完成标记消费后不应重复下发事件"
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

/// 详情页轮询收窄（审计 R42）：2s 轮询只换 `#mail-status`，不再整页
/// `outerHTML`（旧实现每 2s 重建 #page：滚动位置弹回、菜单被关、
/// load 型「置已读」POST 反复触发）。终态片段不带 hx-get，轮询自然停止。
#[test]
fn detail_status_polling_targets_status_region_only() {
    let (state, dir) = temp_state("r42-statusbar");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, created_at, sent_at)
                 VALUES ('out','sent','queued','demo@chinatelecom.cn','peer@x.cn','排队件',
                         '排队 件','体',1,NULL),
                        ('out','sent','sent','demo@chinatelecom.cn','peer@x.cn','已发件',
                         '已发 件','体',2,2)",
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 详情页：轮询挂在状态区，目标是 this，不是 #page
    let page = body(&route(&state, &assets(), &req("GET", "/emails/1", b""), &t));
    assert!(page.contains("id=\"mail-status\""), "应有状态区容器");
    assert!(
        page.contains("hx-get=\"/emails/1/statusbar\"") && page.contains("hx-target=\"this\""),
        "轮询应指向状态区片段并就地替换"
    );
    assert!(
        !page.contains("hx-target=\"#page\""),
        "不得再整页 outerHTML 重建"
    );
    // 状态区片段：只含状态行，不含正文/侧栏/顶栏
    let frag = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1/statusbar", b""),
        &t,
    ));
    assert!(
        frag.trim_start().starts_with("<div id=\"mail-status\""),
        "片段根节点是状态区：{frag}"
    );
    assert!(frag.contains("排队中"), "queued 应显示排队文案：{frag}");
    assert!(
        frag.contains("hx-get=\"/emails/1/statusbar\""),
        "进行中应继续自轮询：{frag}"
    );
    assert!(
        !frag.contains("mail-detail") && !frag.contains("class=\"topbar\""),
        "片段不应包含正文与顶栏：{frag}"
    );
    // 终态：不再带轮询属性（服务端渲染即停止逻辑）
    let done = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/2/statusbar", b""),
        &t,
    ));
    assert!(done.contains("已发送"), "sent 片段显示终态");
    assert!(!done.contains("hx-get="), "终态不应再轮询：{done}");
    // 未配置账号：拒绝（不渲染空状态区）
    let (fresh, fresh_dir) = temp_state("r42-statusbar-empty");
    let resp = route(
        &fresh,
        &assets(),
        &req("GET", "/emails/1/statusbar", b""),
        &t,
    );
    assert_eq!(resp.status, 400, "未配置时应 400");
    cleanup(&fresh_dir);
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
    assert!(
        html.contains("需提前生成密码") && html.contains("需提前选择拉取范围"),
        "默认集团域名应展示两条 OA 指引"
    );
    // 两条 OA 指引置顶（表单标题之前），且仍在 form 内供 app.js 域名切换显隐
    assert_eq!(
        html.matches("class=\"ct-guide\"").count(),
        2,
        "首设应有且仅有两条 OA 指引"
    );
    let form_at = html.find("<form").expect("向导表单");
    let alerts_at = html.find("class=\"setup-alerts\"").expect("顶部指引块");
    let h3_at = html.find("<h3>配置邮箱账号</h3>").expect("表单标题");
    assert!(
        form_at < alerts_at && alerts_at < h3_at,
        "OA 指引应位于表单内、标题上方"
    );
    // 首设：数据保存目录默认为空、必填、提示用户自行选择。
    // 默认目录不展示在页面上，只保留隐藏属性 data-default-dir 供自动化选目录。
    assert!(
        html.contains("name=\"data_dir\" value=\"\""),
        "首设目录输入框不得预填"
    );
    assert!(
        html.contains("请先选择数据保存目录") && html.contains("data-default-dir="),
        "首设应提示选择目录并保留隐藏的默认目录属性"
    );
    let hint_at = html.find("class=\"dir-hint\"").expect("目录提示");
    let hint_end = hint_at + html[hint_at..].find("</p>").expect("提示结束");
    let hint = &html[hint_at..hint_end];
    assert!(
        !hint.contains("默认位置") && !hint.contains("class=\"mono\""),
        "目录提示不得展示默认目录路径：{hint}"
    );
    let at = html.find("name=\"data_dir\"").expect("目录输入框");
    let seg: String = html[at..].chars().take(300).collect();
    assert!(seg.contains("required"), "首设目录输入框应 required");
    cleanup(&dir);
}

#[test]
fn wizard_save_first_run_requires_data_dir_choice() {
    let (state, dir) = temp_state("wiz-dir-required");
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // ① 未选择目录：后端兜底拦截（页面 required 之外的第二道），不落库、不跳转
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
        &t,
    );
    assert!(
        body(&resp).contains("请先选择数据保存目录"),
        "未选目录应给出明确提示"
    );
    assert!(
        !resp.extra_headers.iter().any(|(k, _)| k == "HX-Redirect"),
        "未选目录不得进入完成页"
    );
    assert!(!state.configured(), "未选目录不得落库");
    // ② 显式选择目录后放行（选默认位置 = 当前数据目录，行为与旧版预填一致）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
        &t,
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
        "选择目录后应正常完成配置"
    );
    assert!(state.configured());
    assert_eq!(state.data_dir(), dir, "选当前目录不触发切换");
    // ③ 非首设：回显当前值、不再强制提示（改配置/迁移场景）
    let page = body(&route(
        &state,
        &assets(),
        &req("GET", "/welcome/setup", b""),
        &t,
    ));
    assert!(
        !page.contains("请先选择数据保存目录"),
        "已配置后不应再显示首设必选提示"
    );
    let marker = "name=\"data_dir\" value=\"";
    let at = page.find(marker).expect("已配置页应有目录输入框") + marker.len();
    let shown = &page[at..at + page[at..].find('"').expect("value 引号")];
    // minijinja autoescape 会把 '/' 转成 &#x2f;（详情页 title 同样），还原后比对
    let shown = shown.replace("&#x2f;", "/").replace("&#x2F;", "/");
    assert_eq!(
        shown,
        dir.display().to_string(),
        "已配置后应回显当前数据目录"
    );
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn ct_guides_hidden_when_domain_is_not_chinatelecom() {
    // 需求：OA 指引（生成密码/选择拉取范围）仅匹配 chinatelecom.cn 域名。
    // 非集团域名（自定义/其他企业）不适用该 OA 操作路径，不应展示。
    let (state, dir) = temp_state("ct-guide");
    // 模拟已存的非集团域名（直接写库与向导保存落库等效：页面只读 settings 值）
    state
        .set_batch(&[(keys::ACCOUNT_DOMAIN, "@custom.cn")])
        .unwrap();

    // 向导页：预填自定义域名 → 两条指引均不渲染
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
    assert!(html.contains("value=\"@custom.cn\""), "域名应预填自定义值");
    assert!(
        !html.contains("需提前生成密码"),
        "非集团域名不应展示密码指引"
    );
    assert!(
        !html.contains("需提前选择拉取范围"),
        "非集团域名不应展示拉取指引"
    );

    // 大小写不敏感（CHINATELECOM.CN 仍是集团域名）
    state
        .set_batch(&[(keys::ACCOUNT_DOMAIN, "CHINATELECOM.CN")])
        .unwrap();
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/welcome/setup", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        body(&resp).contains("需提前生成密码"),
        "集团域名大小写变体仍应展示指引"
    );
    cleanup(&dir);
}

#[test]
fn settings_panes_never_show_ct_guides() {
    // 需求（2026-11-11）：两条 OA 黄条只在首设向导顶部出现；设置页
    //（账号/同步）与非首设向导一律不展示，集团域名也不例外。
    let (state, dir) = temp_state("ct-guide-none");
    // 走真实向导保存（不 push account_domain = 默认集团域名），使设置页处于已配置态
    let pairs = wizard_pairs(dir.to_str().unwrap());
    route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(&pairs)),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    for path in [
        "/settings",
        "/settings/account",
        "/settings/panes/sync",
        "/welcome/setup",
    ] {
        let html = body(&route(&state, &assets(), &req("GET", path, b""), &t));
        assert!(
            !html.contains("class=\"ct-guide\"")
                && !html.contains("需提前生成密码")
                && !html.contains("需提前选择拉取范围"),
            "{path} 不应展示 OA 指引"
        );
    }
    state.clear_password("unit.router@chinatelecom.cn");
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
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
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
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
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
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
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
    // R38：测试二进制强制走 keyfile 回退（TEST_SKIP_KEYCHAIN），完成页必须
    // 明确提示密码没进系统凭据保管，而不是默默降级
    assert!(
        html.contains("系统凭据服务不可用"),
        "凭据降级应在完成页提示：{html}"
    );
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
    // 钳制值生效（审计 I3）：页码指示不得透出原始巨型值
    //（更完整的口径断言见 clamp_page_unifies_range / clamped_beyond_data_page_renders_empty_state）
    let html = body(&resp);
    assert!(
        !html.contains("9223372036854775807"),
        "页码指示不得透出原始巨型值：{html}"
    );
    cleanup(&dir);
}

/// 协议层来源校验（审计 R34）：自定义协议没有同源策略，WebView 里被注入的
/// 脚本或加载进来的外部页面都能直接调这些端点。浏览器自填的 Origin /
/// Sec-Fetch-Site 是唯一可信信号：跨站一律 403，应用自身来源必须放行
/// （放行口径含 http://ui.localhost——Windows 上 wry 就是这么暴露的）。
#[test]
fn protocol_rejects_foreign_origin() {
    let (state, dir) = temp_state("origin-guard");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 跨站写操作（改配置）
    let resp = route(
        &state,
        &assets(),
        &req_hdr(
            "POST",
            "/settings/sync/save",
            "",
            Some("https://evil.example"),
            None,
            true,
        ),
        &t,
    );
    assert_eq!(resp.status, 403, "跨站写入必须拒绝");
    assert_eq!(
        state.get_or(keys::SYNC_ON_START, &defaults()).as_deref(),
        Some("true"),
        "被拒请求不得产生副作用"
    );
    // 跨站读页面（可用于探测/外带数据）
    let resp = route(
        &state,
        &assets(),
        &req_hdr("GET", "/", "", Some("https://evil.example"), None, true),
        &t,
    );
    assert_eq!(resp.status, 403, "跨站读取同样拒绝");
    // Origin 缺失但浏览器标注 cross-site（如 <img>/<form> 发起的请求）
    let resp = route(
        &state,
        &assets(),
        &req_hdr("GET", "/", "", None, Some("cross-site"), true),
        &t,
    );
    assert_eq!(resp.status, 403, "Sec-Fetch-Site: cross-site 应拒绝");
    // 应用自身来源与不可判定（自定义协议可能不带 Origin / 写 null）放行
    for origin in [
        "http://ui.localhost",
        "http://tauri.localhost",
        "null",
        "ui://localhost",
    ] {
        let resp = route(
            &state,
            &assets(),
            &req_hdr("GET", "/", "", Some(origin), Some("same-origin"), true),
            &t,
        );
        assert_eq!(resp.status, 200, "{origin} 应放行");
    }
    cleanup(&dir);
}

/// 远程图片默认阻止（审计 R4/R36）：开关关闭时 `/proxy/image` 必须直接回
/// 透明占位图，不解析域名、不建连——用永不解析的 `.invalid` 域名取证：
/// 若代码试图联网，结果是 403 而不是 200。开启开关后同一 URL 才走代理路径。
#[test]
fn remote_images_blocked_until_enabled() {
    let (state, dir) = temp_state("remote-img");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let img_req = || {
        req_q(
            "GET",
            "/proxy/image",
            "url=https%3A%2F%2Fimg.invalid%2Fa.png",
            b"",
        )
    };

    let resp = route(&state, &assets(), &img_req(), &t);
    assert_eq!(resp.status, 200, "默认关闭时应回占位图而非错误");
    assert_eq!(resp.content_type, "image/png");
    assert_eq!(resp.cache_control, "no-store", "开关一变必须立刻生效");
    assert!(
        resp.body.starts_with(&[0x89, b'P', b'N', b'G']) && resp.body.len() < 200,
        "应为内置小体积 PNG 占位图"
    );

    // 打开开关（缺省其他字段保持现值），同一 URL 走真实代理：解析失败 → 403
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[
                ("sync_on_start", "true"),
                ("sync_interval_min", "30"),
                ("first_fetch_days", "0"),
                ("allow_remote_images", "1"),
            ]),
        ),
        &t,
    );
    assert!(state.allow_remote_images(), "开关应落库");
    let resp = route(&state, &assets(), &img_req(), &t);
    assert_eq!(resp.status, 403, "开启后应真正进入代理网络路径");

    // 未提交该字段的旧表单不得把开关冲回默认（保持现值）
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[("sync_on_start", "true")]),
        ),
        &t,
    );
    assert!(state.allow_remote_images(), "缺省字段应保持现值");
    cleanup(&dir);
}

/// 远程图片被阻止时详情页必须说明「为什么图是空的」并给出开启入口；
/// 打开开关后提示消失。`/settings?tab=sync` 深链要直达开关所在 pane，
/// 否则用户点提示后落在账号页、找不到开关（审计 R4/R36）。
#[test]
fn remote_image_notice_and_settings_deeplink() {
    let (state, dir) = temp_state("remote-notice");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "@x.cn"),
        ])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, body_html, subject_tokens, body_tokens,
                                    unread, created_at, received_at, thread_key)
                 VALUES ('in','inbox','received','a@b.cn','A','demo@x.cn','带图','图',
                         '<p><img src=\"/proxy/image?url=https%3A%2F%2Fimg.invalid%2Fa.png\"></p>',
                         '带图','图',1,1,1,'带图')",
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };

    let html = body(&route(&state, &assets(), &req("GET", "/emails/1", b""), &t));
    assert!(
        html.contains("远程图片已阻止加载"),
        "被阻止时详情页应说明原因：{html}"
    );
    assert!(
        html.contains("/settings?tab=sync"),
        "提示应直达开关所在 pane：{html}"
    );

    state
        .set_batch(&[(keys::ALLOW_REMOTE_IMAGES, "1")])
        .unwrap();
    let html = body(&route(&state, &assets(), &req("GET", "/emails/1", b""), &t));
    assert!(
        !html.contains("远程图片已阻止加载"),
        "开关打开后不应再提示：{html}"
    );

    // 深链：tab=sync 渲染同步 pane（含开关）；未知 tab 回落账号 pane
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/settings", "tab=sync", b""),
        &t,
    ));
    assert!(
        html.contains("allow_remote_images") && html.contains("收件同步"),
        "深链应落在同步 pane：{html}"
    );
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/settings", "tab=bogus", b""),
        &t,
    ));
    assert!(
        !html.contains("allow_remote_images"),
        "未知 tab 应回落账号 pane：{html}"
    );
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
    assert!(
        body(&resp).contains("已保存") && body(&resp).contains("class=\"ic\""),
        "保存后片段应回显已保存（SVG 勾）"
    );
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

/// 存储保存后自动按新策略清理（2026-11-03 需求：改短保留时长立即生效，
/// 无需等每日定时器或手动点立即清理）+「立即清理」按钮在有可释放项时可用
#[test]
fn settings_storage_save_triggers_cleanup_and_enables_clean_button() {
    let (state, dir) = temp_state("sto-save-clean");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let now = mail::now_epoch();
    // 首拉形态历史件：今天入库（created_at=now）、40 天前接收
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, subject_tokens, body_tokens, unread,
                                created_at, received_at)
             VALUES ('in','inbox','received','老邮件','a@x.cn','甲','demo@chinatelecom.cn',
                     '旧正文','老 邮件','旧 正文',0,?1,?2)",
                rusqlite::params![now, now - 40 * 86_400],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 有过期项时按钮可用（按钮窗口内不得出现 disabled）
    state.set_batch(&[(keys::KEEP_INBOX_DAYS, "30")]).unwrap();
    let pane = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/storage", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    let btn_ctx = pane
        .lines()
        .position(|l| l.contains("hx-post=\"/settings/storage/clean\""))
        .map(|i| {
            let s = i.saturating_sub(2);
            let e = (i + 5).min(pane.lines().count());
            pane.lines()
                .skip(s)
                .take(e - s)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    assert!(!btn_ctx.is_empty(), "应找到立即清理按钮：{pane}");
    // 可用态判定用 title：禁用分支 title 为"当前没有可释放的空间"
    //（按钮固定声明含 hx-disabled-elt 属性名，不能以 disabled 字样判）
    assert!(
        btn_ctx.contains("title=\"按上方策略立即执行清理\""),
        "有待清理项时立即清理按钮应可用：{btn_ctx}"
    );
    // 保存 30 天保留 → 自动执行清理：老邮件删除、报告显示
    //（保存响应为 pane 片段，须取完整Resp看响应头）
    let save_resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/storage/save",
            &form(&[
                ("inline_attach_max_mb", "20"),
                ("clean_sent_bulk_days", ""),
                ("keep_inbox_days", "30"),
                ("keep_sent_days", ""),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let resp = body(&save_resp);
    assert!(
        resp.contains("过期邮件 1"),
        "保存后自动清理应报告删除结果：{}",
        resp.lines()
            .find(|l| l.contains("清理完成") || l.contains("暂无可清理"))
            .unwrap_or("(未找到报告)")
    );
    assert!(
        save_resp
            .extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Trigger" && v.contains("mbx:fetchDone")),
        "清理删除了邮件，保存响应须携带 HX-Trigger mbx:fetchDone 驱动边栏局部刷新：{:?}",
        save_resp.extra_headers
    );
    let total: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(total, 0, "40 天前的历史件应随保存动作被清理");
    // 清理后无待清理项 → 按钮禁用 + 文案
    let pane2 = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/storage", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        pane2.contains("title=\"当前没有可释放的空间\""),
        "无可释放空间时按钮应有禁用提示：{pane2}"
    );
    assert!(
        pane2.contains("收件箱保留时长"),
        "文案应为收件箱保留时长：{pane2}"
    );
    cleanup(&dir);
}

#[test]
fn settings_sync_refetch_resets_cursor_and_keeps_range() {
    let (state, dir) = temp_state("sync-refetch");
    // 预置：已拉过 90 天并落游标（模拟真实首拉后的状态），
    // 首拉阶段化标志也已置位
    state
        .set_batch(&[
            (keys::FIRST_FETCH_DAYS, "90"),
            (keys::LAST_UID, "9503"),
            (keys::LAST_UIDVALIDITY, "1"),
            (keys::FIRST_PULL_DONE_SENT, "1"),
            (keys::FIRST_PULL_DONE_INBOX, "1"),
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
    assert_eq!(
        state.get_or(keys::FIRST_PULL_DONE_SENT, &d).as_deref(),
        Some("0"),
        "首拉阶段化标志须随游标一并清除（重新进入发件箱优先串行模式）"
    );
    assert_eq!(
        state.get_or(keys::FIRST_PULL_DONE_INBOX, &d).as_deref(),
        Some("0"),
        "收件箱首拉标志同样须清除"
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
    assert!(body(&resp).contains("已保存") && body(&resp).contains("class=\"ic\""));
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
        Some("3650"),
        "非法值保持默认（发件箱保留默认 3650 天）"
    );
    cleanup(&dir);
}

/// 附件内联上限钳制到 1..=2048 MB（旧实现只校验"是数字"，可写成 40 亿 → 全走 BLOB）
#[test]
fn settings_storage_clamps_inline_limit() {
    let (state, dir) = temp_state("sto-clamp");
    let post = |v: &str| {
        route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/storage/save",
                &form(&[
                    ("inline_attach_max_mb", v),
                    ("clean_sent_bulk_days", "7"),
                    ("keep_inbox_days", "365"),
                    ("keep_sent_days", "180"),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        )
    };
    let d = defaults();
    let _ = post("999999");
    assert_eq!(
        state.get_or(keys::INLINE_ATTACH_MAX_MB, &d).as_deref(),
        Some("2048"),
        "超大值应钳制到上限"
    );
    let _ = post("0");
    assert_eq!(
        state.get_or(keys::INLINE_ATTACH_MAX_MB, &d).as_deref(),
        Some("1"),
        "0 应钳制到下限（0 等于全部走独立文件）"
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
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
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
    assert!(body(&resp).contains("已保存") && body(&resp).contains("class=\"ic\""));
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
    // 只读字段（服务器/账号）即便随表单提交也不得改动
    assert_eq!(
        state.get_or(keys::SMTP_HOST, &d).as_deref(),
        Some("smtp.test.cn"),
        "服务器字段只读：表单值必须被忽略"
    );
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn wizard_save_with_custom_domain() {
    let (state, dir) = temp_state("wiz-domain");
    let mut pairs = wizard_pairs(dir.to_str().unwrap());
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
fn wizard_save_first_run_custom_dir_switches_immediately() {
    // 需求：首设向导选非默认目录 → 不走迁移（空库无数据可迁），会话内直切立即生效。
    // 覆盖：范围落新库、done 页“已设置（无需重启）”文案、旧默认目录空库清理、
    // 重复提交幂等。
    let (state, dir) = temp_state("wiz-direct");
    let target = std::env::temp_dir().join(format!("mbx-wiz-direct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);

    let mut pairs = wizard_pairs(dir.to_str().unwrap());
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
        "含自定义目录的向导保存应跳转完成页"
    );
    assert!(state.configured(), "配置应已落库（新目录的库）");
    assert_eq!(
        state.data_dir(),
        target,
        "首设空库直切应会话内立即切换数据目录"
    );
    assert!(target.join("corpmail.db").exists(), "新目录应有数据库");
    assert!(state.pending_data_dir().is_none(), "直切后不应有待重启目录");
    // 旧默认目录（== 锚点）的启动空库应被清理，避免存储页误展示“迁移备份”
    assert!(
        !dir.join("corpmail.db").exists(),
        "旧默认目录的空库文件应被清理"
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
    let done = body(&resp);
    assert!(
        done.contains("数据目录已设置为") && done.contains("无需重启"),
        "done 页应提示直切生效：{done}"
    );
    assert!(
        !done.contains("重启应用后生效"),
        "首设直切不应出现迁移的重启提示：{done}"
    );

    // 重启视角：指针（锚点 corpmail.toml）→ 新目录，配置与首拉范围都在新库
    {
        let state3 = AppState::open_with_anchor(&target, &dir);
        assert!(
            state3.configured(),
            "重启视角账号配置完好（新库 + 锚点 toml）"
        );
        assert_eq!(
            state3
                .get_or(keys::FIRST_FETCH_DAYS, &defaults())
                .as_deref(),
            Some("7"),
            "重启视角应保留向导保存的首拉范围"
        );
    }

    // 重复提交同目录：已切换（want == cur）→ 幂等跳过，不再直切/迁移
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
        "重复提交同目录应幂等跳过并正常跳转"
    );

    // 选择默认位置（dir2 自身）的新库向导：无目录提示
    let (state2, dir2) = temp_state("wiz-direct2");
    let resp2 = route(
        &state2,
        &assets(),
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir2.to_str().unwrap())),
        ),
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
        "默认目录保存应成功"
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
        !body(&resp2).contains("数据目录已设置"),
        "未指定目录时不应出现目录提示"
    );
    state.clear_password("unit.router@chinatelecom.cn");
    state2.clear_password("unit.router@chinatelecom.cn");
    let _ = std::fs::remove_dir_all(&target);
    cleanup(&dir);
    cleanup(&dir2);
}

#[test]
fn wizard_save_prefilled_current_dir_redirects() {
    // 回归：向导 data_dir 预填当前目录（与存储页同源回显），用户不动它直接
    // 提交 = 不更换，不得送进直切校验（validate_data_dir 拒绝同路径）。
    // 曾导致首配必失败：返回「数据目录设置失败」片段且不跳转 done 页，
    // E2E 01-wizard 等 /welcome/done 无限轮询getUrl。
    let (state, dir) = temp_state("wiz-prefilled");
    let mut pairs = wizard_pairs(dir.to_str().unwrap());
    pairs.push(("data_dir", dir.to_str().unwrap()));
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
        "预填当前目录（不更换）应正常跳转完成页"
    );
    assert!(state.configured(), "配置应已落库");
    assert_eq!(state.data_dir(), dir, "不动目录时数据目录应保持不变");
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn settings_account_ignores_readonly_submissions() {
    // 需求：账号页仅密码与发件显示名可修改。账号/域名/双通道服务器/加密方式
    // 一律取已存值——即使客户端绕过禁用态提交了这些字段也不得生效
    let (state, dir) = temp_state("acc-readonly");
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/account/save",
            &form(&[
                ("account_prefix", "hacker"),
                ("account_domain", "@evil.cn"),
                ("password", ""),
                ("display_name", "新显示名"),
                ("smtp_host", "smtp.evil.cn"),
                ("smtp_port", "25"),
                ("smtp_security", "plain"),
                ("recv_host", "imap.evil.cn"),
                ("recv_port", "143"),
                ("recv_security", "plain"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        body(&resp).contains("已保存") && body(&resp).contains("class=\"ic\""),
        "{}",
        body(&resp)
    );
    let d = defaults();
    assert_eq!(
        state.get_or(keys::ACCOUNT_PREFIX, &d).as_deref(),
        Some("unit.router"),
        "账号前缀不得被改动"
    );
    assert_eq!(
        state.get_or(keys::ACCOUNT_DOMAIN, &d).as_deref(),
        Some("@chinatelecom.cn"),
        "域名不得被改动"
    );
    assert_eq!(
        state.get_or(keys::SMTP_HOST, &d).as_deref(),
        Some("smtp.test.cn"),
        "发信服务器不得被改动"
    );
    assert_eq!(
        state.get_or(keys::RECV_HOST, &d).as_deref(),
        Some("imap.test.cn"),
        "收信服务器不得被改动"
    );
    assert_eq!(
        state.get_or(keys::SMTP_SECURITY, &d).as_deref(),
        Some("auto"),
        "加密方式不得被改动"
    );
    assert_eq!(
        state.get_or(keys::DISPLAY_NAME, &d).as_deref(),
        Some("新显示名"),
        "发件显示名是唯一可改字段之一"
    );
    // 原账号凭据保持完好，伪造账号不得获得凭据
    assert!(matches!(
        state.retrieve_password("unit.router@chinatelecom.cn"),
        GetResult::Keychain(p) | GetResult::Keyfile(p) if p == "pw-test"
    ));
    assert!(matches!(
        state.retrieve_password("hacker@evil.cn"),
        GetResult::Missing
    ));
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn settings_account_save_requires_configured_account() {
    // 账号页只读化：账号由首次配置确定——未配置时不得经设置页落库
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
        body(&resp).contains("尚未配置账号"),
        "未配置账号时应拒绝从设置页保存：{}",
        body(&resp)
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
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
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
    assert!(html.contains("class=\"ic\""), "假测试器应成功（SVG 勾）");
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

/// page 参数归一（审计 I3/R60）：list_root 与 rebuild_list_query 共用 clamp_page，
/// 口径统一 1..=10000（畸形→1，超上限→10000；100000 页 × 50 封属于无意义的深扫）
#[test]
fn clamp_page_unifies_range() {
    assert_eq!(clamp_page(None), 1, "缺省第 1 页");
    assert_eq!(clamp_page(Some("")), 1, "空串非法 → 1");
    assert_eq!(clamp_page(Some("abc")), 1, "非数字 → 1");
    assert_eq!(clamp_page(Some("0")), 1, "0 → 下限 1");
    assert_eq!(clamp_page(Some("-5")), 1, "负数 → 下限 1");
    assert_eq!(clamp_page(Some("1")), 1);
    assert_eq!(clamp_page(Some("30")), 30, "范围内原样");
    assert_eq!(clamp_page(Some("10000")), 10_000, "恰好上限原样");
    assert_eq!(clamp_page(Some("10001")), 10_000, "超上限钳到 10000");
    assert_eq!(
        clamp_page(Some("9999999999999")),
        10_000,
        "巨型值钳到 10000（防大 OFFSET 扫描）"
    );
    assert_eq!(clamp_page(Some("+3")), 3, "带符号合法值解析");
}

/// per 参数归一：下限 1（E2E B8 用 per=1 测翻页）、上限 100；非法/缺省 → None
#[test]
fn clamp_per_allows_single_page() {
    assert_eq!(clamp_per(None), None, "缺省 → None（回退 settings）");
    assert_eq!(clamp_per(Some("")), None, "空串非法 → None");
    assert_eq!(clamp_per(Some("abc")), None, "非数字 → None");
    assert_eq!(clamp_per(Some("0")), Some(1), "0 → 下限 1");
    assert_eq!(clamp_per(Some("1")), Some(1), "per=1 原样（分页最小粒度）");
    assert_eq!(clamp_per(Some("10")), Some(10), "合法默认值原样");
    assert_eq!(clamp_per(Some("100")), Some(100), "恰好上限原样");
    assert_eq!(clamp_per(Some("200")), Some(100), "超上限钳到 100");
    assert_eq!(clamp_per(Some("-7")), Some(1), "负数 → 下限 1");
}

/// 大页码钳到 100000 后列表页正常渲染（有数据时该页为空态而非崩溃）
#[test]
fn clamped_beyond_data_page_renders_empty_state() {
    let (state, dir) = temp_state("clamp-page");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    // 默认每页 10：11 封 → 2 页（per 语义 2026-11-04：全局常量 PAGE_SIZE 已
    // 除名，分页规格来自 settings/URL，本测试走 HTTP 路由即默认 10）
    state
        .with_conn(|c| {
            for i in 0..11 {
                c.execute(
                    "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, created_at)
                 VALUES ('in','inbox','received','a@x.cn','b@x.cn',?1,?1,'x',1)",
                    [i],
                )
                .unwrap();
            }
            Ok::<(), String>(())
        })
        .unwrap();
    // 页 2（数据范围内）：分页器显示 2 / 2
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "page=2", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("2 / 2"),
        "范围内页码分页器应正常：{}",
        html.lines()
            .find(|l| l.contains("pg-cur"))
            .unwrap_or("(无分页器)")
    );
    // 首页/末页（批次五）：最后一页有首页、无末页
    assert!(html.contains("« 首页"), "非首页应有「首页」入口");
    assert!(!html.contains("末页 »"), "最后一页不应出现「末页」");
    // 第 1 页：有末页（指向 page=2）、无首页
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "page=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("末页 »"), "首页应有「末页」入口");
    assert!(!html.contains("« 首页"), "首页不应出现「首页」");
    // 页 100001 → 钳 100000：超数据范围为空态（而非 panic 或巨型 OFFSET）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "page=100001", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("这里还没有邮件"),
        "钳后页码超数据范围应渲染空态"
    );
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
            .replace("src=\"/office-preview.js\"", "src=\"office-preview.js\"")
            .replace("href=\"/favicon.svg\"", "href=\"favicon.svg\"")
            // wangEditor vendor 资源（仅 compose 页引入）
            .replace(
                "href=\"/vendor/wang-editor/style.css\"",
                "href=\"vendor/wang-editor/style.css\"",
            )
            .replace(
                "src=\"/vendor/wang-editor/index.js\"",
                "src=\"vendor/wang-editor/index.js\"",
            );
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
            // M3-2 签名档种子：设置页回显 + 新邮件预填冒烟
            (
                keys::SIGNATURE,
                "张伟\n中国电信广州分公司 政企客户部\n电话：020-83820000",
            ),
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
            bcc_addrs: String::new(),
            subject: "关于宽带提速配案的意见反馈".into(),
            body_text: "各位领导：\n请查阅本月配案清单，重点客户需在月底前完成提速交付。".into(),
            in_reply_to: None,
            scheduled_at: 0,
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
            bcc_addrs: String::new(),
            subject: "Re: 月度激励汇总表".into(),
            body_text: "收到，我这边同步核对一遍数据口径。".into(),
            in_reply_to: Some(d1),
            scheduled_at: 0,
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
                                        unread, created_at, received_at, thread_key)
                     VALUES ('in', 'inbox', 'received', ?1, ?2, 'demo@chinatelecom.cn',
                             ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
                rusqlite::params![
                    faddr,
                    fname,
                    subj,
                    body,
                    stok,
                    btok,
                    att,
                    *unread as i64,
                    t,
                    mail::thread_key_of(subj)
                ],
            )
            .unwrap();
        });
        let id = d2 + 1 + i as i64;
        if i == 0 {
            inbox_first_id = id;
            // HTML 正文冒烟样例：表格/内联样式应渲染保留，script 与 javascript: 链接
            // 应被净化移除——供浏览器目视核对 HTML 渲染与 XSS 防护
            let sample = r#"<div style="font-family:sans-serif"><h2 style="color:#0057c2">配案清单（HTML 版）</h2><p>各位领导：</p><table border="1" cellpadding="6" style="border-collapse:collapse"><tr><th>客户</th><th>速率</th></tr><tr><td>示例客户 A</td><td>200M</td></tr></table><p><a href="javascript:alert(1)">危险链接</a><script>alert('xss')</script></p></div>"#;
            let safe = mail::sanitize_html(sample);
            // A5 原始信头种子：源码页三视图冒烟（Received 链 + charset 声明）
            let headers = "Return-Path: <boss@chinatelecom.cn>\r\nReceived: from mx.chinatelecom.cn (mx.chinatelecom.cn [10.0.0.8])\r\n\tby imap.chinatelecom.cn with ESMTPS id abc123;\r\n\tTue, 03 Nov 2026 09:12:31 +0800 (CST)\r\nFrom: =?utf-8?B?546L5Ymv5oC757un55CG?= <boss@chinatelecom.cn>\r\nTo: demo@chinatelecom.cn\r\nSubject: =?utf-8?B?5r+A5Yqx5riF5Y2V5bey5om55aSN77yM6K+35oyJ5pa55qGI5omn6KGM?=\r\nMessage-Id: <seed-1@chinatelecom.cn>\r\nContent-Type: text/html; charset=utf-8\r\nX-Mailer: CorpMail-Test";
            state.with_conn(|c| {
                c.execute(
                    "UPDATE emails SET body_html = ?1, body_kind = 'html', raw_headers = ?3 WHERE id = ?2",
                    rusqlite::params![safe, id, headers],
                )
                .unwrap();
            });
        }
    }
    let inbox_id = inbox_first_id;

    // M2-2 会话归组冒烟：发一封与首封收件同主题的回复（sent），
    // thread_key 与首封收件一致 → 两封详情页均出现「会话 · 2 封往来」入口
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                        subject, subject_tokens, body_text, body_tokens,
                                        unread, created_at, sent_at, thread_key)
                     VALUES ('out', 'sent', 'sent', 'demo@chinatelecom.cn', '张伟',
                             'boss@chinatelecom.cn', 'Re: 激励清单已批复，请按方案执行',
                             '激励 清单 已 批复 请 按 方案 执行',
                             '收到，我按方案推进并同步进度。', '收到 我 按 方案 推进 并 同步 进度',
                             0, ?1, ?1, ?2)",
                rusqlite::params![now, mail::thread_key_of("Re: 激励清单已批复，请按方案执行")],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();

    // M2-1 星标素材：第二封收件打上星标——列表页金色★且置顶（跨时间序）
    state
        .with_conn(|c| {
            c.execute(
                "UPDATE emails SET starred = 1 WHERE id = ?1",
                [inbox_id + 1],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();

    dump("main.html", get("/"));
    dump("main-inbox-unread.html", get_url("/?unread=1"));
    // A2 排序素材：时间正序 / 发件人升序（对照默认最新优先）
    dump("main-sort-asc.html", get_url("/?sort=date_asc"));
    dump("main-sort-from.html", get_url("/?sort=from_asc"));
    dump("main-sort-subject.html", get_url("/?sort=subject_asc"));
    // B1 高级检索素材：发件人 / 仅附件（面板默认展开 + 条件回显）
    dump("main-adv-from.html", get_url("/?from=partner"));
    dump("main-adv-att.html", get_url("/?att=1"));
    // M4-2 定时草稿种子：第三个草稿设 2h 后定时——列表 ⏰ 徽标实测
    state
        .with_conn(|c| {
            let at = mail::now_epoch() + 7200;
            c.execute(
                "UPDATE emails SET scheduled_at = ?1
                  WHERE folder = 'draft' AND status = 'draft'
                    AND subject = 'Re: 月度激励汇总表'",
                [at],
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();
    dump("main-draft.html", get_url("/?folder=draft"));
    // 定时草稿详情页（「已定时」状态条）
    dump("email-scheduled.html", get(&format!("/emails/{d2}")));
    // 既有数据沿用后的完成页（重装/换机采纳目录 → 沿用提示可见）
    dump("done-reuse.html", get("/welcome/done"));
    // M2-2 会话素材：详情页会话入口 + 会话详情（入口下沉，列表分支已删）
    dump("thread-detail.html", get(&format!("/threads/{inbox_id}")));
    dump("email-inbox.html", get(&format!("/emails/{inbox_id}")));
    // A5 源码页素材：HTML 源码 / 纯文本 / 原始信头三视图
    dump(
        "source-html.html",
        get(&format!("/emails/{inbox_id}/source")),
    );
    dump(
        "source-text.html",
        get_url(&format!("/emails/{inbox_id}/source?part=text")),
    );
    dump(
        "source-headers.html",
        get_url(&format!("/emails/{inbox_id}/source?part=headers")),
    );
    // M2-1 星标详情页素材（email-inbox.html 为未星标态，两态各有对照）
    dump(
        "email-starred.html",
        get(&format!("/emails/{}", inbox_id + 1)),
    );
    // 带列表上下文（未读筛选 + 检索 + 第 2 页）：验证返回/回复/删除链接保持状态
    dump(
        "email-inbox-filtered.html",
        get_url(&format!(
            "/emails/{inbox_id}?folder=inbox&unread=1&q=%E6%BF%80%E5%8A%B1&page=2"
        )),
    );
    dump("email-draft.html", get(&format!("/emails/{d1}")));
    // B2 模板素材：两条模板（纯文本 + HTML），撰写页下拉与设置页列表共用
    {
        use crate::mail::templates;
        templates::save(&state, None, "收到请回复", "收到，我核对后回复您。", "").unwrap();
        templates::save(
            &state,
            None,
            "月度报送",
            "各位领导：本月清单见附件，请查收。",
            "<p>各位领导：</p><p>本月清单见附件，请查收。</p>",
        )
        .unwrap();
    }
    dump("compose.html", get("/compose"));
    dump(
        "compose-reply.html",
        get_url(&format!("/compose?reply_to={inbox_id}")),
    );
    dump(
        "compose-draft.html",
        get_url(&format!("/compose?draft_id={d2}")),
    );
    // M6 转发素材：带附件提示的转发撰写页（种子第二封 attach_cnt=1）
    dump(
        "compose-forward.html",
        get_url(&format!("/compose?forward_from={}", inbox_id + 1)),
    );
    // B6 作为附件转发素材：原主题预填 + .eml 携带提示
    dump(
        "compose-attach-eml.html",
        get_url(&format!("/compose?attach_eml={}", inbox_id + 1)),
    );
    // M7 回复全部：发件人 + 收件人 + 抄送（去自己）
    dump(
        "compose-reply-all.html",
        get_url(&format!("/compose?reply_all_to={}", inbox_id + 4)),
    );
    // M7 返回链接保过滤：带未读/外部/搜索/页码进入详情
    dump(
        "email-filtered.html",
        get_url(&format!(
            "/emails/{}?unread=1&external=1&q=%E6%99%BA%E6%85%A7&page=2",
            inbox_id + 4
        )),
    );
    // M6 状态条素材：发件箱同步阶段（data-running=true 供按钮禁用联调）。
    // ETA 场景两态：大批量拉取（12/48，10s 拉了 12 → 剩 36 ≈ 30s <60s 不显示）
    // 与超大首拉（120/9600，120s 拉 120 → 剩 9480 ≈ 2.6h 显示"约 2 小时 36 分钟"）
    {
        let mut s = state.fetch_state.lock().unwrap();
        s.running = true;
        s.phase = "sent".into();
        s.started_at = mail::now_epoch();
        s.fetch_done = 12;
        s.fetch_total = 48;
        let now = mail::now_epoch();
        s.progress_started_at = now - 10;
        s.progress_updated_at = now;
        s.progress_started_done = 0;
    }
    dump("fetch-status-running.html", get("/emails/fetch/status"));
    {
        let mut s = state.fetch_state.lock().unwrap();
        let now = mail::now_epoch();
        s.fetch_done = 120;
        s.fetch_total = 9600;
        s.progress_started_at = now - 120;
        s.progress_updated_at = now;
        s.progress_started_done = 0;
    }
    dump("fetch-status-eta.html", get("/emails/fetch/status"));
    {
        let mut s = state.fetch_state.lock().unwrap();
        s.running = false;
        s.phase.clear();
        s.fetch_done = 0;
        s.fetch_total = 0;
        s.progress_started_at = 0;
        s.progress_updated_at = 0;
        s.progress_started_done = 0;
    }

    // M3 发送状态素材：sending 轮询态 / failed 重发态 / sent 终态 + 已发送列表。
    // 注意：此前用 d2+2/3/4 推算 id 会与上方 inbox 种子(id 3..8)冲突，导致
    // email-sent.html 等实际渲染的是收件箱邮件——改为插入后取 last_insert_rowid。
    let mut sending_id = 0i64;
    let mut failed_id = 0i64;
    let mut sent_id = 0i64;
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
        sending_id = c.last_insert_rowid();
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
        failed_id = c.last_insert_rowid();
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
        sent_id = c.last_insert_rowid();
    });
    dump("email-sending.html", get(&format!("/emails/{sending_id}")));
    dump("email-failed.html", get(&format!("/emails/{failed_id}")));
    dump("email-sent.html", get(&format!("/emails/{sent_id}")));
    dump("main-sent.html", get_url("/?folder=sent"));
    // 外部发件人素材：外部筛选视图 + 外部来信详情（种子第 5 封为合作方域）
    dump("main-external.html", get_url("/?external=1"));
    dump(
        "email-external.html",
        get(&format!("/emails/{}", inbox_id + 4)),
    );

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

    // A1 撤销窗口素材：即将发送详情（scheduled_at = now+5）+ 撤销后的撰写页提示
    state.with_conn(|c| {
        c.execute(
            "UPDATE emails SET scheduled_at = ?1 WHERE id = ?2",
            rusqlite::params![mail::now_epoch() + 5, d1],
        )
        .unwrap();
    });
    dump("email-scheduled-soon.html", get(&format!("/emails/{d1}")));
    dump(
        "compose-undone.html",
        get_url(&format!("/compose?draft_id={d1}&undone=1")),
    );

    // 首设直切素材：全新状态真实走一遍向导（含自定义目录）。
    // 验证点：done 页“已设置（无需重启）”、存储页当前目录 = 新目录且无“待生效/备份”
    {
        let fresh = root.join("first-run");
        std::fs::create_dir_all(&fresh).unwrap();
        let st = AppState::open_with_anchor(&fresh, &fresh);
        let target = root.join("first-run-target");
        let mut pairs = wizard_pairs(fresh.to_str().unwrap());
        pairs.push(("data_dir", target.to_str().unwrap()));
        let resp = route(
            &st,
            &a,
            &req("POST", "/welcome/setup/save", &form(&pairs)),
            &t,
        );
        assert!(
            resp.extra_headers
                .iter()
                .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
            "冒烟：首设直切向导应保存成功"
        );
        dump(
            "done-switched.html",
            body(&route(&st, &a, &req("GET", "/welcome/done", b""), &t)),
        );
        dump(
            "storage-switched.html",
            body(&route(&st, &a, &req("GET", "/settings/storage", b""), &t)),
        );
    }

    dump("settings-account.html", get("/settings"));
    dump("settings-sync.html", get("/settings/sync"));
    dump("settings-storage.html", get("/settings/storage"));
    dump("settings-about.html", get("/settings/about"));
    dump("settings-templates.html", get("/settings/templates"));

    // M3-3 联系人页素材：smoke 直写 emails 表不经过采集管道，手动补
    // contacts 种子（姓名/最近联系偏移/往来次数各异）后 dump 整页 + 搜索态
    {
        let cnow = mail::now_epoch();
        state
            .with_conn(|c| {
                c.execute_batch(&format!(
                    "INSERT INTO contacts(addr, name, updated_at, hits) VALUES
                     ('boss@chinatelecom.cn', '王副总经理', {a}, 12),
                     ('finance@chinatelecom.cn', '财务部-李静', {a}, 3),
                     ('market@partner.cn', '合作方-智慧园区项目组', {b}, 2),
                     ('hr@chinatelecom.cn', '人力资源部', {b}, 1)",
                    a = cnow - 1800,
                    b = cnow - 3 * 86_400,
                ))
                .map_err(|e| e.to_string())
            })
            .unwrap();
    }
    dump("contacts.html", get("/contacts"));
    dump(
        "contacts-search.html",
        get_url("/contacts?q=%E7%8E%8B"), // 「王」
    );
    // B3 编辑态素材（地址回填 + 只读）
    dump(
        "contacts-edit.html",
        get_url("/contacts?edit=boss%40chinatelecom.cn"),
    );

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
    for f in [
        "app.css",
        "htmx.min.js",
        "app.js",
        "office-preview.js",
        "favicon.svg",
    ] {
        std::fs::copy(ui_dist.join(f), root.join(f)).unwrap();
    }
    // wangEditor vendor（compose 页富文本编辑器）：保持子目录结构
    let vendor = root.join("vendor/wang-editor");
    std::fs::create_dir_all(&vendor).unwrap();
    for f in ["index.js", "style.css", "VERSION.txt"] {
        std::fs::copy(ui_dist.join("vendor/wang-editor").join(f), vendor.join(f)).unwrap();
    }
    // Office 预览 vendor（xlsx/docx/pptx 解析脚本）：预览实测需要
    let office = root.join("vendor/office");
    std::fs::create_dir_all(&office).unwrap();
    if let Ok(rd) = std::fs::read_dir(ui_dist.join("vendor/office")) {
        for e in rd.flatten() {
            if e.path().is_file() {
                std::fs::copy(e.path(), office.join(e.file_name())).unwrap();
            }
        }
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
    for p in ["/", "/compose", "/emails/1", "/settings", "/welcome/done"] {
        let resp = route(&fresh, &a, &req("GET", p, b""), &t);
        assert_eq!(resp.status, 200, "{p} 未配置应渲染欢迎页");
    }
    // 副作用 GET 非 htmx 请求：400（R34）；带 HX-Request 才进业务分支
    let resp = route(&fresh, &a, &req("GET", "/emails/fetch", b""), &t);
    assert_eq!(resp.status, 400, "手动收取非 htmx 请求应拒绝");
    let resp = route(
        &fresh,
        &a,
        &req_hdr("GET", "/emails/fetch", "", None, None, true),
        &t,
    );
    assert_eq!(resp.status, 200, "htmx 发起的手动收取应正常");
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

/// 设置·账号：加密方式落盘（conf 键）且端口推断生效；表单非法值回落已存值
#[test]
fn settings_account_pane_disables_readonly_fields() {
    // 需求：账号页只有密码与发件显示名允许修改——其余字段渲染为禁用态
    let (state, dir) = temp_state("acct-readonly-ui");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/account", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    for field in [
        "name=\"smtp_host\"",
        "name=\"smtp_port\"",
        "name=\"smtp_security\"",
        "name=\"recv_host\"",
        "name=\"recv_port\"",
        "name=\"recv_security\"",
    ] {
        let idx = html
            .find(field)
            .unwrap_or_else(|| panic!("账号页应有字段 {field}"));
        let tag_end = html[idx..].find('>').unwrap() + idx;
        assert!(
            html[idx..tag_end].contains("disabled"),
            "{field} 应为禁用态：{}",
            &html[idx..tag_end]
        );
    }
    // 可编辑字段不得带禁用态
    for field in ["name=\"password\"", "name=\"display_name\""] {
        let idx = html.find(field).expect("应有可编辑字段 {field}");
        let tag_end = html[idx..].find('>').unwrap() + idx;
        assert!(
            !html[idx..tag_end].contains("disabled"),
            "{field} 必须可编辑"
        );
    }
    // 登录账号展示为完整地址的只读回显
    assert!(html.contains("demo@chinatelecom.cn"), "应展示完整登录账号");
    assert!(
        html.contains("仅可修改密码与发件显示名"),
        "应说明可修改范围"
    );
    cleanup(&dir);
}

/// 设置·收件同步：单封上限落盘并钳制到 1..=200MB
#[test]
fn settings_sync_saves_max_mail_mb_with_clamp() {
    let (state, dir) = temp_state("max-mail");
    let save = |value: &str| {
        let _ = route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/sync/save",
                &form(&[
                    ("sync_on_start", "true"),
                    ("sync_interval_min", "30"),
                    ("first_fetch_days", "30"),
                    ("max_mail_mb", value),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
    };
    let d = defaults();
    save("80");
    assert_eq!(state.get_or(keys::MAX_MAIL_MB, &d).as_deref(), Some("80"));
    save("0");
    assert_eq!(
        state.get_or(keys::MAX_MAIL_MB, &d).as_deref(),
        Some("1"),
        "0 会令所有邮件都成占位，应钳到下限 1"
    );
    save("9999");
    assert_eq!(
        state.get_or(keys::MAX_MAIL_MB, &d).as_deref(),
        Some("200"),
        "上限 200 防止内存失控"
    );
    // 页面回显
    let pane = route(
        &state,
        &assets(),
        &req("GET", "/settings/panes/sync", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(body(&pane).contains("name=\"max_mail_mb\""));
    cleanup(&dir);
}

#[test]
fn forward_prefills_headers_body_and_attachment_hint() {
    let (state, dir) = temp_state("m6-forward");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    cc_addrs, subject, body_text, subject_tokens, body_tokens,
                                    unread, created_at, received_at)
                 VALUES ('in', 'inbox', 'received', 'boss@x.cn', '部门领导',
                         'demo@chinatelecom.cn', 'cc1@x.cn', '季度激励清单',
                         '请核对后回复。', '季度 激励 清单', '请 核对 后 回复', 1, 1, 1)",
            [],
        )
        .unwrap();
    });
    // 附件：用真实的附件入库路径，保证 attach_cnt 与模板提示一致
    let f = dir.join("清单.xlsx");
    std::fs::write(&f, b"xlsx").unwrap();
    mail::add_attachments(&state, 1, &[f.to_str().unwrap().into()]).unwrap();

    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "forward_from=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("转发邮件"), "应进入转发态标题");
    assert!(html.contains("Fwd: 季度激励清单"), "主题应加 Fwd: 前缀");
    assert!(
        html.contains("---------- 转发的邮件 ----------"),
        "正文应含转发头"
    );
    assert!(
        html.contains("部门领导 &lt;boss@x.cn&gt;"),
        "应保留原发件人显示名"
    );
    assert!(html.contains("抄送：cc1@x.cn"), "有抄送时应出现在转发头");
    assert!(html.contains("请核对后回复。"), "应引用原文");
    assert!(
        html.contains("name=\"forward_from\" value=\"1\""),
        "转发来源应写入隐藏域"
    );
    assert!(
        html.contains("将携带原邮件 1 个附件"),
        "应提示保存后携带附件：{html}"
    );
    // 详情页入口：转发按钮指向带来源 id 的撰写页
    let detail = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        detail.contains("href=\"/compose?forward_from=1&folder=inbox\""),
        "详情页应有转发入口（收件箱 context 带 folder）"
    );
    cleanup(&dir);
}

#[test]
fn forward_save_copies_attachments_only_on_first_save() {
    let (state, dir) = temp_state("m6-forward-save");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in', 'inbox', 'received', 'boss@x.cn', '领导', 'demo@chinatelecom.cn',
                         '带附件来件', '正文', '带 附件 来件', '正文', 1, 1, 1)",
            [],
        )
        .unwrap();
    });
    for name in ["a.txt", "b.txt"] {
        let p = dir.join(name);
        std::fs::write(&p, name.as_bytes()).unwrap();
        mail::add_attachments(&state, 1, &[p.to_str().unwrap().into()]).unwrap();
    }

    // 首次保存：forward_from 触发附件复制
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("forward_from", "1"),
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("subject", "Fwd: 带附件来件"),
                ("body_text", "转给你"),
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
            .any(|(k, v)| k == "HX-Redirect" && v.starts_with("/compose?draft_id=")),
        "存草稿应跳转编辑页：{:?}",
        resp.extra_headers
    );
    let draft_id: i64 = state.with_conn(|c| {
        c.query_row("SELECT id FROM emails WHERE folder = 'draft'", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    let d = mail::get(&state, draft_id).unwrap();
    assert_eq!(d.attachments.len(), 2, "首次保存应复制原邮件全部附件");

    // 二次保存（带 draft_id）：页面回显的 keep_ids 会列出已复制附件（真实前端行为），
    // 服务端按 keep 保留、且不再复制源附件
    let keep: Vec<i64> = d.attachments.iter().map(|a| a.id).collect();
    let keep_json = serde_json::to_string(&keep).unwrap();
    route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("draft_id", &draft_id.to_string()),
                ("forward_from", "1"),
                ("to_addrs", "peer@x.cn"),
                ("cc_addrs", ""),
                ("subject", "Fwd: 带附件来件"),
                ("body_text", "转给你（改）"),
                ("keep_ids", keep_json.as_str()),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let d = mail::get(&state, draft_id).unwrap();
    assert_eq!(d.attachments.len(), 2, "重存不得重复复制附件");
    cleanup(&dir);
}

#[test]
fn contacts_suggest_returns_json_for_name_and_addr() {
    let (state, dir) = temp_state("m6-contacts");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            mail::contacts::upsert_conn(c, "boss@x.cn", "部门领导", 100).unwrap();
            mail::contacts::upsert_conn(c, "peer@x.cn", "李四", 90).unwrap();
            Ok::<(), String>(())
        })
        .unwrap();

    let resp = route(
        &state,
        &assets(),
        &req_q("GET", "/contacts/suggest", "q=%E9%83%A8%E9%97%A8", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let json = body(&resp);
    assert!(json.contains("\"boss@x.cn\""), "按姓名应命中地址：{json}");
    assert!(json.contains("\"部门领导\""), "应返回姓名：{json}");
    assert!(!json.contains("peer@x.cn"), "不相关联系人不应出现");

    // 未配置：空数组（前端无需特判）
    let (state2, dir2) = temp_state("m6-contacts-empty");
    let json = body(&route(
        &state2,
        &assets(),
        &req_q("GET", "/contacts/suggest", "q=a", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert_eq!(json.trim(), "[]");
    cleanup(&dir);
    cleanup(&dir2);
}

#[test]
fn fetch_status_marks_running_and_phase_for_button_disable() {
    let (state, dir) = temp_state("m6-status");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    // 发件箱阶段：data-running=true + 阶段文案 + 2s 快轮询
    {
        let mut s = state.fetch_state.lock().unwrap();
        s.running = true;
        s.phase = "sent".into();
        s.started_at = mail::now_epoch();
        s.fetch_done = 3;
        s.fetch_total = 10;
    }
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/fetch/status", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("data-running=\"true\""),
        "运行中应下发禁用标记：{html}"
    );
    assert!(html.contains("正在同步发件箱"), "发件箱阶段文案");
    assert!(html.contains("（3/10）"), "应展示进度");
    assert!(html.contains("every 2s"), "运行中 2s 轮询");

    // 收件箱阶段文案
    {
        let mut s = state.fetch_state.lock().unwrap();
        s.phase = "inbox".into();
    }
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/fetch/status", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("正在收取新邮件"), "收件箱阶段文案");

    // 空闲：不再禁用，10s 轮询
    {
        let mut s = state.fetch_state.lock().unwrap();
        s.running = false;
        s.phase.clear();
        s.last_error.clear();
    }
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/fetch/status", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("data-running=\"false\""), "空闲应解除禁用");
    assert!(html.contains("every 10s"), "空闲 10s 轮询");
    cleanup(&dir);
}

#[test]
fn wizard_save_triggers_first_fetch_unless_migration_pending() {
    // 需求：向导保存配置后立即触发首拉（先发件箱后收件箱），不必等定时周期。
    // 白盒测法：占满并发轮名额，start_fetch 走同步拒绝分支并写入确定错误，
    // 从而判断"是否被触发"，避免真实网络线程让断言不确定。
    let (state, dir) = temp_state("wiz-first-fetch");
    state.active_rounds.store(
        state::MAX_CONCURRENT_FETCH_ROUNDS,
        std::sync::atomic::Ordering::SeqCst,
    );
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
        "向导保存应成功"
    );
    let err = state
        .fetch_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_error
        .clone();
    assert!(
        err.contains(&format!("已有 {} 轮", state::MAX_CONCURRENT_FETCH_ROUNDS)),
        "保存成功应触发首拉（名额占满时留下明确拒绝原因）：{err}"
    );
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);

    // 库非空（历史数据）+ 未配置 + 填了不同目录 → R2（复查审计）：向导不再
    // 同步迁移（协议线程里 VACUUM + 全量附件复制会卡死「保存中…」，绕过
    // 设置页的后台迁移链），改为返回引导文案；账号已保存、指针未动、
    // 目标目录零产物、不触发首拉（未配置完成，不写旧目录）
    // 注意 to_addrs 必须是登录账号本身：归属校验（first_run && !fresh）
    // 会先于目录分支执行——旧版素材写 demo@ 导致请求被归属拒绝，
    // 旧断言 err.is_empty() 在被拒时同样成立（假通过），迁移路径从未真覆盖
    let (state, dir) = temp_state("wiz-first-fetch-pending");
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in','inbox','received','old@x.cn','unit.router@chinatelecom.cn',
                         '历史邮件','旧正文','历史 邮件','旧 正文',0,1,1)",
            [],
        )
        .unwrap();
    });
    assert!(!state.store_is_empty(), "前置：库非空才走引导分支");
    let target = std::env::temp_dir().join(format!("mbx-wiz-first-fetch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    let mut pairs = wizard_pairs(dir.to_str().unwrap());
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
    let text = body(&resp);
    assert!(
        resp.extra_headers
            .iter()
            .all(|(k, _)| k != "HX-Redirect"),
        "引导分支不得跳转完成页"
    );
    assert!(text.contains("存储管理"), "应引导去设置页发起迁移：{text}");
    assert!(
        text.contains("清空数据目录"),
        "应说明可清空目录重提完成向导：{text}"
    );
    assert!(state.configured(), "账号配置已保存（C6 半保存语义）");
    assert_eq!(state.data_dir(), dir, "数据目录指针不得在向导内推进");
    assert!(
        !target.join("corpmail.db").exists(),
        "向导内不得执行迁移（目标零产物）"
    );
    assert!(state.pending_data_dir().is_none(), "无待重启目录");
    assert!(!state.migration_pending(), "未进入冻结期");
    let err = state
        .fetch_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_error
        .clone();
    assert!(
        err.is_empty(),
        "引导分支停在向导页，不应触发首拉写旧目录：{err}"
    );
    state.clear_password("unit.router@chinatelecom.cn");
    let _ = std::fs::remove_dir_all(&target);
    cleanup(&dir);
}

#[test]
fn detail_back_link_preserves_list_filters() {
    let (state, dir) = temp_state("m7-back");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','领导','demo@chinatelecom.cn',
                         '激励清单','正文','激励 清单','正文',1,1,1)",
                [],
            )
            .unwrap();
            // 已发送一封，用于 folder=sent 的往返
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, sent_at)
                 VALUES ('out','sent','sent','demo@chinatelecom.cn','张伟','peer@x.cn',
                         '已发送件','正文','已 发送 件','正文',0,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();

    // 列表侧：行链接携带当前过滤状态（未读 + 搜索词）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "unread=1&q=%E6%BF%80%E5%8A%B1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("/emails/1?unread=1&amp;q=%E6%BF%80%E5%8A%B1"),
        "列表行链接应携带过滤参数：{html}"
    );

    // 详情侧：进入时带上完整过滤状态 → 返回链接原样还原
    let html = body(&route(
        &state,
        &assets(),
        &req_q(
            "GET",
            "/emails/1",
            "folder=inbox&unread=1&external=1&q=%E6%BF%80%E5%8A%B1&page=3",
            b"",
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains(
            r#"href="&#x2f;?folder=inbox&amp;unread=1&amp;external=1&amp;q=%E6%BF%80%E5%8A%B1&amp;page=3"#
        ),
        "返回列表应原样还原过滤状态（含 folder=inbox）：{html}"
    );

    // 旧会话视图书签携带 mode=thread：详情页忽略 mode，返回链接不再携带
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/emails/1", "folder=inbox&mode=thread", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        !html.contains("mode=thread"),
        "mode=thread 已废弃，返回链接不应再携带：{html}"
    );

    // 无参数进入：回落到邮件所在 folder（sent → /?folder=sent）
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/2", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains(r#"href="&#x2f;?folder=sent""#),
        "无参数时按邮件 folder 生成返回链接：{html}"
    );

    // 收件箱默认视图：无过滤 → 返回链接显式 folder=inbox（不再省略）
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
        html.contains(r#"href="&#x2f;?folder=inbox""#),
        "收件箱默认视图返回链接应显式带 folder=inbox：{html}"
    );
    cleanup(&dir);
}

#[test]
fn reply_all_prefills_all_recipients_excluding_self() {
    let (state, dir) = temp_state("m7-reply-all");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
            (keys::DISPLAY_NAME, "张伟"),
        ])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    cc_addrs, subject, body_text, subject_tokens, body_tokens,
                                    unread, created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','部门领导',
                         'demo@chinatelecom.cn, peer@x.cn', 'ops@x.cn',
                         '激励清单','请核对。','激励 清单','请 核对',1,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();

    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "reply_all_to=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("回复全部"), "标题应为回复全部：{html}");
    assert!(html.contains("Re: 激励清单"), "主题应加 Re: 前缀");
    // 收件人 = 原发件人 + 原收件人，去掉自己（demo@chinatelecom.cn）；
    // 抄送 = 原抄送（ops@x.cn），不再塞进收件人（M9 拆分）
    let to_input = html
        .split("name=\"to_addrs\"")
        .nth(1)
        .and_then(|s| s.split("value=\"").nth(1))
        .and_then(|s| s.split('"').next())
        .unwrap_or_default();
    let cc_input = html
        .split("name=\"cc_addrs\"")
        .nth(1)
        .and_then(|s| s.split("value=\"").nth(1))
        .and_then(|s| s.split('"').next())
        .unwrap_or_default();
    assert!(
        to_input.contains("部门领导 &lt;boss@x.cn&gt;"),
        "收件人应含原发件人：{to_input}"
    );
    assert!(
        to_input.contains("peer@x.cn"),
        "收件人应含原收件人：{to_input}"
    );
    assert!(
        !to_input.to_ascii_lowercase().contains("ops@x.cn"),
        "原抄送不应再进收件人：{to_input}"
    );
    assert!(
        !to_input
            .to_ascii_lowercase()
            .contains("demo@chinatelecom.cn"),
        "不得把自己列为收件人：{to_input}"
    );
    assert!(
        cc_input.contains("ops@x.cn"),
        "抄送应复制原抄送：{cc_input}"
    );
    assert!(
        !cc_input
            .to_ascii_lowercase()
            .contains("demo@chinatelecom.com"),
        "不得把自己列为抄送：{cc_input}"
    );
    assert!(
        html.contains("name=\"in_reply_to\" value=\"1\""),
        "回复链应写入隐藏域"
    );
    assert!(
        !html.contains("name=\"forward_from\" value=\"1\""),
        "不应误入转发态"
    );
    cleanup(&dir);
}

/// M9：回复（非全部）也把原邮件的抄送人复制到新邮件（去自己、去重）
#[test]
fn reply_copies_original_cc() {
    let (state, dir) = temp_state("m9-reply-cc");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
            (keys::DISPLAY_NAME, "张伟"),
        ])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    cc_addrs, subject, body_text, subject_tokens, body_tokens,
                                    unread, created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','部门领导',
                         'demo@chinatelecom.cn, peer@x.cn',
                         'cc1@x.cn, demo@chinatelecom.cn, cc1@x.cn',
                         '主题','正文','主题','正文',1,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "reply_to=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("回复邮件"), "回复态标题：{html}");
    // 收件人 = 原发件人（地址；display name 不用于收件人回填）
    assert!(html.contains("boss@x.cn"), "收件人应预填原发件人");
    // 抄送 = 原抄送（去自己 demo、去重 cc1）
    let cc_input = html
        .split("name=\"cc_addrs\"")
        .nth(1)
        .and_then(|s| s.split("value=\"").nth(1))
        .and_then(|s| s.split('"').next())
        .unwrap_or_default();
    assert!(cc_input.contains("cc1@x.cn"), "应复制原抄送：{cc_input}");
    assert!(
        !cc_input
            .to_ascii_lowercase()
            .contains("demo@chinatelecom.cn"),
        "不得把自己列为抄送：{cc_input}"
    );
    assert_eq!(
        cc_input.matches("cc1@x.cn").count(),
        1,
        "原抄送重复应去重：{cc_input}"
    );
    cleanup(&dir);
}

/// M9：发件箱「重新编辑」——载入已发送邮件正文/主题/收件人/抄送（原样，非转发引文），
/// 首次保存复制原附件
#[test]
fn edit_from_prefills_sent_content() {
    let (state, dir) = temp_state("m9-edit-from");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
            (keys::DISPLAY_NAME, "张伟"),
        ])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    cc_addrs, subject, body_text, body_html, subject_tokens,
                                    body_tokens, unread, created_at, sent_at)
                 VALUES ('out','sent','sent','demo@chinatelecom.cn','张伟',
                         'peer@x.cn','cc1@x.cn','已发送主题','正文',
                         '<div style=\"font-family:sans-serif\"><h2>配案</h2><table><tr><td>表</td></tr></table></div>',
                         '已 发送 主题','正文',0,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 一个附件：重新编辑首次保存应复制
    let p = dir.join("a.txt");
    std::fs::write(&p, "x").unwrap();
    mail::add_attachments(&state, 1, &[p.to_str().unwrap().into()]).unwrap();

    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "edit_from=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("name=\"to_addrs\" value=\"peer@x.cn\""),
        "收件人应原样回填：{html}"
    );
    assert!(
        html.contains("name=\"cc_addrs\" value=\"cc1@x.cn\""),
        "抄送应原样回填"
    );
    assert!(
        html.contains("name=\"subject\" value=\"已发送主题\""),
        "主题应原样回填"
    );
    assert!(
        html.contains(r#"<input type="hidden" name="body_mode" id="body-mode" value="html">"#),
        "HTML 原件重新编辑应为 html 模式"
    );
    assert!(
        html.contains("name=\"forward_from\" value=\"1\""),
        "重新编辑应写入 forward_from（首次保存复制附件）"
    );
    assert!(
        html.contains("将携带原邮件 1 个附件"),
        "应提示保存后携带原附件"
    );
    assert!(
        !html.contains("---------- 转发的邮件 ----------"),
        "重新编辑不应是转发引文"
    );
    assert!(
        html.contains("&lt;table&gt;"),
        "正文应原样保留（表格透传，非 blockquote 包裹）：{}",
        html.lines()
            .find(|l| l.contains("rt-initial"))
            .unwrap_or("(未找到 rt-initial)")
    );
    cleanup(&dir);
}

#[test]
fn detail_actions_render_at_top_with_reply_all() {
    let (state, dir) = temp_state("m7-actions-top");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','领导','demo@chinatelecom.cn',
                         '主题','正文','主题','正文',1,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    // 按钮区在头部之前（置顶），且仅有这一处操作区
    let pos_actions = html
        .find("md-actions md-actions-top")
        .expect("应有置顶按钮区");
    let pos_head = html.find("md-head").expect("应有邮件头");
    assert!(pos_actions < pos_head, "按钮区应在邮件头上方");
    assert_eq!(
        html.matches("md-actions-top").count(),
        1,
        "应有且仅有一处置顶按钮区"
    );
    assert!(!html.contains("\"md-actions\""), "底部不应再残留旧按钮区");
    assert!(html.contains("/compose?reply_to=1"), "应有回复");
    assert!(html.contains("/compose?reply_all_to=1"), "应有回复全部");
    assert!(html.contains("/compose?forward_from=1"), "应有转发");
    assert!(html.contains("删除这封邮件"), "应有删除");
    // 草稿/发件方向不显示回复入口
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, sent_at)
                 VALUES ('out','sent','sent','demo@chinatelecom.cn','张伟','peer@x.cn',
                         '出件','正文','出 件','正文',0,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/2", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        !html.contains("reply_all_to") && !html.contains("reply_to="),
        "非收件方向不应有回复入口：{html}"
    );
    // M9 发件箱：重新编辑（载入撰写页重发），不显示转发
    assert!(
        html.contains("/compose?edit_from=2"),
        "发件箱应有重新编辑入口：{html}"
    );
    assert!(html.contains("重新编辑"), "发件箱按钮文案应为重新编辑");
    assert!(
        !html.contains("forward_from=2"),
        "发件箱不应有转发入口：{html}"
    );
    // M9 草稿箱：编辑草稿，不显示转发
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, sent_at)
                 VALUES ('out','draft','draft','demo@chinatelecom.cn','张伟','peer@x.cn',
                         '草稿','正文','草 稿','正文',0,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/emails/3", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("/compose?draft_id=3"),
        "草稿箱应有编辑入口：{html}"
    );
    assert!(html.contains(" 编辑</a>"), "草稿箱按钮应为编辑：{html}");
    assert!(
        !html.contains("forward_from=3"),
        "草稿箱不应有转发入口：{html}"
    );
    cleanup(&dir);
}

#[test]
fn side_frame_returns_bare_sidebar_fragment() {
    let (state, dir) = temp_state("m7-side-frame");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::DISPLAY_NAME, "张伟"),
            (keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
        ])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs,
                                    subject, body_text, subject_tokens, body_tokens, unread,
                                    created_at, received_at)
                 VALUES ('in','inbox','received','boss@x.cn','demo@chinatelecom.cn',
                         '主题','正文','主题','正文',1,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let frag = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/side/frame", "folder=sent", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(frag.contains("<aside"), "应为侧栏片段：{frag}");
    assert!(!frag.contains("<!doctype"), "不应带整页布局");
    assert!(frag.contains("fetch-status"), "侧栏含收件状态条");
    // 计数与高亮：收件箱 1 / 未读 1；当前 folder=sent 高亮（2026-11-04 起
    // 文件夹名套 .fw 胶囊、列表增加设置行，断言按新结构）
    assert!(
        frag.contains("收件箱") && frag.contains("<span class=\"cnt\">1"),
        "计数应刷新：{frag}"
    );
    assert!(frag.contains("发件箱"), "应有发件箱入口");
    assert!(
        frag.contains(r#"class="folder on" href="/?folder=sent""#),
        "folder 参数应驱动高亮：{frag}"
    );
    cleanup(&dir);
}

#[test]
fn sync_pane_refetch_entry_hidden_until_range_changes() {
    // 需求：「重新拉取」仅在首次拉取范围有变化时显示——服务端下发已存范围
    // （data-first-fetch-initial）与隐藏态按钮，前端比对后显隐（app.js）。
    // 本测试锁定服务端契约：按钮默认隐藏、初始值随保存结果刷新。
    let (state, dir) = temp_state("sync-refetch-ui");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
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
    assert!(
        html.contains("id=\"refetch-row\"") && html.contains("hidden"),
        "重新拉取入口应默认隐藏：{html}"
    );
    assert!(
        html.contains("data-first-fetch-initial=\"0\""),
        "应携带已保存范围作比对基准（默认 0 = 全部）：{html}"
    );

    // 保存新范围 → 返回片段的基准同步为新值，按钮保持隐藏（无变化）
    let html = body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[
                ("sync_on_start", "true"),
                ("sync_interval_min", "30"),
                ("first_fetch_days", "90"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("已保存") && html.contains("class=\"ic\""));
    assert!(
        html.contains("data-first-fetch-initial=\"90\""),
        "保存后基准应更新为 90：{html}"
    );
    assert!(
        html.contains("id=\"refetch-row\"") && html.contains("hidden"),
        "保存返回片段仍应隐藏重拉入口：{html}"
    );
    cleanup(&dir);
}

/// 收件 ETA 展示门槛（后台更新时间预估）：仅 running、有计时、跨度≥3s、
/// 有增量吞吐、且预估剩余 >60s 才出文案；分钟/小时档位格式正确
#[test]
fn fetch_eta_label_gates_and_formats() {
    use crate::state::FetchState;
    let base = |running: bool, done: usize, total: usize, sa: i64, ua: i64, sd: usize| FetchState {
        running,
        new_count: 0,
        last_error: String::new(),
        just_finished: false,
        epoch: 1,
        started_at: 0,
        fetch_done: done,
        fetch_total: total,
        phase: "inbox".into(),
        progress_started_at: sa,
        progress_updated_at: ua,
        progress_started_done: sd,
    };
    let now = mail::now_epoch();
    // 非进行中：不显示
    assert_eq!(eta_label(&base(false, 10, 100, now - 10, now, 0)), "");
    // 无剩余：不显示
    assert_eq!(eta_label(&base(true, 100, 100, now - 10, now, 0)), "");
    // 未起表（progress_started_at=0，首拍下载中）：不显示
    assert_eq!(eta_label(&base(true, 0, 100, 0, 0, 0)), "");
    // 计时跨度 < 3s：首拍吞吐不可信，不显示
    assert_eq!(eta_label(&base(true, 5, 100, now, now, 0)), "");
    // 零增量（started_done==done，计时内无进展）：不显示
    assert_eq!(eta_label(&base(true, 10, 1000, now - 30, now, 10)), "");
    // 预估 ≤60s：不显示；36s 拉 40 封（吞吐≈1.1封/s）剩 60 封 → ≈54s
    assert_eq!(eta_label(&base(true, 100, 160, now - 36, now, 60)), "");
    // 预估恰 72s（>60s 显示）→ "约 1 分钟"；36s 拉 40 封、剩 80 → 80/40*36 = 72s
    assert_eq!(
        eta_label(&base(true, 80, 160, now - 36, now, 40)),
        "约 1 分钟"
    );
    // 60s 拉 100 封剩 900 → 540s → "约 9 分钟"
    assert_eq!(
        eta_label(&base(true, 100, 1000, now - 60, now, 0)),
        "约 9 分钟"
    );
    // 3600s 拉 100 封剩 1100 → 约 39600s → 11 小时档（≥10h 不带余分钟）
    assert_eq!(
        eta_label(&base(true, 100, 1200, now - 3600, now, 0)),
        "约 11 小时"
    );
    // 90 分钟档：3600s 拉 100 封剩 250 → 9000s = 150 分钟 → 约 2 小时 30 分钟
    assert_eq!(
        eta_label(&base(true, 100, 350, now - 3600, now, 0)),
        "约 2 小时 30 分钟"
    );
}

/// 批量操作（M1-1）：页内精确模式——标已读/未读/删除 + 响应含边栏刷新事件
#[test]
fn batch_actions_ids_mode() {
    let (state, dir) = temp_state("batch-ids");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let mut ids = Vec::new();
    state
        .with_conn(|c| {
            for i in 0..5 {
                c.execute(
                    "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                        subject_tokens, body_tokens, unread, created_at, received_at)
                     VALUES ('in','inbox','received','a@x.cn','b@x.cn',?1,?1,'x',1,?2,?2)",
                    rusqlite::params![i.to_string(), 100 + i],
                )
                .unwrap();
                ids.push(c.last_insert_rowid());
            }
            Ok::<(), String>(())
        })
        .unwrap();
    // 标已读（3 封：第 3、4、5 封 = ids[2..5]）
    let ids_read = format!("[{},{},{}]", ids[2], ids[3], ids[4]);
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[
                ("action", "read"),
                ("ids", ids_read.as_str()),
                ("folder", "inbox"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 200);
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Trigger" && v.contains("mbx:fetchDone")),
        "批量操作须附带边栏刷新事件"
    );
    let unread_cnt: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails WHERE unread = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert_eq!(unread_cnt, 2, "3 封应置已读");
    // 删除（2 封，其中 ids[0] 含独立文件附件）——附件文件应一并清理。
    // 先把 inline 上限设为 1MB 并写入 1.5MB 文件，确保附件落 file 存储
    state
        .set_batch(&[(keys::INLINE_ATTACH_MAX_MB, "1")])
        .unwrap();
    let big: Vec<u8> = vec![7u8; 1536 * 1024];
    let f = dir.join("att.bin");
    std::fs::write(&f, &big).unwrap();
    mail::add_attachments(&state, ids[0], &[f.to_str().unwrap().into()]).unwrap();
    let file_rel: String = state.with_conn(|c| {
        c.query_row(
            "SELECT COALESCE(file_path,'') FROM attachments WHERE email_id = ?1 AND storage = 'file'",
            [ids[0]],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert!(!file_rel.is_empty(), "前置：附件应落 file 存储");
    assert!(
        state
            .data_dir()
            .join("attachments")
            .join(&file_rel)
            .exists(),
        "前置：附件文件已落盘"
    );
    let ids_del = format!("[{},{}]", ids[0], ids[1]);
    let responses = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[
                ("action", "delete"),
                ("ids", ids_del.as_str()),
                ("folder", "inbox"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(responses.status, 200);
    let total: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(total, 3, "2 封已删");
    assert!(
        !state
            .data_dir()
            .join("attachments")
            .join(&file_rel)
            .exists(),
        "删除邮件的附件文件应清理"
    );
    let fts: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM email_fts JOIN emails e ON e.id = email_fts.rowid",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(fts, 3, "FTS 行随批量删除同步");
    // 空集合：无操作提示
    let empty = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[("action", "read"), ("ids", "[]"), ("folder", "inbox")]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let b = body(&empty);
    assert!(
        b.contains("toast") && b.contains("未选中"),
        "空集合应提示未选中：{b}"
    );
    cleanup(&dir);
}

/// 批量操作：跨页全选模式——select_all + exclude_ids + 筛选条件同口径
#[test]
fn batch_actions_select_all_mode() {
    let (state, dir) = temp_state("batch-all");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let now = mail::now_epoch();
    state
        .with_conn(|c| {
            for i in 0..100 {
                c.execute(
                    "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                        subject_tokens, body_tokens, unread, created_at, received_at)
                     VALUES ('in','inbox','received','a@chinatelecom.cn','b@x.cn',?1,?1,'x',?2,?3,?3)",
                    rusqlite::params![i.to_string(), if i < 90 { 1 } else { 0 }, now],
                )
                .unwrap();
            }
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, unread, created_at, sent_at)
                 VALUES ('out','sent','sent','demo@chinatelecom.cn','b@x.cn','s','s','x',1,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    // 跨页标已读：未读筛选 90 封命中，排除前 3 → 87 置已读
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[
                ("action", "read"),
                ("select_all", "1"),
                ("exclude_ids", "[1,2,3]"),
                ("folder", "inbox"),
                ("unread", "1"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(resp.status, 200);
    let unread_cnt: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM emails WHERE unread = 1 AND folder = 'inbox'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(unread_cnt, 3, "90 未读 - 3 排除 = 87 置已读，剩 3 封排除项");
    // 跨页删除：inbox 全删（含已读），sent 不受牵连
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[
                ("action", "delete"),
                ("select_all", "1"),
                ("folder", "inbox"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let in_cnt: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM emails WHERE folder = 'inbox'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    let sent_cnt: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM emails WHERE folder = 'sent'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(in_cnt, 0, "inbox 全删");
    assert_eq!(sent_cnt, 1, "其他文件夹不受牵连");
    cleanup(&dir);
}

/// 批量操作防护：未知 action / 畸形 folder 白名单回落 / 检索词跨页全选拒绝
#[test]
fn batch_actions_guards() {
    let (state, dir) = temp_state("batch-guard");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, unread, created_at, received_at)
                 VALUES ('in','inbox','received','a@x.cn','b@x.cn','t','t','x',1,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let r = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[("action", "purge"), ("ids", "[1]"), ("folder", "inbox")]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(body(&r).contains("未知操作"));
    let r = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[
                ("action", "read"),
                ("select_all", "1"),
                ("folder", "inbox' OR 1=1 --"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(r.status, 200, "畸形 folder 应被白名单回落而非错误");
    let r = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[
                ("action", "read"),
                ("select_all", "1"),
                ("folder", "inbox"),
                ("q", "合同"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let b = body(&r);
    assert!(b.contains("暂不支持跨页全选"), "检索态应拒绝跨页全选：{b}");
    cleanup(&dir);
}

/// 首拉范围变化即清双箱游标（2026-11-03 Bug 修复）：扩大范围后收件箱
/// 增量游标挡住低 UID 的历史邮件，表现为"收件箱不更新、手动收取无新邮件"。
/// 保存时检测范围变化 → 清 INBOX+SENT 四游标（首拉分支全量重拉 + 去重回填）
#[test]
fn sync_save_range_change_resets_both_cursors() {
    let (state, dir) = temp_state("sync-range");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::FIRST_FETCH_DAYS, "30"),
            (keys::LAST_UID, "9503"),
            (keys::LAST_UIDVALIDITY, "7"),
            (keys::LAST_UID_SENT, "1200"),
            (keys::LAST_UIDVALIDITY_SENT, "7"),
        ])
        .unwrap();
    // 范围 30 → 90：四游标清零
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[
                ("sync_on_start", "true"),
                ("sync_interval_min", "15"),
                ("first_fetch_days", "90"),
                ("max_mail_mb", "20"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    let d = defaults();
    for k in [
        keys::LAST_UID,
        keys::LAST_UIDVALIDITY,
        keys::LAST_UID_SENT,
        keys::LAST_UIDVALIDITY_SENT,
    ] {
        assert_eq!(
            state.get_or(k, &d).as_deref(),
            Some("0"),
            "范围变化应清游标 {k}"
        );
    }
    assert_eq!(
        state.get_or(keys::FIRST_FETCH_DAYS, &d).as_deref(),
        Some("90"),
        "新范围已保存"
    );
    // 范围不变：游标不动（不触发无谓全量重拉）
    state
        .set_batch(&[(keys::LAST_UID, "5"), (keys::LAST_UIDVALIDITY, "7")])
        .unwrap();
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[
                ("sync_on_start", "true"),
                ("sync_interval_min", "15"),
                ("first_fetch_days", "90"),
                ("max_mail_mb", "20"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert_eq!(
        state.get_or(keys::LAST_UID, &d).as_deref(),
        Some("5"),
        "范围未变不得清游标"
    );
    // 重新拉取：同样清双箱游标（修复了此前漏清发件箱）
    state
        .set_batch(&[(keys::LAST_UID_SENT, "88"), (keys::FIRST_FETCH_DAYS, "7")])
        .unwrap();
    let _ = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/refetch",
            &form(&[("first_fetch_days", "7")]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    for k in [
        keys::LAST_UID,
        keys::LAST_UIDVALIDITY,
        keys::LAST_UID_SENT,
        keys::LAST_UIDVALIDITY_SENT,
    ] {
        assert_eq!(
            state.get_or(k, &d).as_deref(),
            Some("0"),
            "重新拉取应清双箱游标 {k}"
        );
    }
    cleanup(&dir);
}

// ---------- M3-1 草稿自动保存 ----------

/// autosave 契约：空表单不落库（ok:true, draft_id:0）；有内容创建草稿并回传
/// draft_id；带 draft_id 再次 autosave 变覆盖（不新增行）
#[test]
fn compose_autosave_silent_and_id_reuse() {
    let (state, dir) = temp_state("m3-autosave");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    // 空表单：不生成空草稿（打开撰写页几秒即自动保存的场景）
    let resp = body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/autosave",
            &form(&[
                ("to_addrs", ""),
                ("cc_addrs", ""),
                ("bcc_addrs", ""),
                ("subject", ""),
                ("body_text", ""),
                ("body_mode", "plain"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(resp.contains("\"ok\":true"), "空表单应返回 ok：{resp}");
    assert!(
        resp.contains("\"draft_id\":0"),
        "空表单不落库（draft_id=0）：{resp}"
    );
    let drafts: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(drafts, 0, "空表单不应生成空草稿");
    // 有内容：创建草稿，返回 draft_id
    let resp = body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/autosave",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("subject", "自动保存主题"),
                ("body_text", "写了一半的正文"),
                ("body_mode", "plain"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        resp.contains("\"draft_id\":") && !resp.contains("\"draft_id\":0"),
        "有内容应落库并回传 draft_id：{resp}"
    );
    // 再次 autosave（带 draft_id=1，改主题）→ 覆盖不新增
    let resp = body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/autosave",
            &form(&[
                ("draft_id", "1"),
                ("to_addrs", "peer@x.cn"),
                ("subject", "改后的主题"),
                ("body_text", "写完的正文"),
                ("body_mode", "plain"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(resp.contains("\"draft_id\":1"), "覆盖保存回传原 id：{resp}");
    let (cnt, subj): (i64, String) = state.with_conn(|c| {
        c.query_row("SELECT count(*), MAX(subject) FROM emails", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    });
    assert_eq!(cnt, 1, "重复自动保存应覆盖同一条草稿");
    assert_eq!(subj, "改后的主题");
    cleanup(&dir);
}

// ---------- M3-2 签名档 ----------

/// 新邮件预填签名；编辑已有草稿不重复插入
#[test]
fn compose_new_mail_prefills_signature_once() {
    let (state, dir) = temp_state("m3-signature");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::DISPLAY_NAME, "张伟"),
            (keys::SIGNATURE, "张伟\n政企客户部"),
        ])
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
    assert!(html.contains("政企客户部"), "新邮件应预填签名：{html}");
    // 存草稿后再进编辑页：正文回显草稿原文，签名不重复（仍只出现一次）
    body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("subject", "带签名草稿"),
                ("body_text", "\n\n--\n张伟\n政企客户部"),
                ("body_mode", "plain"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "draft_id=1", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert_eq!(
        html.matches("政企客户部").count(),
        1,
        "编辑草稿回显原文，不再插入第二份签名"
    );
    cleanup(&dir);
}

/// 设置·账号保存签名档（多行），页面回显
#[test]
fn settings_account_save_persists_signature() {
    let (state, dir) = temp_state("m3-sig-save");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/account/save",
            &form(&[("display_name", "张伟"), ("signature", "张伟\r\n电话 123")]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("已保存") && html.contains("class=\"ic\""),
        "保存应成功：{html}"
    );
    assert!(html.contains("张伟\n电话 123"), "CRLF 归一为 LF 后回显");
    let d = defaults();
    let saved = state.get_or(keys::SIGNATURE, &d).unwrap_or_default();
    assert_eq!(saved, "张伟\n电话 123");
    cleanup(&dir);
}

// ---------- M3-3 联系人管理页 ----------

/// 联系人页：列表渲染 + 搜索过滤 + 删除（htmx 片段刷新当前过滤态）
#[test]
fn contacts_page_lists_searches_and_deletes() {
    let (state, dir) = temp_state("m3-contacts");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let now = mail::now_epoch();
    state
        .with_conn(|c| {
            c.execute_batch(&format!(
                "INSERT INTO contacts(addr, name, updated_at, hits) VALUES
                 ('boss@x.cn', '王副总', {a}, 5),
                 ('li@x.cn', '李静', {b}, 1)",
                a = now,
                b = now - 86400
            ))
            .map_err(|e| e.to_string())
        })
        .unwrap();
    // 整页：列表 + 入口
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/contacts", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("boss@x.cn"), "列表应含地址：{html}");
    assert!(html.contains("王副总"), "列表应含姓名");
    assert!(
        html.contains("联系人 <span class=\"sub\">2 位"),
        "计数 {html}"
    );
    assert!(html.contains("/contacts/delete"));
    // 搜索：只留王副总
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "q=%E7%8E%8B", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("王副总"));
    assert!(!html.contains("li@x.cn"), "搜索应过滤未命中联系人");
    // 删除（带 q 保过滤）：库删除 + 片段不再含该地址
    let frag = body(&route(
        &state,
        &assets(),
        &req(
            "POST",
            "/contacts/delete",
            &form(&[("addr", "boss@x.cn"), ("q", "王")]),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(!frag.contains("boss@x.cn"), "删除后列表应移除");
    let remain: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM contacts WHERE addr='boss@x.cn'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(remain, 0, "库中应物理删除");
    cleanup(&dir);
}

/// 联系人分页：默认每页 10、per 持久化、page 越界钳回；翻页链接带 q/per
#[test]
fn contacts_page_pagination_slices_and_defaults() {
    let (state, dir) = temp_state("m3-contacts-page");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let now = mail::now_epoch();
    // 3 条，updated_at 倒序（c0 最新 … c2 最旧）
    state
        .with_conn(|c| {
            c.execute_batch(&format!(
                "INSERT INTO contacts(addr, name, updated_at, hits) VALUES
                 ('c0@x.cn', '甲', {a}, 1),
                 ('c1@x.cn', '乙', {b}, 1),
                 ('c2@x.cn', '丙', {c}, 1)",
                a = now,
                b = now - 3600,
                c = now - 7200
            ))
            .map_err(|e| e.to_string())
        })
        .unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 默认每页 10：3 条全在一页，无翻页器，下拉 10 选中
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/contacts", b""),
        &tester,
    ));
    assert!(
        html.contains("c0@x.cn") && html.contains("c2@x.cn"),
        "默认应全量: {html}"
    );
    assert!(!html.contains("class=\"pager\""), "不足一页不应有翻页器");
    assert!(html.contains("每页 10 人"), "应提供每页下拉");
    // per=2&page=2：只含最旧 c2；翻页器 2 / 2，无下一页
    let html2 = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "per=2&page=2", b""),
        &tester,
    ));
    assert!(html2.contains("c2@x.cn"), "第二页应含最旧: {html2}");
    assert!(
        !html2.contains("c0@x.cn") && !html2.contains("c1@x.cn"),
        "第二页不应含前两页"
    );
    assert!(html2.contains(">2 / 2"), "页次应 2 / 2");
    assert!(html2.contains("上一页"), "第 2 页应有上一页");
    assert!(!html2.contains("下一页"), "末页不应有下一页");
    // 首页/末页：第 2 页（末页）有「首页」、无「末页」
    assert!(html2.contains("« 首页"), "非首页应有「首页」入口");
    assert!(!html2.contains("末页 »"), "最后一页不应出现「末页」");
    // page 越界钳回：page=99 → 仍 2 / 2
    let html3 = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "per=2&page=99", b""),
        &tester,
    ));
    assert!(html3.contains(">2 / 2"), "越界应钳回末页: {html3}");
    // per 持久化：/contacts?per=5 后 settings.PAGE_SIZE=5
    let _ = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "per=5", b""),
        &tester,
    ));
    let d = defaults();
    assert_eq!(
        state.get_or(keys::PAGE_SIZE, &d).unwrap_or_default(),
        "5",
        "per=5 应持久化"
    );
    cleanup(&dir);
}

/// 联系人排序：默认最近联系；sort=hits 联系最多、sort=name 姓名（SQLite
/// BINARY 序即 UTF-8 字节序，非拼音）；排序下拉与邮件列表同构，非默认进翻页链接
#[test]
fn contacts_page_sort_orders_and_preserves() {
    let (state, dir) = temp_state("m3-contacts-sort");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let now = mail::now_epoch();
    // c0 最新但联系最少；c1 最旧但联系最多；姓名字节序 李(c2)<王(c0)<陈(c1)
    state
        .with_conn(|c| {
            c.execute_batch(&format!(
                "INSERT INTO contacts(addr, name, updated_at, hits) VALUES
                 ('c0@x.cn', '王五', {a}, 1),
                 ('c1@x.cn', '陈一', {b}, 9),
                 ('c2@x.cn', '李三', {c}, 5),
                 ('c3@x.cn', '', {d}, 0)",
                a = now,
                b = now - 7200,
                c = now - 3600,
                d = now - 10800
            ))
            .map_err(|e| e.to_string())
        })
        .unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 默认：最近联系倒序（c0 → c2 → c1）+ 排序下拉选项齐全
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/contacts", b""),
        &tester,
    ));
    assert!(
        html.contains("最近联系") && html.contains("联系最多") && html.contains("姓名"),
        "排序下拉选项应齐全：{html}"
    );
    let (i0, i2, i1) = (
        html.find("c0@x.cn").unwrap(),
        html.find("c2@x.cn").unwrap(),
        html.find("c1@x.cn").unwrap(),
    );
    assert!(i0 < i2 && i2 < i1, "默认应按最近联系倒序");
    // sort=hits：联系次数降序（c1 → c2 → c0，默认每页全量）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "sort=hits", b""),
        &tester,
    ));
    let (h0, h1, h2) = (
        html.find("c0@x.cn").unwrap(),
        html.find("c1@x.cn").unwrap(),
        html.find("c2@x.cn").unwrap(),
    );
    assert!(h1 < h2 && h2 < h0, "sort=hits 应按联系次数降序");
    assert!(
        html.contains(r#"<option value="hits" selected"#),
        "hits 应选中：{html}"
    );
    // 翻页链接保留排序：per=2 第 1 页的「下一页」应带 sort=hits&page=2
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "sort=hits&per=2", b""),
        &tester,
    ));
    assert!(
        html.contains("sort=hits&page=2") && html.contains("下一页"),
        "翻页链接应保留排序：{html}"
    );
    // sort=name：姓名升序（字节序：李 → 王 → 陈）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "sort=name&per=10", b""),
        &tester,
    ));
    let (n0, n1, n2) = (
        html.find("c0@x.cn").unwrap(),
        html.find("c1@x.cn").unwrap(),
        html.find("c2@x.cn").unwrap(),
    );
    assert!(n2 < n0 && n0 < n1, "sort=name 应按姓名升序（UTF-8 字节序）");
    // 姓名排序排除未命名联系人（无可排姓名的行不显示）
    assert!(
        !html.contains("c3@x.cn"),
        "sort=name 不应显示未命名联系人：{html}"
    );
    // sort=addr：地址升序，且未命名联系人在此视图保留
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "sort=addr&per=10", b""),
        &tester,
    ));
    assert!(
        html.contains(r#"<option value="addr" selected"#),
        "addr 选项应选中：{html}"
    );
    let pos = |needle: &str| html.find(needle).unwrap();
    assert!(
        pos("c0@x.cn") < pos("c1@x.cn")
            && pos("c1@x.cn") < pos("c2@x.cn")
            && pos("c2@x.cn") < pos("c3@x.cn"),
        "sort=addr 应按地址升序"
    );
    cleanup(&dir);
}

// ---------- M4-2 定时发送（compose 页 + 详情/列表展示） ----------

/// 定时发送主链路：未来时刻提交 → 草稿落 scheduled_at、不入队 → 跳详情页
/// 展示「已定时」；past 时刻被拒；立即发送清定时并排队
#[test]
fn compose_schedule_send_flow() {
    let (state, dir) = temp_state("m4-sched-flow");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let future = mail::sched_local_value(mail::now_epoch() + 3600);
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 定时发送：落计划、跳详情
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("subject", "定时汇报"),
                ("body_text", "定时发送的内容"),
                ("body_mode", "plain"),
                ("scheduled_at", future.as_str()),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &t,
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/emails/1"),
        "定时成功应跳草稿详情页：{:?}",
        resp.extra_headers
    );
    let (folder, status, at): (String, String, i64) = state.with_conn(|c| {
        c.query_row(
            "SELECT folder, status, scheduled_at FROM emails WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    });
    assert_eq!(
        (folder.as_str(), status.as_str()),
        ("draft", "draft"),
        "定时邮件停留草稿"
    );
    assert!(at > mail::now_epoch(), "scheduled_at 应为未来时刻");
    // 详情页渲染「已定时」与完整时刻
    let html = body(&route(&state, &assets(), &req("GET", "/emails/1", b""), &t));
    assert!(html.contains("已定时"), "详情应有定时状态条：{html}");
    assert!(
        html.contains(mail::time_full(at).as_str()),
        "定时时刻完整展示"
    );
    // 草稿箱列表行 ⏰ 徽标
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "folder=draft", b""),
        &t,
    ));
    assert!(
        html.contains("mr-flag sched"),
        "草稿行应有定时徽标（SVG 时钟）：{html}"
    );
    // 撰写页回显控件值（datetime-local）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "draft_id=1", b""),
        &t,
    ));
    assert!(
        html.contains(&format!("value=\"{future}\"")),
        "编辑定时草稿应回显控件值：{html}"
    );

    // 过时刻拒绝：不落库、不产生新草稿
    let past = mail::sched_local_value(mail::now_epoch() - 3600);
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("subject", "过时定时"),
                ("body_text", "x"),
                ("body_mode", "plain"),
                ("scheduled_at", past.as_str()),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &t,
    );
    let location = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Redirect")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(
        location.contains("error=") && !location.contains("/emails/"),
        "过时刻应被拒并带错误回撰写页：{location}"
    );
    let total: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(total, 1, "拒绝路径不应生成新草稿");

    // 立即发送定时草稿（scheduled_at 置空）：清计划 + 排队
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("draft_id", "1"),
                ("to_addrs", "peer@x.cn"),
                ("subject", "定时汇报"),
                ("body_text", "改为立即发"),
                ("body_mode", "plain"),
                ("scheduled_at", ""),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &t,
    );
    let location = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Redirect")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(
        location.contains("just_sent=1"),
        "立即发送应走既有反馈链路：{location}"
    );
    let (status, at): (String, i64) = state.with_conn(|c| {
        c.query_row(
            "SELECT status, scheduled_at FROM emails WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(status, "draft", "清空定时后立即发送先进入撤销窗口（A1）");
    assert!(
        at > mail::now_epoch() && at <= mail::now_epoch() + 10,
        "撤销窗口的 scheduled_at 应为未来 8 秒左右：{at}"
    );

    cleanup(&dir);
}

/// 定时草稿由调度线程派发（M4-2 E2E 内核）：due 行转 queued——
/// 与 mail_send::dispatch_due_scheduled 单元测试互补，本测试验证
/// 「HTTP 定时 → 调度派发」两级衔接（state 无 worker：queued 即终态）
#[test]
fn scheduled_draft_is_dispatched_when_due() {
    let (state, dir) = temp_state("m4-sched-dispatch");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 建定时草稿后手改到点（模拟时间流逝，比构造未来 2 分钟更可控）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("subject", "定时到期"),
                ("body_text", "到点自动发"),
                ("body_mode", "plain"),
                (
                    "scheduled_at",
                    mail::sched_local_value(mail::now_epoch() + 3600).as_str(),
                ),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &t,
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/emails/1")
    );
    state
        .with_conn(|c| {
            c.execute(
                "UPDATE emails SET scheduled_at = ?1 WHERE id = 1",
                [mail::now_epoch() - 1],
            )
            .unwrap();
            Ok::<(), ()>(())
        })
        .unwrap();

    assert_eq!(mail::dispatch_due_scheduled(&state), 1, "到点草稿应被派发");
    let status: String = state.with_conn(|c| {
        c.query_row("SELECT status FROM emails WHERE id = 1", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(
        status, "queued",
        "派发后进入发送队列（无 worker 时停留，启动补发）"
    );
    cleanup(&dir);
}

/// 存草稿保留/取消定时（M4-2）：定时草稿清空控件后存草稿 → scheduled_at 归零；
/// 未改控件存草稿 → 计划保留
#[test]
fn draft_save_keeps_or_clears_schedule() {
    let (state, dir) = temp_state("m4-sched-save");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let future = mail::sched_local_value(mail::now_epoch() + 3600);
    // 建定时草稿（经 HTTP 路径，与 M4-2 计划一致）
    route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("subject", "计划草稿"),
                ("body_text", "x"),
                ("body_mode", "plain"),
                ("scheduled_at", future.as_str()),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &t,
    );
    // 未改控件再存草稿：计划保留
    route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("draft_id", "1"),
                ("to_addrs", "peer@x.cn"),
                ("subject", "计划草稿"),
                ("body_text", "改了一下正文"),
                ("body_mode", "plain"),
                ("scheduled_at", future.as_str()),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &t,
    );
    let at: i64 = state.with_conn(|c| {
        c.query_row("SELECT scheduled_at FROM emails WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert!(at > 0, "存草稿应保留定时计划");
    // 清空控件存草稿：取消定时
    route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("draft_id", "1"),
                ("to_addrs", "peer@x.cn"),
                ("subject", "计划草稿"),
                ("body_text", "再改一下"),
                ("body_mode", "plain"),
                ("scheduled_at", ""),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &t,
    );
    let (at, status): (i64, String) = state.with_conn(|c| {
        c.query_row(
            "SELECT scheduled_at, status FROM emails WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(
        (at, status.as_str()),
        (0, "draft"),
        "清空控件存草稿应取消定时且保持草稿"
    );
    cleanup(&dir);
}

/// 批量星标进批量操作栏（M2-1 扩展）：star/unstar 动作经 /emails/batch
/// 生效——ids 与跨页两模式受影响数正确、草稿箱星标置顶重排、幂等重试返回 0
#[test]
fn batch_actions_star_unstar() {
    let (state, dir) = temp_state("batch-star");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "x.cn"),
        ])
        .unwrap();
    // 种子：3 封收件（不同时间命中跨页过滤集）+ 1 封草稿
    state
        .with_conn(|c| {
            for i in 0..3 {
                c.execute(
                    "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                        subject_tokens, body_tokens, unread, created_at, received_at)
                     VALUES ('in','inbox','received','a@x.cn','b@x.cn',?1,?1,'x',0,?2,?2)",
                    rusqlite::params![format!("收件{i}"), 100 + i],
                )
                .unwrap();
            }
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, unread, created_at)
                 VALUES ('out','draft','draft','b@x.cn','a@x.cn','草稿D','草稿D','x',0,50)",
                [],
            )
            .unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // ids 模式：星标第 1、2 封收件
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[("action", "star"), ("ids", "[1,2]"), ("folder", "inbox")]),
        ),
        &t,
    );
    assert_eq!(resp.status, 200);
    let starred: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails WHERE starred = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert_eq!(starred, 2, "两封应置星标");
    // SQLite changes 语义：UPDATE 匹配行即计数（不比较新旧值）——
    // 重复星标仍报 2 行受影响（值未变），语义无损（前端只看成功与否）
    let n = mail::batch_apply(
        &state,
        &mail::BatchTarget::Ids(vec![1, 2]),
        mail::BatchAction::Star,
    );
    assert_eq!(n.unwrap(), 2, "重复星标按匹配计数（SQLite changes 语义）");
    // 列表重排：星标置顶（id 1/2 压过更新的 id 3）
    let html = body(&route(&state, &assets(), &req("GET", "/", b""), &t));
    let first_pos = html.find("收件0").unwrap();
    let last_pos = html.find("收件2").unwrap();
    assert!(
        first_pos < last_pos,
        "星标邮件应置顶（含 mbx:fetchDone 后片段重排）"
    );

    // 跨页模式（select_all）：星标全部收件含草稿（星标对所有文件夹生效）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[
                ("action", "star"),
                ("select_all", "1"),
                ("exclude_ids", "[3]"),
                ("folder", "inbox"),
                ("unread", "0"),
                ("external", "0"),
                ("q", ""),
            ]),
        ),
        &t,
    );
    assert_eq!(resp.status, 200);
    let starred_inbox: i64 = state.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM emails WHERE starred = 1 AND direction = 'in'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(starred_inbox, 2, "排除 id=3 后仍只有原两封（跨页排除生效）");

    // unstar 全部回零
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/batch",
            &form(&[
                ("action", "unstar"),
                ("select_all", "1"),
                ("exclude_ids", "[]"),
                ("folder", "inbox"),
                ("unread", "0"),
                ("external", "0"),
                ("q", ""),
            ]),
        ),
        &t,
    );
    assert_eq!(resp.status, 200);
    let starred: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM emails WHERE starred = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert_eq!(starred, 0, "unstar 跨页全量应清零星标");
    cleanup(&dir);
}

/// 每页条数（per）语义：默认 10；URL per=20 生效且持久化到 settings；
/// 非法/越界钳制；非默认值在翻页链接与 list_query 往返保持
#[test]
fn list_per_page_select_and_persist() {
    let (state, dir) = temp_state("per-page");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state
        .with_conn(|c| {
            for i in 0..25 {
                c.execute(
                    "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, created_at)
                 VALUES ('in','inbox','received','a@x.cn','b@x.cn',?1,?1,'x',1)",
                    [i],
                )
                .unwrap();
            }
            Ok::<(), String>(())
        })
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 默认（无 per）：10 条/页、第 1 页显示 "1 / 3"
    let html = body(&route(&state, &assets(), &req("GET", "/", b""), &t));
    assert!(html.contains("1 / 3"), "25 封默认 10/页 = 3 页");
    assert_eq!(html.matches("data-mail-id").count(), 10, "默认每页 10 行");
    // 选择器存在且 selected=10
    assert!(html.contains("per-select"), "顶栏应有每页选择器");
    assert!(html.contains("<option value=\"10\" selected"));
    // ?per=20：第 1 页显示 "1 / 2"、URL 持久化到 settings
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "per=20", b""),
        &t,
    ));
    assert!(html.contains("1 / 2"), "25 封 per=20 = 2 页");
    let saved: Option<String> = state.get(keys::PAGE_SIZE).unwrap();
    assert_eq!(saved.as_deref(), Some("20"), "per 选择应持久化");
    // 分页链接携带 per（page_base 注入；第 1 页下一页指向 page=2）
    assert!(
        html.contains("per=20&amp;page=2") || html.contains("per=20&page=2"),
        "翻页链接应保持 per：{}",
        html.lines().find(|l| l.contains("pg-cur")).unwrap_or("?")
    );
    // 无参访问沿用持久化值 20
    let html = body(&route(&state, &assets(), &req("GET", "/", b""), &t));
    assert!(html.contains("1 / 2"), "settings 持久化的 20 条/页应生效");
    // 越界钳制：per=9999 → 100，25 封一页装下（无分页器）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "per=9999", b""),
        &t,
    ));
    assert_eq!(
        html.matches("data-mail-id").count(),
        25,
        "per 钳到 100 后 25 封一页"
    );
    // 非法值：回退 settings（已被上一步持久化为 100）不炸、不致 0 页
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "per=abc", b""),
        &t,
    ));
    assert_eq!(
        html.matches("data-mail-id").count(),
        25,
        "非法 per 应回退 settings(100)"
    );
    // list_query 往返（详情上下文）：per=20 进链
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "per=20&page=2", b""),
        &t,
    ));
    assert!(
        html.contains("per=20"),
        "list_query 应携带非默认 per（详情往返保持）"
    );
    cleanup(&dir);
}

/// 列表行不再渲染星标按钮（2026-11-04 设计变更）：星标行以金色脊线 class
/// 表达（.mail-row.starred），行内无 star-btn；批量工具栏 star/unstar 仍在
#[test]
fn list_rows_show_starred_as_spine_not_button() {
    let (state, dir) = temp_state("star-spine");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "x.cn"),
        ])
        .unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    subject_tokens, body_tokens, created_at, starred) VALUES
                 ('in','inbox','received','a@x.cn','b@x.cn','星标件','星标件','x',1,1),
                 ('in','inbox','received','a@x.cn','b@x.cn','普通件','普通件','x',1,0)",
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let html = body(&route(&state, &assets(), &req("GET", "/", b""), &t));
    assert!(
        html.contains("mail-row starred"),
        "星标行应有 starred class（金脊线钩子）：{html}"
    );
    assert!(
        !html.contains("class=\"star-btn on\""),
        "列表行不应再有星标按钮"
    );
    assert!(
        html.contains("data-batch=\"unstar\""),
        "批量工具栏保留置顶/取消置顶操作"
    );
    cleanup(&dir);
}

/// 侧栏改版（2026-11-04）：「文件夹」分组标题删除；设置入口从底部 gear
/// 迁到联系人下方的 folder 行（settings-entry，可被收取屏蔽）；核心图标
/// emoji 化（瘦符号换实心形态）
#[test]
fn side_settings_entry_in_folder_list_and_no_folder_title() {
    let (state, dir) = temp_state("side-entry");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let html = body(&route(&state, &assets(), &req("GET", "/", b""), &t));
    // 「文件夹」标题删除（分组自明，冗余）
    assert!(
        !html.contains("side-title") && !html.contains(">文件夹<"),
        "侧栏不应再有「文件夹」分组标题"
    );
    // 设置入口：folder 列表内 settings-entry（gear 已除名）
    assert!(
        html.contains("settings-entry") && html.contains("<svg class=\"ic\""),
        "设置应为侧栏 folder 行入口（SVG 图标）"
    );
    assert!(!html.contains("class=\"gear\""), "底部 gear 入口应移除");
    // 位置：设置行紧跟联系人行（DOM 顺序 = 渲染顺序）
    let contacts_pos = html.find("/contacts").unwrap_or(usize::MAX);
    let settings_pos = html.find("/settings").unwrap_or(usize::MAX);
    assert!(
        contacts_pos < settings_pos,
        "设置入口应在联系人下方（DOM 序）"
    );
    // 图标全面 SVG 化（2026-11-10）：核心入口在场且不再出现 emoji
    assert!(
        html.contains("compose-btn") && html.contains("<svg class=\"ic\""),
        "撰写按钮应使用 SVG 图标"
    );
    assert!(
        !html.contains("📥")
            && !html.contains("👥")
            && !html.contains("🔄")
            && !html.contains("✏️"),
        "侧栏/顶栏不应再出现 emoji 图标"
    );
    // 设置页侧栏：settings-entry 高亮（frame.folder=settings）
    let html = body(&route(&state, &assets(), &req("GET", "/settings", b""), &t));
    assert!(
        html.contains("folder settings-entry on"),
        "设置页侧栏设置行应高亮：{}",
        html.lines().nth(30).unwrap_or("?")
    );
    cleanup(&dir);
}

/// A2 列表排序：URL sort 生效、非默认值进入详情往返上下文、非法值回退默认
#[test]
fn list_sort_applies_and_persists_in_list_query() {
    let (state, dir) = temp_state("a2-sort");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute_batch(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, subject_tokens, body_tokens, unread,
                                created_at, received_at, thread_key)
             VALUES
             ('in','inbox','received','a@x.cn','甲','demo@chinatelecom.cn',
              '第一封','正文一','第一封','正文 一',1,100,100,'第一封'),
             ('in','inbox','received','b@x.cn','乙','demo@chinatelecom.cn',
              '第二封','正文二','第二封','正文 二',1,200,200,'第二封')",
        )
        .unwrap();
    });
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 时间正序：第一封（id=1）应排在第二封（id=2）之前
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "sort=date_asc", b""),
        &tester,
    ));
    let p1 = html.find("data-mail-id=\"1\"").expect("应渲染第一封");
    let p2 = html.find("data-mail-id=\"2\"").expect("应渲染第二封");
    assert!(p1 < p2, "时间正序应 id1 在前：{p1} vs {p2}");
    assert!(
        html.contains("sort=date_asc"),
        "详情链接应携带排序参数（往返保持）"
    );
    // 默认（时间倒序）：id2 在前
    let html = body(&route(&state, &assets(), &req("GET", "/", b""), &tester));
    let p1 = html.find("data-mail-id=\"1\"").unwrap();
    let p2 = html.find("data-mail-id=\"2\"").unwrap();
    assert!(p2 < p1, "默认时间倒序应 id2 在前");
    // 非法值回退默认：URL 不残留 sort=bogus
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "sort=bogus", b""),
        &tester,
    ));
    assert!(!html.contains("sort=bogus"), "非法排序值不应透传");
    let p1 = html.find("data-mail-id=\"1\"").unwrap();
    let p2 = html.find("data-mail-id=\"2\"").unwrap();
    assert!(p2 < p1, "非法排序值应按默认时间倒序");
    // 详情页返回列表链接保留 sort
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/emails/1", "sort=date_asc", b""),
        &tester,
    ));
    assert!(
        html.contains("sort=date_asc"),
        "详情页返回链接应保留排序参数"
    );
    cleanup(&dir);
}

/// A4 全部另存路由：POST /emails/{id}/attachments/all 逐附件落盘；
/// 目录缺失/无附件时回错误 toast（不 500）
#[test]
fn save_all_attachments_route_writes_and_reports_errors() {
    let (state, dir) = temp_state("a4-save-all");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                body_text, subject_tokens, body_tokens, unread, created_at, received_at)
             VALUES ('in','inbox','received','a@x.cn','demo@chinatelecom.cn','带附件',
                     '正文','带 附件','正文',1,1,1)",
            [],
        )
        .unwrap();
    });
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("清单.xlsx"), b"XLSX").unwrap();
    mail::add_attachments(&state, 1, &[src.join("清单.xlsx").to_str().unwrap().into()]).unwrap();
    let dest = dir.join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/1/attachments/all",
            &form(&[("dest_dir", dest.to_str().unwrap())]),
        ),
        &tester,
    );
    let html = body(&resp);
    assert!(html.contains("已保存 1 个附件"), "应回成功 toast：{html}");
    assert_eq!(std::fs::read(dest.join("清单.xlsx")).unwrap(), b"XLSX");
    // 目录不存在：错误 toast，不是 500
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/1/attachments/all",
            &form(&[("dest_dir", dir.join("nope").to_str().unwrap())]),
        ),
        &tester,
    );
    assert_eq!(resp.status, 200, "错误应回 toast 而非 500");
    assert!(body(&resp).contains("目录不存在"));
    cleanup(&dir);
}

/// A1 撤销发送：窗口内撤销 → 清 scheduled_at 回撰写页（带 undone 提示）；
/// 窗口已过（已入队）→ 拒绝撤销并回详情页提示，不改发送状态
#[test]
fn undo_send_cancels_window_or_reports_too_late() {
    let (state, dir) = temp_state("a1-undo");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 发送（进入撤销窗口）
    route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/send",
            &form(&[
                ("to_addrs", "peer@x.cn"),
                ("subject", "待撤销"),
                ("body_text", "正文"),
                ("body_mode", "plain"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
            ]),
        ),
        &t,
    );
    // 撤销：回撰写页并带 undone=1
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/undo-send", b""),
        &t,
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/compose?draft_id=1&undone=1"),
        "撤销应回撰写页：{:?}",
        resp.extra_headers
    );
    let (status, at): (String, i64) = state.with_conn(|c| {
        c.query_row(
            "SELECT status, scheduled_at FROM emails WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(status, "draft");
    assert_eq!(at, 0, "撤销应清除 scheduled_at");
    // 撰写页提示已撤销
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", "draft_id=1&undone=1", b""),
        &t,
    ));
    assert!(html.contains("已撤销发送"), "撰写页应提示已撤销：{html}");
    // 模拟窗口已过（调度线程已入队）：撤销请求应被拒绝
    state.with_conn(|c| {
        c.execute(
            "UPDATE emails SET status = 'queued', scheduled_at = 0 WHERE id = 1",
            [],
        )
        .unwrap();
    });
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/undo-send", b""),
        &t,
    );
    let loc = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Redirect")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(
        loc.starts_with("/emails/1?error="),
        "窗口已过应回详情页提示：{loc}"
    );
    let status: String = state.with_conn(|c| {
        c.query_row("SELECT status FROM emails WHERE id = 1", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(status, "queued", "拒绝撤销不得改动发送状态");
    cleanup(&dir);
}

/// B1 高级检索：URL 条件生效、详情往返保持、非法日期静默忽略
#[test]
fn advanced_search_filters_and_persists() {
    let (state, dir) = temp_state("b1-adv");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let now = mail::now_epoch();
    let today = mail::sched_local_value(now)[..10].to_string();
    let yesterday = mail::sched_local_value(now - 86_400)[..10].to_string();
    state.with_conn(|c| {
        c.execute_batch(&format!(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, subject_tokens, body_tokens, unread,
                                created_at, received_at, attach_cnt, thread_key)
             VALUES
             ('in','inbox','received','boss@chinatelecom.cn','王总','demo@chinatelecom.cn',
              '今天的信','x','今天','x',1,{now},{now},0,'今天的信'),
             ('in','inbox','received','market@partner.cn','合作方','demo@chinatelecom.cn',
              '昨天的信','y','昨天','y',1,{y1},{y1},2,'昨天的信')",
            y1 = now - 86_400
        ))
        .unwrap();
    });
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 发件人包含 partner → 只剩昨天的信
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "from=partner", b""),
        &t,
    ));
    assert!(html.contains("昨天的信"), "应按发件人命中：{html}");
    assert!(!html.contains("今天的信"), "非匹配发件人不应出现");
    assert!(
        html.contains("from=partner"),
        "详情链接应携带发件人条件（往返保持）"
    );
    // 仅附件 → 昨天的信（attach_cnt=2）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "att=1", b""),
        &t,
    ));
    assert!(html.contains("昨天的信"));
    assert!(!html.contains("今天的信"));
    // 日期区间：今天起 → 只剩今天的信
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", &format!("df={today}"), b""),
        &t,
    ));
    assert!(html.contains("今天的信"), "起始日期应命中今天：{html}");
    assert!(!html.contains("昨天的信"), "昨天应被日期排除");
    // 结束日期 = 昨天 → 只剩昨天的信
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", &format!("dt={yesterday}"), b""),
        &t,
    ));
    assert!(html.contains("昨天的信"));
    assert!(!html.contains("今天的信"));
    // 非法日期：静默忽略（按未填处理，两封都在）
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "df=2026-02-30&dt=abc", b""),
        &t,
    ));
    assert_eq!(
        html.matches("data-mail-id=").count(),
        2,
        "非法日期应忽略：{html}"
    );
    // 高级面板显隐（2026-11-09 调整）：有激活筛选保持展开——点「应用筛选」
    // 提交回来仍能看到/修改条件；无筛选默认收起
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/", "from=partner", b""),
        &t,
    ));
    // 取面板开标签整体（含可能的换行），判定 hidden 显隐
    fn panel_tag(html: &str) -> &str {
        let pos = html.find(r#"id="adv-panel""#).unwrap();
        let start = html[..pos].rfind("<form").unwrap();
        let end = html[pos..].find('>').unwrap() + pos;
        &html[start..=end]
    }
    assert!(
        !panel_tag(&html).contains("hidden"),
        "有激活筛选时面板应保持展开（不带 hidden）：{html}"
    );
    let html = body(&route(&state, &assets(), &req("GET", "/", b""), &t));
    assert!(
        panel_tag(&html).contains("hidden"),
        "无筛选时面板应默认收起：{html}"
    );
    cleanup(&dir);
}

/// B2 模板页与保存/删除路由：设置页 tab、片段回显、撰写页下拉
#[test]
fn template_settings_and_compose_picker() {
    let (state, dir) = temp_state("b2-tpl");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 设置页 templates tab 可打开
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/templates", b""),
        &t,
    ));
    assert!(html.contains("邮件模板"), "应渲染模板页：{html}");
    assert!(html.contains("还没有模板"), "空态提示");
    // 新增（HTMX 片段）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/templates/save",
            &form(&[
                ("name", "收到请回复"),
                ("body_text", "收到，我核对后回复您。"),
                ("body_html", ""),
            ]),
        ),
        &t,
    );
    let html = body(&resp);
    assert!(
        html.contains("已保存") && html.contains("class=\"ic\""),
        "保存成功提示：{html}"
    );
    assert!(html.contains("收到请回复"), "列表应出现新模板");
    // 撰写页下拉含模板（HTML 转义后的 data-text）
    let html = body(&route(&state, &assets(), &req("GET", "/compose", b""), &t));
    assert!(html.contains("id=\"tpl-select\""), "撰写页应有模板下拉");
    assert!(html.contains("收到请回复"), "下拉应含模板名");
    // 删除
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/templates/delete", &form(&[("id", "1")])),
        &t,
    );
    let html = body(&resp);
    assert!(html.contains("还没有模板"), "删除后回到空态");
    cleanup(&dir);
}

/// 模板内容与邮件正文逻辑对齐（批次五修订）：单一内容字段按 body_mode
/// 单选保存——html 模式后端从 body_html 派生纯文本（预览/纯文本插入回退），
/// plain 模式只存 body_text；编辑回显时存量有 body_html 直接 html 模式
/// （不可降级），无则 plain 可单向升级。表单不再有「转为纯文本」入口。
#[test]
fn template_body_mode_single_field_semantics() {
    let (state, dir) = temp_state("b5-tpl-mode");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // html 模式保存：表单 body_text 应被忽略，由 body_html 派生
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/templates/save",
            &form(&[
                ("name", "富文本模板"),
                ("body_mode", "html"),
                ("body_text", "这段应被忽略"),
                ("body_html", "<p>各位领导：</p><p>清单见附件。</p>"),
            ]),
        ),
        &t,
    );
    let html = body(&resp);
    assert!(
        html.contains("已保存") && html.contains("class=\"ic\""),
        "保存成功：{html}"
    );
    let (text, tpl_html): (String, String) = state.with_conn(|c| {
        c.query_row(
            "SELECT body_text, body_html FROM templates WHERE name = '富文本模板'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(tpl_html, "<p>各位领导：</p><p>清单见附件。</p>");
    assert!(
        text.contains("各位领导") && text.contains("清单见附件"),
        "body_text 应由 body_html 派生：{text}"
    );
    assert!(!text.contains("这段应被忽略"), "表单 body_text 应被忽略");
    // 编辑回显：存量有 body_html → 回 html 模式；无「转为纯文本」，有单向升级
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/templates", b""),
        &t,
    ));
    assert!(
        html.contains("name=\"body_mode\" value=\"html\""),
        "存量 html 模板编辑应回 html 模式：{html}"
    );
    assert!(html.contains("切换为富文本"), "应保留单向升级入口");
    assert!(!html.contains("转为纯文本"), "不应再有降级入口");
    assert!(
        html.contains("name=\"body_mode\" value=\"plain\""),
        "新增表单应初始 plain：{html}"
    );
    // plain 模式保存：body_html 清空（表单值被忽略）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/templates/save",
            &form(&[
                ("name", "纯文本模板"),
                ("body_mode", "plain"),
                ("body_text", "收到，我核对后回复您。"),
                ("body_html", "<p>应被忽略</p>"),
            ]),
        ),
        &t,
    );
    assert!(body(&resp).contains("已保存") && body(&resp).contains("class=\"ic\""));
    let saved_html: String = state.with_conn(|c| {
        c.query_row(
            "SELECT body_html FROM templates WHERE name = '纯文本模板'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(saved_html, "", "plain 模式 body_html 应清空");
    cleanup(&dir);
}

/// B3 联系人手动新增/编辑：校验、hits 不因编辑变化、编辑态回显
#[test]
fn contact_save_creates_updates_and_validates() {
    let (state, dir) = temp_state("b3-contact");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 新增（无搜索词）
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/contacts/save",
            &form(&[("addr", "New.Peer@Example.com"), ("name", "新同事")]),
        ),
        &t,
    );
    let html = body(&resp);
    assert!(
        html.contains("已保存联系人 New.Peer@Example.com"),
        "应回保存提示：{html}"
    );
    assert!(html.contains("新同事"), "列表应出现新联系人");
    let (name, hits): (String, i64) = state.with_conn(|c| {
        c.query_row(
            "SELECT name, hits FROM contacts WHERE addr = 'New.Peer@Example.com'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(name, "新同事");
    assert_eq!(hits, 1, "手动新增 hits 从 1 起");
    // 编辑：只改姓名，hits 不变
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/contacts/save",
            &form(&[("addr", "new.peer@example.com"), ("name", "改名后")]),
        ),
        &t,
    );
    assert!(body(&resp).contains("已保存联系人"));
    let (name, hits): (String, i64) = state.with_conn(|c| {
        c.query_row(
            "SELECT name, hits FROM contacts WHERE addr = 'New.Peer@Example.com'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(name, "改名后");
    assert_eq!(hits, 1, "手动编辑不应累加往来次数");
    let total: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM contacts", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(total, 1, "NOCASE 主键下同地址不应新增行");
    // 非法地址与超长姓名
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/contacts/save",
            &form(&[("addr", "not-an-addr"), ("name", "")]),
        ),
        &t,
    );
    assert!(body(&resp).contains("缺少 @"), "应提示地址格式");
    let long_name = "名".repeat(61);
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/contacts/save",
            &form(&[("addr", "ok@x.cn"), ("name", long_name.as_str())]),
        ),
        &t,
    );
    assert!(body(&resp).contains("最多 60"), "应提示姓名超长");
    let total: i64 = state.with_conn(|c| {
        c.query_row("SELECT count(*) FROM contacts", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(total, 1, "非法输入不应落库");
    // 编辑态回显：地址只读、姓名预填
    let resp = route(
        &state,
        &assets(),
        &req_q("GET", "/contacts", "edit=New.Peer%40Example.com", b""),
        &t,
    );
    let html = body(&resp);
    assert!(
        html.contains("value=\"New.Peer@Example.com\""),
        "应预填地址：{html}"
    );
    assert!(html.contains("readonly"), "编辑态地址只读");
    assert!(html.contains("value=\"改名后\""), "应预填姓名");
    assert!(html.contains("保存修改"), "按钮文案切换");
    cleanup(&dir);
}

/// B4 导出 .eml 路由：写盘成功回成功 toast；空 dest 回错误 toast；不 500
#[test]
fn export_eml_route_writes_file() {
    let (state, dir) = temp_state("b4-export-route");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                body_text, subject_tokens, body_tokens, unread, created_at, received_at)
             VALUES ('in','inbox','received','a@189.cn','demo@chinatelecom.cn','导出测试',
                     '正文','导出 测试','正文',1,1,1)",
            [],
        )
        .unwrap();
    });
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let dest = dir.join("out.eml");
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/1/export-eml",
            &form(&[("dest", dest.to_str().unwrap())]),
        ),
        &tester,
    );
    let html = body(&resp);
    assert!(html.contains("已导出到"), "应回成功 toast：{html}");
    let raw = std::fs::read_to_string(&dest).unwrap();
    assert!(raw.contains("Subject: =?UTF-8?B?"), "主题应编码：{raw}");
    assert!(raw.contains("Content-Type: text/plain"), "{raw}");

    // 未选保存位置：错误提示而非 500
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/export-eml", &form(&[])),
        &tester,
    );
    assert!(body(&resp).contains("未选择保存位置"));
    cleanup(&dir);
}

/// B6 作为附件转发：首次保存附加 .eml；重存（keep_ids 带回）不重复
#[test]
fn compose_attach_eml_saves_once() {
    let (state, dir) = temp_state("b6-compose");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                body_text, subject_tokens, body_tokens, unread, created_at, received_at)
             VALUES ('in','inbox','received','a@189.cn','demo@chinatelecom.cn','源邮件',
                     '源正文','源 邮件','源正文',1,1,1)",
            [],
        )
        .unwrap();
    });
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("to_addrs", "peer@189.cn"),
                ("subject", "Fwd: 源邮件"),
                ("body_text", "请查收"),
                ("keep_ids", "[]"),
                ("attach_paths", "[]"),
                ("attach_eml_from", "1"),
                ("attach_eml_name", "源邮件.eml"),
            ]),
        ),
        &tester,
    );
    assert!(
        resp.extra_headers.iter().any(|(k, _)| k == "HX-Redirect"),
        "首次保存应回撰写页"
    );
    let d = mail::get(&state, 2).unwrap();
    assert_eq!(d.attach_cnt, 1, "应附加一个 .eml");
    assert_eq!(d.attachments[0].mime, "message/rfc822");
    assert_eq!(d.attachments[0].filename, "源邮件.eml");
    let aid = d.attachments[0].id;

    // 重存：页面会带回 keep_ids（含刚附加的 .eml），不得重复添加
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                ("draft_id", "2"),
                ("to_addrs", "peer@189.cn"),
                ("subject", "Fwd: 源邮件"),
                ("body_text", "请查收（改）"),
                ("keep_ids", &format!("[{aid}]")),
                ("attach_paths", "[]"),
                ("attach_eml_from", "1"),
                ("attach_eml_name", "源邮件.eml"),
            ]),
        ),
        &tester,
    );
    let _ = resp;
    let d = mail::get(&state, 2).unwrap();
    assert_eq!(d.attach_cnt, 1, "重存后仍只有一份 .eml");
    cleanup(&dir);
}

/// B5 打印入口：详情页提供打印按钮（调用 window.print，配合 @media print 隐藏界面）
#[test]
fn detail_page_has_print_action() {
    let (state, dir) = temp_state("b5-print");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                body_text, subject_tokens, body_tokens, unread, created_at, received_at)
             VALUES ('in','inbox','received','a@189.cn','demo@chinatelecom.cn','打印测试',
                     '正文','打印 测试','正文',1,1,1)",
            [],
        )
        .unwrap();
    });
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
        html.contains("data-mbx-action=\"print-email\""),
        "详情页应有打印按钮（data-mbx-action 委托）：{html}"
    );
    assert!(html.contains("打印</button>"), "打印按钮文案：{html}");
    cleanup(&dir);
}

fn office_fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/office")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("读取样例 {} 失败：{e}", p.display()))
}

/// B8 Office 预览路由：清洗副本 + 纯文本兜底 + 原 /view 下载语义不变
#[test]
fn office_preview_routes_serve_clean_bytes_and_text() {
    let (state, dir) = temp_state("b8-office");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                body_text, subject_tokens, body_tokens, unread, created_at, received_at)
             VALUES ('in','inbox','received','a@189.cn','demo@chinatelecom.cn','带 Word',
                     '正文','带 word','正文',1,1,1)",
            [],
        )
        .unwrap();
    });
    let bytes = office_fixture("report.docx");
    mail::add_attachment_bytes(
        &state,
        1,
        "季度汇报.docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        &bytes,
    )
    .unwrap();
    let aid = mail::get(&state, 1).unwrap().attachments[0].id;
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };

    let resp = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/1/attachments/{aid}/office"), b""),
        &tester,
    );
    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type, "application/octet-stream");
    assert!(resp.body.starts_with(b"PK"), "应回清洗后的 ZIP");
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "X-Content-Type-Options" && v == "nosniff")
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "Content-Disposition" && v == "inline")
    );

    let resp = route(
        &state,
        &assets(),
        &req(
            "GET",
            &format!("/emails/1/attachments/{aid}/office-text"),
            b"",
        ),
        &tester,
    );
    assert_eq!(resp.status, 200);
    let text = String::from_utf8(resp.body).unwrap();
    assert!(text.contains("宽带提速配案季度汇报"), "{text}");

    // 附件 id 不存在（或跨邮件）：404，不暴露任何字节
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/1/attachments/999/office", b""),
        &tester,
    );
    assert_eq!(resp.status, 404);
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/2/attachments/999/office", b""),
        &tester,
    );
    assert_eq!(resp.status, 404, "跨邮件/不存在的邮件同样 404");

    // 原 /view 仍按下载处理（Office 不做内联 MIME）
    let resp = route(
        &state,
        &assets(),
        &req("GET", &format!("/emails/1/attachments/{aid}/view"), b""),
        &tester,
    );
    assert_eq!(resp.status, 200);
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "Content-Disposition" && v == "attachment")
    );
    cleanup(&dir);
}

/// B8 错误分支：加密 422 / 旧版 doc 415 / 超限 413 / PPTX 文本 415
#[test]
fn office_preview_routes_report_expected_errors() {
    let (state, dir) = temp_state("b8-office-err");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                body_text, subject_tokens, body_tokens, unread, created_at, received_at)
             VALUES ('in','inbox','received','a@189.cn','demo@chinatelecom.cn','错误分支',
                     '正文','错误 分支','正文',1,1,1)",
            [],
        )
        .unwrap();
    });
    let cfb = [0xD0, 0xCF, 0x11, 0xE0, 0, 0, 0, 0];
    mail::add_attachment_bytes(&state, 1, "secret.docx", "application/octet-stream", &cfb).unwrap();
    mail::add_attachment_bytes(&state, 1, "old.doc", "application/msword", &cfb).unwrap();
    mail::add_attachment_bytes(
        &state,
        1,
        "big.docx",
        "application/octet-stream",
        &[b'x'; 2 * 1024 * 1024],
    )
    .unwrap();
    let deck = office_fixture("deck.pptx");
    mail::add_attachment_bytes(
        &state,
        1,
        "deck.pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        &deck,
    )
    .unwrap();
    let ids: Vec<i64> = mail::get(&state, 1)
        .unwrap()
        .attachments
        .iter()
        .map(|a| a.id)
        .collect();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };

    let resp = route(
        &state,
        &assets(),
        &req(
            "GET",
            &format!("/emails/1/attachments/{}/office", ids[0]),
            b"",
        ),
        &tester,
    );
    assert_eq!(resp.status, 422);
    assert!(String::from_utf8_lossy(&resp.body).contains("加密"));

    let resp = route(
        &state,
        &assets(),
        &req(
            "GET",
            &format!("/emails/1/attachments/{}/office", ids[1]),
            b"",
        ),
        &tester,
    );
    assert_eq!(resp.status, 415);
    assert!(String::from_utf8_lossy(&resp.body).contains("不支持"));

    // 预览上限 1MB：2MB 文件直接 413（在格式判定之前）
    state
        .set_batch(&[(keys::OFFICE_PREVIEW_MAX_MB, "1")])
        .unwrap();
    let resp = route(
        &state,
        &assets(),
        &req(
            "GET",
            &format!("/emails/1/attachments/{}/office", ids[2]),
            b"",
        ),
        &tester,
    );
    assert_eq!(resp.status, 413);
    assert!(String::from_utf8_lossy(&resp.body).contains("超过预览上限"));

    // PPTX 不提供文本降级
    let resp = route(
        &state,
        &assets(),
        &req(
            "GET",
            &format!("/emails/1/attachments/{}/office-text", ids[3]),
            b"",
        ),
        &tester,
    );
    assert_eq!(resp.status, 415);
    assert!(String::from_utf8_lossy(&resp.body).contains("PPTX"));
    cleanup(&dir);
}

/// 导出 Markdown 路由（B9）：dest 落盘 + front matter/正文/附件结构；
/// 敏感路径守卫与未选位置提示与 .eml 同口径。
#[test]
fn export_md_route_writes_file() {
    let (state, dir) = temp_state("b9-export-md");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                subject, body_text, body_html, subject_tokens, body_tokens,
                                unread, created_at, received_at)
             VALUES ('in','inbox','received','a@189.cn','客户张','demo@chinatelecom.cn',
                     '配案说明',
                     '占位正文','<p>第一段</p><ul><li>要点一</li><li>要点二</li></ul>',
                     'x','y',1,1,1)",
                [],
            )
            .unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let dest = dir.join("out.md");
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/1/export-md",
            &form(&[("dest", dest.to_str().unwrap())]),
        ),
        &tester,
    );
    let html = body(&resp);
    assert!(html.contains("已导出到"), "应回成功 toast：{html}");
    let raw = std::fs::read_to_string(&dest).unwrap();
    assert!(raw.starts_with("---\n"), "front matter 开头：{raw}");
    assert!(raw.contains("subject: \"配案说明\""), "{raw}");
    assert!(raw.contains("from: \"客户张 <a@189.cn>\""), "{raw}");
    assert!(raw.contains("## 正文"), "{raw}");
    // HTML 正文应转块级结构：段落与列表空行分隔、列表项紧凑
    assert!(raw.contains("第一段\n\n- 要点一\n- 要点二"), "{raw}");
    // 未选保存位置
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/emails/1/export-md", &form(&[])),
        &tester,
    );
    assert!(body(&resp).contains("未选择保存位置"));
    cleanup(&dir);
}

/// 安全修复①端到端：库内主题/地址含 CR/LF 时，导出 .eml 不产生头注入。
/// 主题夹带 `\r\nBcc: evil@x.cn` 若未清洗会被拼成新头（伪造收件人）。
#[test]
fn export_eml_strips_crlf_from_headers() {
    let (state, dir) = temp_state("b4-crlf");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let evil_subject = "normal subject\r\nBcc: evil@attacker.example";
    let evil_from = "spoofer@x.cn\r\nX-Injected: yes";
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                message_id, body_text, subject_tokens, body_tokens,
                                unread, created_at, received_at)
             VALUES ('in','inbox','received',?1,'demo@chinatelecom.cn',?2,?3,
                     '正文','x','y',1,1,1)",
                rusqlite::params![evil_from, evil_subject, "<fake\r\nid@x>"],
            )
            .unwrap();
            Ok::<(), rusqlite::Error>(())
        })
        .unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let dest = dir.join("out.eml");
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/1/export-eml",
            &form(&[("dest", dest.to_str().unwrap())]),
        ),
        &tester,
    );
    assert!(body(&resp).contains("已导出到"));
    let raw = std::fs::read_to_string(&dest).unwrap();
    // 头区（首个空行前）不得出现注入头：任何 CRLF 后接 "Bcc:"/"X-Injected:"/"id@"
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or(("", &raw));
    assert!(
        !head.contains("\r\nBcc:") && !head.contains("\r\nX-Injected:"),
        "头区被注入：{head:?}"
    );
    // CR/LF 应被替换为空格，注入载荷不得成为独立的头行（残留为 Subject
    // 行内文本属预期——它已经不是头字段，无法伪造收件人）
    for line in head.split("\r\n") {
        let l = line.trim_start();
        assert!(
            !l.starts_with("Bcc:") && !l.starts_with("X-Injected:"),
            "注入载荷成为独立头行：{head:?}"
        );
    }
    assert!(!head.contains("\r\nid@"), "Message-ID 注入应清洗：{head:?}");
    // 主体（正文部分）不受影响
    let _ = rest;
    cleanup(&dir);
}

/// 导出后缀自动补全：保存对话框改了文件名/手输路径漏写后缀时，
/// .eml / .md 落盘文件以补全后的路径为准
#[test]
fn export_routes_autocomplete_missing_extension() {
    let (state, dir) = temp_state("export-ext");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                body_text, subject_tokens, body_tokens, unread, created_at, received_at)
             VALUES ('in','inbox','received','a@189.cn','demo@chinatelecom.cn','后缀补全',
                     '正文','x','y',1,1,1)",
            [],
        )
        .unwrap();
        Ok::<(), rusqlite::Error>(())
    })
    .unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // eml：dest 无后缀 → 落盘 .eml
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/1/export-eml",
            &form(&[("dest", dir.join("邮件").to_str().unwrap())]),
        ),
        &tester,
    );
    assert!(body(&resp).contains(".eml"), "toast 应显示补全后路径");
    assert!(dir.join("邮件.eml").exists(), "应落盘到补全后的路径");
    // md：dest 带其他后缀（.txt）→ 追加 .md
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/emails/1/export-md",
            &form(&[("dest", dir.join("归档").to_str().unwrap())]),
        ),
        &tester,
    );
    assert!(body(&resp).contains(".md"), "toast 应显示补全后路径");
    assert!(dir.join("归档.md").exists(), "应落盘到补全后的路径");
    cleanup(&dir);
}

/// 2026-11-10：设置/向导页的辅助说明由行内 hint 小字改为 info 图标 + 气泡，
/// 且全站图形图标统一为内联 SVG（不再出现 emoji）。
#[test]
fn settings_hints_use_tip_and_no_emoji_icons() {
    let (state, dir) = temp_state("w10-tips");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "chinatelecom.cn"),
            (keys::DISPLAY_NAME, "张伟"),
        ])
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 每个页面至少应有的气泡数（存储页随备份/待生效状态增减，取基础项）
    for (path, min_tips) in [
        ("/settings/account", 3),
        ("/settings/sync", 1),
        ("/settings/storage", 2),
        ("/settings/templates", 2),
        ("/welcome/setup", 2),
    ] {
        let html = body(&route(&state, &assets(), &req("GET", path, b""), &t));
        assert!(
            html.contains("class=\"tip-i\"") && html.contains("data-tip="),
            "{path} 辅助说明应为气泡触发点：{html}"
        );
        let n = html.matches("class=\"tip-i\"").count();
        assert!(n >= min_tips, "{path} 气泡数不足（{n} < {min_tips}）");
        assert!(
            !html.contains("class=\"hint\""),
            "{path} 不应再有行内 hint 小字"
        );
        // emoji 图标回归守卫（彩色 emoji 区段 + 常用符号）
        for bad in [
            '📥', '📤', '📝', '👥', '⚙', '✏', '📎', '🔄', '🗑', '⭐', '💬', '🌐', '🎛', '🔍', '🔎',
            '💾', '🖨', '📨', '📄', '📁', '🔒', '🗄', '📡', '⚠', '⌨', '⏰', '⏳', '↪', '↩', '✎', '✓',
            '✗', '✕', '●', '×',
        ] {
            assert!(
                !html.contains(bad),
                "{path} 不应再出现 emoji「{bad}」：{html}"
            );
        }
    }
    cleanup(&dir);
}

// ---------- 首设向导：既有数据目录的采纳/拒用（重装/换机找回历史邮件） ----------

/// 造一个"上一台机器留下的"数据目录：corpmail.db 含 owner 账号的发件与
/// 收件各一封，另写入已完成首拉的收件游标（采纳后应原样保留、增量续收）
fn seed_old_store(dir: &std::path::Path, owner: &str) {
    use rusqlite::params;
    std::fs::create_dir_all(dir).unwrap();
    let conn = crate::db::open(&dir.join("corpmail.db")).unwrap();
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                            body_text, subject_tokens, body_tokens, unread, created_at, sent_at)
         VALUES ('out','sent','sent',?1,'peer@x.cn','旧库外发','正文','旧 库','正文',0,1,1)",
        params![owner],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                            body_text, subject_tokens, body_tokens, unread, created_at, received_at)
         VALUES ('in','inbox','received','peer@x.cn',?1,'旧库来信','正文','旧 库','正文',1,2,2)",
        params![owner],
    )
    .unwrap();
    for (k, v) in [
        (keys::LAST_UIDVALIDITY, "111"),
        (keys::LAST_UID, "55"),
        (keys::FIRST_PULL_DONE_INBOX, "1"),
        (keys::FIRST_PULL_DONE_SENT, "1"),
    ] {
        conn.execute("INSERT INTO settings(k, v) VALUES (?1, ?2)", params![k, v])
            .unwrap();
    }
}

#[test]
fn wizard_adopts_existing_store_of_same_account() {
    // 需求：首设向导选非空数据目录 → 归属账号一致时直接沿用（存量邮件、
    // 收件游标全保留，增量续收），不走"请选择空目录"的拒绝路径
    let (state, dir) = temp_state("wiz-adopt-ok");
    let target = std::env::temp_dir().join(format!("mbx-wiz-adopt-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    seed_old_store(&target, "unit.router@chinatelecom.cn");

    let mut pairs = wizard_pairs(dir.to_str().unwrap());
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
        "同账号既有目录应采纳成功并跳转完成页"
    );
    assert!(state.configured(), "配置应已保存");
    assert_eq!(state.data_dir(), target, "应会话内切换到既有数据目录");
    assert!(state.pending_data_dir().is_none(), "采纳即时生效，无需重启");
    // 存量数据与收件进度原样保留 + 归属标记写入
    let (mails, marker, cursor, pull_inbox) = state.with_conn(|c| {
        (
            c.query_row("SELECT count(*) FROM emails", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            c.query_row(
                "SELECT v FROM settings WHERE k = ?1",
                [keys::STORE_OWNER],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
            c.query_row(
                "SELECT v FROM settings WHERE k = ?1",
                [keys::LAST_UID],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
            c.query_row(
                "SELECT v FROM settings WHERE k = ?1",
                [keys::FIRST_PULL_DONE_INBOX],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
        )
    });
    assert_eq!(mails, 2, "存量邮件应原样保留");
    assert_eq!(marker, "unit.router@chinatelecom.cn", "归属标记应写入");
    assert_eq!(cursor, "55", "收件游标应保留（增量续收）");
    assert_eq!(pull_inbox, "1", "首拉完成标志应保留（不重新全量拉取）");
    // 首拉范围按本次表单覆盖写入（重新拉取按钮按它工作）
    assert_eq!(
        state.get_or(keys::FIRST_FETCH_DAYS, &defaults()).as_deref(),
        Some("7")
    );
    // 锚点（默认目录）启动空库应被清理
    assert!(!dir.join("corpmail.db").exists());
    // 完成页：目录立即生效 + 沿用提示，不出现重启提示
    let done = body(&route(
        &state,
        &assets(),
        &req("GET", "/welcome/done", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        done.contains("数据目录已设置为") && done.contains("立即生效"),
        "done 页应提示目录即时生效：{done}"
    );
    assert!(
        done.contains("2 封历史邮件") && done.contains("增量拉取"),
        "done 页应提示沿用既有数据：{done}"
    );
    assert!(!done.contains("重启应用后生效"), "不应出现重启提示");
    // 主列表能看到旧库来信（真实复用，不是表面指针切换）
    let list = body(&route(
        &state,
        &assets(),
        &req("GET", "/", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        list.contains("旧库来信"),
        "收件箱应显示沿用目录中的既有邮件"
    );
    // 重启视角：指针 + 配置 + 存量数据都在
    {
        let state3 = AppState::open_with_anchor(&target, &dir);
        assert!(state3.configured());
        let n = state3.with_conn(|c| {
            c.query_row("SELECT count(*) FROM emails", [], |r| r.get::<_, i64>(0))
                .unwrap()
        });
        assert_eq!(n, 2, "重启视角存量数据完好");
    }
    state.clear_password("unit.router@chinatelecom.cn");
    let _ = std::fs::remove_dir_all(&target);
    cleanup(&dir);
}

#[test]
fn wizard_rejects_existing_store_of_other_account() {
    // 需求：目标目录数据与登录账号不一致 → 拒绝使用（防两个账号邮件混库），
    // 且不动指针、不动目标目录
    let (state, dir) = temp_state("wiz-adopt-mismatch");
    let target =
        std::env::temp_dir().join(format!("mbx-wiz-adopt-mismatch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    seed_old_store(&target, "colleague@chinatelecom.cn");

    let mut pairs = wizard_pairs(dir.to_str().unwrap());
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
    let b = body(&resp);
    assert!(
        !resp.extra_headers.iter().any(|(k, _)| k == "HX-Redirect"),
        "不一致应拒绝，不跳转：{b}"
    );
    assert!(
        b.contains("colleague@chinatelecom.cn") && b.contains("不一致"),
        "拒绝文案应指明库内账号与不一致：{b}"
    );
    assert!(!state.configured(), "不得落任何配置");
    assert_eq!(state.data_dir(), dir, "当前数据目录不应变化");
    assert!(
        state.get(keys::DATA_DIR).flatten().is_none(),
        "指针不应写入：{:?}",
        state.get(keys::DATA_DIR)
    );
    // 目标目录数据原样（仍 2 封）
    let conn = crate::db::open(&target.join("corpmail.db")).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "目标目录数据不得被改动");
    let _ = std::fs::remove_dir_all(&target);
    cleanup(&dir);
}

#[test]
fn wizard_rejects_existing_store_with_unknown_owner() {
    // 库非空但无法判定归属（无标记、无发件、收件无可统计收件人）→ 保守拒绝
    let (state, dir) = temp_state("wiz-adopt-unknown");
    let target = std::env::temp_dir().join(format!("mbx-wiz-adopt-unknown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    {
        let conn = crate::db::open(&target.join("corpmail.db")).unwrap();
        conn.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                body_text, subject_tokens, body_tokens, unread, created_at, received_at)
             VALUES ('in','inbox','received','peer@x.cn','','哑来信','正文','哑','正文',1,1,1)",
            [],
        )
        .unwrap();
    }
    let mut pairs = wizard_pairs(dir.to_str().unwrap());
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
    let b = body(&resp);
    assert!(
        !resp.extra_headers.iter().any(|(k, _)| k == "HX-Redirect"),
        "无法判定归属应拒绝：{b}"
    );
    assert!(
        b.contains("无法确认目标目录数据的归属账号"),
        "应说明无法判定归属：{b}"
    );
    assert!(!state.configured());
    let _ = std::fs::remove_dir_all(&target);
    cleanup(&dir);
}

#[test]
fn wizard_rejects_non_corpmail_db_directory() {
    // corpmail.db 是无关 SQLite 文件或非 SQLite 文件 → 明确拒绝，不当库用
    let (state, dir) = temp_state("wiz-adopt-foreign");
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // a) 无关 SQLite 文件（缺核心表）
    let target_a = std::env::temp_dir().join(format!("mbx-wiz-foreign-a-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target_a);
    std::fs::create_dir_all(&target_a).unwrap();
    {
        let conn = rusqlite::Connection::open(target_a.join("corpmail.db")).unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
    }
    // b) 非 SQLite 文件（文本冒充）
    let target_b = std::env::temp_dir().join(format!("mbx-wiz-foreign-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target_b);
    std::fs::create_dir_all(&target_b).unwrap();
    std::fs::write(target_b.join("corpmail.db"), b"this is not a database").unwrap();

    for target in [&target_a, &target_b] {
        let mut pairs = wizard_pairs(dir.to_str().unwrap());
        pairs.push(("data_dir", target.to_str().unwrap()));
        let resp = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(&pairs)),
            &t,
        );
        let b = body(&resp);
        assert!(
            !resp.extra_headers.iter().any(|(k, _)| k == "HX-Redirect"),
            "非本应用数据库应拒绝：{target:?} → {b}"
        );
        assert!(
            b.contains("不是 CorpMail 数据目录"),
            "应说明不是本应用数据目录：{b}"
        );
    }
    assert!(!state.configured());
    let _ = std::fs::remove_dir_all(&target_a);
    let _ = std::fs::remove_dir_all(&target_b);
    cleanup(&dir);
}

#[test]
fn wizard_reuses_current_dir_store_when_owner_matches() {
    // corpmail.toml 丢失后重装：当前（默认目录）库有本账号旧数据、无配置 ——
    // 向导直接沿用（不换目录），数据保留、标记补写
    let (state, dir) = temp_state("wiz-reuse-cur");
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    body_text, subject_tokens, body_tokens, unread, created_at, sent_at)
                 VALUES ('out','sent','sent','unit.router@chinatelecom.cn','peer@x.cn','旧外发',
                         '正文','旧','正文',0,1,1)",
                [],
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();
    assert!(!state.store_is_empty(), "前置：当前库非空");
    assert!(!state.configured(), "前置：未配置（config 丢失）");

    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    );
    assert!(
        resp.extra_headers
            .iter()
            .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
        "归属一致的当前库数据应允许沿用并完成配置"
    );
    assert!(state.configured());
    assert_eq!(state.data_dir(), dir, "显式选择当前目录时数据目录不变");
    let (mails, marker) = state.with_conn(|c| {
        (
            c.query_row("SELECT count(*) FROM emails", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            c.query_row(
                "SELECT v FROM settings WHERE k = ?1",
                [keys::STORE_OWNER],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
        )
    });
    assert_eq!(mails, 1, "原有数据保留");
    assert_eq!(marker, "unit.router@chinatelecom.cn", "归属标记补写");
    let done = body(&route(
        &state,
        &assets(),
        &req("GET", "/welcome/done", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(done.contains("已直接沿用"), "done 页应有沿用提示");
    state.clear_password("unit.router@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn wizard_rejects_current_dir_store_of_other_account() {
    // toml 丢失但锚点库属于别的账号：换空目录也不得放行配置（防混库）
    let (state, dir) = temp_state("wiz-reuse-mismatch");
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    body_text, subject_tokens, body_tokens, unread, created_at, sent_at)
                 VALUES ('out','sent','sent','someone.else@chinatelecom.cn','peer@x.cn','他人外发',
                         '正文','他','正文',0,1,1)",
                [],
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // ① 显式选择当前默认目录（页面必选后用户选默认位置）：归属校验仍应拒绝
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/welcome/setup/save",
            &form(&wizard_pairs(dir.to_str().unwrap())),
        ),
        &t,
    );
    let b = body(&resp);
    assert!(
        b.contains("someone.else@chinatelecom.cn") && b.contains("已停止保存"),
        "当前库归属他账号应拒绝保存：{b}"
    );
    assert!(!state.configured());
    // ② 换一个空目录也应拒绝（两处数据不能都留着配新账号）
    let target = std::env::temp_dir().join(format!("mbx-wiz-reuse-mm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    let mut pairs = wizard_pairs(dir.to_str().unwrap());
    pairs.push(("data_dir", target.to_str().unwrap()));
    let resp = route(
        &state,
        &assets(),
        &req("POST", "/welcome/setup/save", &form(&pairs)),
        &t,
    );
    assert!(
        !resp.extra_headers.iter().any(|(k, _)| k == "HX-Redirect"),
        "当前库归属他账号时，换空目录也不得直接配置：{}",
        body(&resp)
    );
    assert!(!state.configured());
    let _ = std::fs::remove_dir_all(&target);
    cleanup(&dir);
}

/// 发件显示名留空：完成页/侧栏/账号页都从发件箱 From 自动补齐并写回配置
#[test]
fn display_name_autofill_is_visible_in_ui() {
    let (state, dir) = temp_state("display-ui");
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
            (keys::DISPLAY_NAME, ""),
        ])
        .unwrap();
    state.with_conn(|c| {
        c.execute(
            "INSERT INTO emails(direction, folder, status, subject, from_addr, from_name,
                                to_addrs, subject_tokens, body_tokens, attach_cnt, attach_bytes,
                                unread, created_at, sent_at)
             VALUES('out','sent','sent','t','demo@chinatelecom.cn','张伟',
                    'x@example.com','','',0,0,0,100,100)",
            [],
        )
        .unwrap();
    });
    let t = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 完成页是首设/沿用既有库后的第一屏，触发推断并写回
    let done = body(&route(
        &state,
        &assets(),
        &req("GET", "/welcome/done", b""),
        &t,
    ));
    assert!(
        done.contains("张伟"),
        "完成页应展示自动补齐的显示名：{done}"
    );
    let saved = state.get(keys::DISPLAY_NAME).flatten();
    assert_eq!(saved.as_deref(), Some("张伟"), "完成页应触发配置写回");
    let side = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/side/frame", "folder=sent", b""),
        &t,
    ));
    assert!(side.contains("张伟"), "侧栏应展示写回后的显示名");
    let settings = body(&route(&state, &assets(), &req("GET", "/settings", b""), &t));
    assert!(
        settings.contains("value=\"张伟\""),
        "账号页应回显写回后的显示名"
    );
    cleanup(&dir);
}

/// 账号页两栏布局：发信 / 收信+签名档并排，且所有字段仍在同一个 form
#[test]
fn account_pane_uses_two_column_layout() {
    let (state, dir) = temp_state("acct-two-col");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/settings/account", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("class=\"settings-pane on account-pane\""));
    assert!(html.contains("class=\"account-grid\""), "应为两栏容器");
    assert!(html.contains("class=\"acct-side\""), "右栏应存在");
    assert!(html.contains("rows=\"3\""), "签名档默认 3 行以压缩高度");
    let a = html.find("<form").unwrap();
    let b = html.find("</form>").unwrap();
    for field in [
        "name=\"password\"",
        "name=\"display_name\"",
        "name=\"signature\"",
        "hx-post=\"/settings/account/test-smtp\"",
        "hx-post=\"/settings/account/test-imap\"",
    ] {
        let at = html
            .find(field)
            .unwrap_or_else(|| panic!("缺少字段 {field}"));
        assert!(at > a && at < b, "{field} 必须留在同一个 form 内");
    }
    cleanup(&dir);
}

/// 首启向导两栏布局（2026-11-11）：账号凭据+数据目录在左、服务器+拉取范围在右。
/// 所有字段仍在同一个 form.setup-card 内——htmx 提交序列化、app.js 表单锁定、
/// 「选择…」按钮的 closest('.port-row') 都依赖这个结构。
#[test]
fn wizard_setup_form_uses_two_column_layout() {
    let (state, dir) = temp_state("setup-two-col");
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/welcome/setup", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(html.contains("class=\"setup-grid\""), "应为两栏容器");
    assert_eq!(
        html.matches("class=\"setup-col\"").count(),
        2,
        "应有左右两栏"
    );
    let a = html.find("<form").unwrap();
    let b = html.find("</form>").unwrap();
    for field in [
        "name=\"account_prefix\"",
        "name=\"password\"",
        "name=\"display_name\"",
        "name=\"data_dir\"",
        "name=\"smtp_host\"",
        "name=\"recv_host\"",
        "name=\"recv_security\"",
        "name=\"first_fetch_days\"",
        "data-mbx-action=\"pick-setup-dir\"",
    ] {
        let at = html
            .find(field)
            .unwrap_or_else(|| panic!("缺少字段 {field}"));
        assert!(at > a && at < b, "{field} 必须留在向导 form 内");
    }
    cleanup(&dir);
}

#[test]
fn private_ip_check_rejects_v4_mapped_and_special_ranges() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    for ip in [
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V4(Ipv4Addr::new(192, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(100, 100, 0, 1)),
        // ::ffff:127.0.0.1 与 ::ffff:169.254.169.254 的 V6 is_loopback() 为 false
        IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001)),
        IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0xa9fe, 0xa9fe)),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ] {
        assert!(is_private_ip(ip), "{ip} 应被拒绝");
    }
    for ip in [
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)),
        IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111)),
    ] {
        assert!(!is_private_ip(ip), "{ip} 应放行");
    }
}

#[test]
fn proxy_image_accepts_bitmap_mimes_only() {
    assert!(allowed_image_mime("image/png"));
    assert!(allowed_image_mime("image/JPEG; charset=binary"));
    assert!(allowed_image_mime("image/avif"));
    assert!(!allowed_image_mime("image/svg+xml"), "SVG 可携带脚本");
    assert!(!allowed_image_mime("text/html"));
    assert!(!allowed_image_mime(""));
}

/// R2 迁移冻结：写请求 409 + 纯文本提示，读页面照常放行。
/// 迁移成功后本进程仍连着旧库，若不在路由层拦截，用户的保存会静默写进
/// 即将被抛弃的旧库——重启后凭空消失
#[test]
fn migration_freeze_blocks_writes_but_allows_reads() {
    let base = std::env::temp_dir().join(format!("mbx-router-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let cur = base.join("cur");
    std::fs::create_dir_all(&cur).unwrap();
    let state = AppState::open(&cur);
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let dest = base.join("moved");
    state
        .move_data_dir(dest.to_str().unwrap())
        .expect("迁移应成功");
    assert!(state.migration_pending(), "迁移成功后进入冻结期");

    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    // 写（POST）：设置保存被拒
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[("sync_interval_min", "5")]),
        ),
        &tester,
    );
    assert_eq!(resp.status, 409, "冻结期写请求必须 409");
    assert!(
        body(&resp).contains("已迁移"),
        "应提示重启：{}",
        body(&resp)
    );
    // 带副作用的 GET：手动收件同样被拒（否则会写旧库并卡住状态条）
    let resp = route(
        &state,
        &assets(),
        &req("GET", "/emails/fetch", b""),
        &tester,
    );
    assert_eq!(resp.status, 409, "冻结期手动收件必须 409");
    // 读：列表页照常渲染（冻结只挡写）
    let resp = route(&state, &assets(), &req("GET", "/", b""), &tester);
    assert_eq!(resp.status, 200, "读页面不受冻结影响");
    assert!(body(&resp).contains("收件箱"), "列表页应正常渲染");
    let _ = std::fs::remove_dir_all(&base);
}

// ---------- R9：迁移「进行中」与「待重启」文案必须分开 ----------

/// R9：后台迁移期间 migration_pending 与 migration_in_progress 同时为 true。
/// 若 409 一律回「数据已迁移，请重启」，用户会主动中断一次仍在跑的复制。
#[test]
fn migration_running_and_pending_restart_have_distinct_409_text() {
    let base = std::env::temp_dir().join(format!("mbx-router-mig-phase-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let cur = base.join("cur");
    std::fs::create_dir_all(&cur).unwrap();
    let state = AppState::open(&cur);
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    // 注入 no-op executor：start_migration 返回 Background 且提前冻结，
    // 正好复现「进行中」的双位同真状态
    state.set_migration_executor(std::sync::Arc::new(|_dest: &str| Ok(())));
    let dest = base.join("moved");
    assert!(matches!(
        state.start_migration(dest.to_str().unwrap()).unwrap(),
        state::MigrateOutcome::Background
    ));
    assert!(state.migration_pending() && state.migration_in_progress());

    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[("sync_interval_min", "5")]),
        ),
        &tester,
    );
    assert_eq!(resp.status, 409, "进行中也必须拦写");
    assert!(
        body(&resp).contains("正在进行中"),
        "进行中必须回「稍候」而非「请重启」：{}",
        body(&resp)
    );
    assert!(
        !body(&resp).contains("请重启"),
        "进行中不得催重启：{}",
        body(&resp)
    );

    // 后台线程收尾（成功路径只清进行中、保留冻结）→ 此后才是「请重启」
    state.mark_migration_finished();
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/settings/sync/save",
            &form(&[("sync_interval_min", "5")]),
        ),
        &tester,
    );
    assert_eq!(resp.status, 409);
    assert!(
        body(&resp).contains("请重启"),
        "落盘完成待重启必须提示重启：{}",
        body(&resp)
    );
    let _ = std::fs::remove_dir_all(&base);
}

// ---------- R44：保存/发送失败回撰写页不丢表单值 ----------
/// 保存失败（草稿 id 已被删/非草稿）→ HX-Redirect 必须带上未落库的
/// 收件人/主题/正文，且撰写页读回这些参数后原样回填
#[test]
fn compose_save_failure_carries_form_values_back() {
    let (state, dir) = temp_state("r44-retry");
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let tester = FakeTester {
        smtp_ok: true,
        imap_ok: true,
    };
    let resp = route(
        &state,
        &assets(),
        &req(
            "POST",
            "/compose/draft",
            &form(&[
                // 不存在的草稿 id → save_draft 直接拒绝，库中不会有任何新内容
                ("draft_id", "999999"),
                ("to_addrs", "peer@x.cn"),
                ("subject", "季度预算"),
                ("body_text", "这是用户刚敲的正文"),
            ]),
        ),
        &tester,
    );
    let loc = resp
        .extra_headers
        .iter()
        .find(|(k, _)| k == "HX-Redirect")
        .map(|(_, v)| v.clone())
        .expect("失败必须回撰写页");
    assert!(loc.contains("error="), "应带错误原因：{loc}");
    assert!(loc.contains("to=peer%40x.cn"), "应带回收件人：{loc}");
    assert!(loc.contains("subject="), "应带回主题：{loc}");
    assert!(loc.contains("body="), "应带回正文：{loc}");

    // 按该 URL 重新打开撰写页：值必须回填（否则用户看到的是空表单）
    let qs = loc.split_once('?').map(|(_, q)| q).unwrap_or("");
    let html = body(&route(
        &state,
        &assets(),
        &req_q("GET", "/compose", qs, b""),
        &tester,
    ));
    assert!(html.contains("这是用户刚敲的正文"), "正文应回填：{html}");
    assert!(html.contains("peer@x.cn"), "收件人应回填");
    assert!(html.contains("季度预算"), "主题应回填");
    cleanup(&dir);
}

/// 正文按 8KB 截断时不得切出半个 UTF-8 序列
#[test]
fn retry_body_truncates_on_char_boundary() {
    let mut f = HashMap::new();
    f.insert("body_text".to_string(), "汉".repeat(3000)); // 9000 字节
    let qs = retry_form_qs(&f);
    let raw = qs
        .split("body=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .expect("应带 body 参数");
    let decoded: String = form_urlencoded::parse(format!("body={raw}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap();
    assert!(decoded.len() <= RETRY_BODY_LIMIT, "超长正文按上限截断");
    assert_eq!(decoded.len() % 3, 0, "截断点必须落在 UTF-8 字符边界");
    assert!(decoded.chars().all(|c| c == '汉'));
}

/// C8：收件同步 / 存储管理四个保存失败分支旧实现都返回「与点击前逐字节相同」
/// 的正常 pane，用户误以为策略已生效。错误必须落到可见错误条。
#[test]
fn settings_save_errors_render_an_error_banner() {
    let (state, dir) = temp_state("c8-settings");
    let msg = "保存失败：磁盘只读";
    let sync = settings_ctx(&state, "sync", false, msg, None, None);
    let html = body(&render_html(&crate::ui::settings::pane_template("sync"), &sync));
    assert!(html.contains(msg), "sync 模板必须渲染错误条：{html}");
    let storage = settings_ctx(&state, "storage", false, msg, None, None);
    let html = body(&render_html(
        &crate::ui::settings::pane_template("storage"),
        &storage,
    ));
    assert!(html.contains(msg), "storage 模板必须渲染错误条：{html}");
    // 四个失败分支都必须走 *_pane_error（含定义各 1 处）
    let src = include_str!("../ui/settings.rs");
    assert!(
        src.matches("sync_pane_error(").count() >= 4,
        "范围变更 / 同步保存 / 重新拉取三个失败分支都要回显"
    );
    assert!(
        src.matches("storage_pane_error(").count() >= 2,
        "存储策略保存失败也要回显"
    );
    cleanup(&dir);
}

/// C12：联系人主列表查询失败不得被 unwrap_or_default 伪装成「没有联系人」——
/// 库锁/损坏时用户会以为联系人全没了，甚至重新录入。
#[test]
fn contacts_query_failure_is_reported_not_mistaken_for_empty_list() {
    let (state, dir) = temp_state("c12-contacts");
    state
        .set_batch(&[(keys::ACCOUNT_PREFIX, "demo"), (keys::DISPLAY_NAME, "张伟")])
        .unwrap();
    state
        .with_conn(|c| c.execute_batch("DROP TABLE contacts"))
        .unwrap();
    let html = body(&route(
        &state,
        &assets(),
        &req("GET", "/contacts", b""),
        &FakeTester {
            smtp_ok: true,
            imap_ok: true,
        },
    ));
    assert!(
        html.contains("读取联系人失败"),
        "必须把查询错误显示出来：{html}"
    );
    assert!(
        html.contains("联系人列表暂时不可用"),
        "空态文案必须区分「读失败」与「真的没有联系人」"
    );
    assert!(
        !html.contains("还没有联系人"),
        "不得把读取失败渲染成空通讯录"
    );
    cleanup(&dir);
}
