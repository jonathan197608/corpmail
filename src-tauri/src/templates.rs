//! 模板环境（minijinja 3 运行时渲染，沿用 toolkit_web 已验证封装）
//!
//! 模板以 `include_str!` 编译期内嵌，离线可用；
//! 自动转义按 `.html` 扩展名开启，防 XSS。

use minijinja::Environment;
use minijinja::value::Serde;
use std::sync::LazyLock;

/// 注册的全部模板名（测试也按此表断言）
const TEMPLATES: &[(&str, &str)] = &[
    // 统一 SVG 图标宏（供各模板 {% import "icons.html" as ic %}）
    ("icons.html", include_str!("templates/icons.html")),
    ("base.html", include_str!("templates/base.html")),
    ("welcome.html", include_str!("templates/welcome.html")),
    ("setup.html", include_str!("templates/setup.html")),
    ("done.html", include_str!("templates/done.html")),
    ("main.html", include_str!("templates/main.html")),
    ("mail_list.html", include_str!("templates/mail_list.html")),
    // 列表行片段（审计 R41）：后台刷新只回邮件行，不覆盖顶栏搜索/筛选状态
    (
        "mail_list_rows.html",
        include_str!("templates/mail_list_rows.html"),
    ),
    ("email.html", include_str!("templates/email.html")),
    ("compose.html", include_str!("templates/compose.html")),
    ("side.html", include_str!("templates/side.html")),
    ("settings.html", include_str!("templates/settings.html")),
    (
        "settings_pane_account.html",
        include_str!("templates/settings_pane_account.html"),
    ),
    (
        "settings_pane_sync.html",
        include_str!("templates/settings_pane_sync.html"),
    ),
    (
        "settings_pane_storage.html",
        include_str!("templates/settings_pane_storage.html"),
    ),
    (
        "settings_pane_about.html",
        include_str!("templates/settings_pane_about.html"),
    ),
    (
        "settings_pane_templates.html",
        include_str!("templates/settings_pane_templates.html"),
    ),
    (
        "test_result.html",
        include_str!("templates/test_result.html"),
    ),
    (
        "fetch_status.html",
        include_str!("templates/fetch_status.html"),
    ),
    ("thread.html", include_str!("templates/thread.html")),
    ("contacts.html", include_str!("templates/contacts.html")),
    (
        "contacts_area.html",
        include_str!("templates/contacts_area.html"),
    ),
    // A5 源码排查页（HTML/纯文本/原始信头）
    ("source.html", include_str!("templates/source.html")),
];

/// JSON 字符串字面量编码（内联 JS 属性用）：双引号包裹 + 全量转义，
/// 任何字符（含单引号/换行/反斜杠/代理对）都不会破坏外层单引号属性
fn js_str(s: String) -> String {
    serde_json::to_string(&s).unwrap_or_else(|_| "\"\"".to_string())
}

/// 文件大小人性化：按 1024 进制自动选 B/KB/MB/GB/TB，非 B 单位保留 1 位小数。
/// 模板过滤器（`{{ n | fmt_size }}`）与设置页 human_bytes 共用，保证全局一致。
pub(crate) fn fmt_size(bytes: i64) -> String {
    if bytes < 0 {
        return "0 B".into();
    }
    const K: i64 = 1024;
    const M: i64 = 1024 * 1024;
    const G: i64 = 1024 * 1024 * 1024;
    const T: i64 = 1024 * 1024 * 1024 * 1024;
    let (val, unit) = if bytes >= T {
        (bytes as f64 / T as f64, "TB")
    } else if bytes >= G {
        (bytes as f64 / G as f64, "GB")
    } else if bytes >= M {
        (bytes as f64 / M as f64, "MB")
    } else if bytes >= K {
        (bytes as f64 / K as f64, "KB")
    } else {
        (bytes as f64, "B")
    };
    if unit == "B" {
        format!("{val:.0} B")
    } else {
        format!("{val:.1} {unit}")
    }
}

fn build_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_auto_escape_callback(minijinja::default_auto_escape_callback);
    env.add_filter("js_str", js_str);
    env.add_filter("fmt_size", fmt_size);
    for (name, src) in TEMPLATES {
        // include_str 内嵌的模板编译失败属于程序 bug（模板语法错误），直接 panic 暴露
        env.add_template(name, src).expect("模板语法有误");
    }
    env
}

/// 全局模板环境（模板加载后不可变）
static ENV: LazyLock<Environment<'static>> = LazyLock::new(build_env);

/// 渲染命名模板，返回 HTML 字符串
pub fn render(name: &str, ctx: &impl serde::Serialize) -> Result<String, minijinja::Error> {
    // minijinja 3：get_template 取已注册模板后 .render(ctx)，ctx 经 Serde 包装转 Value
    ENV.get_template(name)?
        .render(minijinja::Value::from(Serde(ctx)))
}

#[cfg(test)]
#[path = "tests/templates.rs"]
mod tests;
