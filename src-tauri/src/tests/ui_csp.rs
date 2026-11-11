//! ui 协议层响应头测试（审计 R7：CSP 真正生效）
//!
//! 三个不变量：
//! 1. HTML 响应带 `Content-Security-Policy`，且与 `tauri.conf.json` 逐字一致；
//! 2. 非 HTML（JSON/图片/纯文本）不加 CSP，避免误伤预览/导出；
//! 3. 模板与 app.js 里没有内联事件属性——CSP 一落地它们就会静默失效，
//!    这条测试把「迁移」钉死，防止以后又写回 `onclick=`。

use super::*;
use crate::templates::TEMPLATES;

/// 内联事件属性（CSP `script-src 'self'` 下全部会被拦）
const INLINE_HANDLERS: &[&str] = &[
    "onclick=",
    "onchange=",
    "oninput=",
    "onsubmit=",
    "onload=",
    "onerror=",
    "onkeydown=",
    "onkeyup=",
    "onmouseover=",
    "onfocus=",
    "onblur=",
    "onselect=",
    "ondblclick=",
];

/// 直接按字段造响应：`Resp::html` 是 router 模块私有的构造器，
/// 这里只需要 content_type 与 body，字段本身是 pub
fn resp_with_type(content_type: &str, body: &str) -> Resp {
    Resp {
        status: 200,
        content_type: content_type.to_string(),
        cache_control: "no-store",
        body: body.as_bytes().to_vec(),
        extra_headers: Vec::new(),
    }
}

#[test]
fn html_responses_carry_csp_header() {
    let resp = to_http_response(resp_with_type("text/html; charset=utf-8", "<p>hi</p>"));
    let got = resp
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(got, CSP_POLICY, "HTML 响应必须带与配置一致的 CSP");
    // htmx 片段同样是 HTML：统一加头（不区分整页/片段）
    let frag = to_http_response(resp_with_type("text/html; charset=utf-8", "<div>x</div>"));
    assert!(frag.headers().contains_key("content-security-policy"));
}

#[test]
fn non_html_responses_skip_csp() {
    let json = to_http_response(resp_with_type("application/json; charset=utf-8", "{}"));
    assert!(
        !json.headers().contains_key("content-security-policy"),
        "JSON 响应不需要 CSP（且会干扰非文档消费方）"
    );
}

/// CSP 两处定义（配置 + 响应头常量）必须一致：只改一处等于线上策略漂移
#[test]
fn csp_header_matches_tauri_conf() {
    let conf: serde_json::Value =
        serde_json::from_str(include_str!("../../tauri.conf.json")).expect("tauri.conf.json 合法");
    let csp = conf["app"]["security"]["csp"]
        .as_str()
        .expect("tauri.conf.json 必须配置 app.security.csp");
    assert_eq!(
        csp, CSP_POLICY,
        "tauri.conf.json 的 csp 与 ui::CSP_POLICY 不一致"
    );
}

/// `withGlobalTauri` 仍为 true：app.js 依赖 `window.__TAURI__.dialog`
/// （无 Node 构建链，不能用 ESM 导入）。这一条不是「越权」，而是把
/// 依赖写成断言——哪天真的改成 ESM 了，这条会提醒一起关掉全局对象。
#[test]
fn global_tauri_flag_matches_frontend_usage() {
    let conf: serde_json::Value =
        serde_json::from_str(include_str!("../../tauri.conf.json")).expect("tauri.conf.json 合法");
    let global = conf["app"]["withGlobalTauri"].as_bool().unwrap_or(false);
    let app_js = include_str!("../../../ui-dist/app.js");
    let uses_global = app_js.contains("window.__TAURI__");
    assert_eq!(
        global, uses_global,
        "withGlobalTauri={global} 但 app.js 依赖 window.__TAURI__ = {uses_global}"
    );
}

/// 在 HTML 标签内找内联事件属性（`<button ... onclick="...">`）。
/// 不做整串 contains：注释里提到 `onclick="..."` 是允许的（本文件与 app.js
/// 都有这类说明文字），只有真的写在标签属性位置才会被 CSP 拦。
fn inline_handler_in_markup(src: &str) -> Option<(String, usize)> {
    for name in INLINE_HANDLERS {
        let mut from = 0;
        while let Some(off) = src[from..].find(name) {
            let i = from + off;
            let before = &src[..i];
            let in_tag = match (before.rfind('<'), before.rfind('>')) {
                (Some(lt), Some(gt)) => lt > gt,
                (Some(_), None) => true,
                _ => false,
            };
            if in_tag {
                return Some((name.to_string(), i));
            }
            from = i + name.len();
        }
    }
    None
}

#[test]
fn templates_have_no_inline_event_handlers() {
    for (name, src) in TEMPLATES {
        if let Some((pat, at)) = inline_handler_in_markup(src) {
            panic!(
                "模板 {name} 第 {} 个字节处含内联事件属性 {pat}：CSP script-src 'self' 会拦掉它，改用 data-mbx-action",
                at
            );
        }
    }
}

#[test]
fn frontend_scripts_have_no_inline_event_handlers() {
    // app.js 会用 innerHTML 拼列表行：同样是内联属性，同样会被 CSP 拦
    let app_js = include_str!("../../../ui-dist/app.js");
    if let Some((pat, at)) = inline_handler_in_markup(app_js) {
        panic!("app.js 第 {at} 个字节处含内联事件属性 {pat}：改用 data-mbx-action 委托");
    }
}
