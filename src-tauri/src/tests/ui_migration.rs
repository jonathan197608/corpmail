//! 迁移模态的前端静态护栏（审计 A1/A2/A3）。
//!
//! 现有 E2E 一律用 `htmxAjax` 直发 `/settings/storage/move` 并断言响应文本，
//! 绕过 `mbxMoveDataDir` 的点击链——A1/A2 两个死锁就是这样漏网的。完整点击式
//! spec 在 `e2e/specs/90-failure.spec.mjs` F3b；这里用不依赖 GUI 的源码断言
//! 钉住「响应必须被消费」「[hidden] 必须显式补」「事件名前后端一致」三件事。

use super::MIGRATION_BACKGROUND_HEADER;

const APP_JS: &str = include_str!("../../../ui-dist/app.js");
const APP_CSS: &str = include_str!("../../../ui-dist/app.css");
const SETTINGS_RS: &str = include_str!("../ui/settings.rs");
const LIB_RS: &str = include_str!("../lib.rs");

#[test]
fn migration_start_response_is_fully_consumed() {
    // fetch 的响应必须被读取：只读 status 或不读都会让校验失败/同步完成卡在模态里
    assert!(APP_JS.contains("await res.text()"), "必须读取响应体");
    assert!(APP_JS.contains("mbxMigApplyPane(text)"), "HTML pane 必须落到页面");
    assert!(
        APP_JS.contains("res.headers.get('X-CorpMail-Migration')"),
        "必须按后台标记分支，不能一律等事件"
    );
    assert!(
        SETTINGS_RS.contains("MIGRATION_BACKGROUND_HEADER"),
        "后端必须给 Background 分支打标记"
    );
    assert_eq!(MIGRATION_BACKGROUND_HEADER, "X-CorpMail-Migration");
    assert!(
        APP_JS.contains("mbxMigCloseModal();\n            return;"),
        "同步完成/校验失败后必须关闭模态"
    );
}

#[test]
fn migration_modal_hidden_rules_are_explicit() {
    // 作者样式 display:flex / inline-flex 压过 UA 的 [hidden]，缺这两条规则
    // 会让「返回」关不掉遮罩（app.css:2646 的同坑注释）
    assert!(APP_CSS.contains(".migrate-mask[hidden]"), "遮罩需显式 [hidden]");
    assert!(
        APP_CSS.contains(".migrate-card .btn[hidden]"),
        "返回按钮需显式 [hidden]"
    );
    assert!(
        APP_JS.contains("mask.hidden = true"),
        "关闭路径必须真的置 hidden"
    );
}

#[test]
fn migration_event_names_match_between_rust_and_frontend() {
    for name in ["migration-progress", "migration-done", "migration-error"] {
        assert!(
            LIB_RS.contains(&format!("app.emit(\"{name}\"")),
            "后端必须 emit {name}"
        );
        assert!(
            APP_JS.contains(&format!("add('{name}'")),
            "前端必须监听 {name}"
        );
    }
}

#[test]
fn migration_unlisten_is_registered_before_first_await() {
    // 注册中途抛错时，已注册的监听也必须可解绑（A14）
    let listen = APP_JS.find("async function mbxMigListen()").expect("监听函数存在");
    let assign = APP_JS[listen..]
        .find("mbxMigUnlisten = function ()")
        .expect("解绑收集器存在");
    let first_await = APP_JS[listen..].find("await add(").expect("存在注册 await");
    assert!(assign < first_await, "解绑收集器必须先于首个 await 赋值");
}

#[test]
fn background_thread_holds_the_migration_guard() {
    // A5：生产后台线程必须持有 RAII guard，否则 panic 会把进行中标志卡死
    assert!(
        LIB_RS.contains("let _guard = s.migration_guard();"),
        "后台线程开头必须取 MigrationGuard"
    );
    assert!(
        LIB_RS.contains("std::thread::Builder::new()"),
        "线程创建失败必须能回传 Err 供 start_migration 复位"
    );
}
