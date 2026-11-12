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
const STATE_RS: &str = include_str!("../state.rs");
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

#[test]
fn storage_pane_uses_cached_attachment_stats() {
    // A9：不能在每个 tab 的 settings_ctx 里重新全目录递归
    assert!(
        !SETTINGS_RS.contains("state.attachments_stats()"),
        "附件占用必须走 storage_stats 缓存"
    );
    assert!(SETTINGS_RS.contains("storage_stats"));
}

#[test]
fn migration_done_renders_backend_message_and_manual_restart_hint() {
    // A10/A11：完整成功文案（新目录 / 旧备份）必须来自 payload；
    // exit 未生效时要有手动重启兜底，不能永久停在 100%
    let handler = APP_JS
        .find("add('migration-done'")
        .expect("done 监听器存在");
    let rest = &APP_JS[handler..];
    let body = &rest[..rest.find("add('migration-error'").expect("error 监听器存在")];
    assert!(body.contains("e.payload"), "必须渲染后端成功文案");
    assert!(body.contains("手动重启"), "必须给出手动重启兜底");
    assert!(
        APP_CSS.contains("white-space: pre-line"),
        "多行成功文案必须保留换行"
    );
}

#[test]
fn migration_exit_stops_background_threads_first() {
    // A10：退出前置停止位，避免与收件/发送/清理线程抢 SQLite 写锁
    let success = LIB_RS.find("Ok(msg) =>").expect("成功分支存在");
    // 用下一个 match 臂做边界：两端都是 find 结果（char boundary），
    // 不能写 success+900——中文注释会把切片切进 UTF-8 字符中间
    let rest = &LIB_RS[success..];
    let body = &rest[..rest.find("Err(e) =>").expect("失败分支存在")];
    assert!(body.contains("request_shutdown();"), "退出前必须置停止位");
    assert!(body.contains("wait_or_stop("), "给前端留渲染时间且可中断");
    assert!(body.contains("app.exit(0);"), "仍要自动退出");
}

#[test]
fn stats_cache_is_invalidated_on_store_swap() {
    // A12：swap_store 换数据目录后 TTL 内旧统计必须立即作废
    let swap = STATE_RS.find("fn swap_store(").expect("swap_store 存在");
    let rest = &STATE_RS[swap..];
    let body = &rest[..rest.find("fn quiesce_fetch_rounds").expect("下一函数存在")];
    assert!(
        body.contains("self.invalidate_stats_cache();"),
        "切库必须失效统计缓存"
    );
    assert!(
        STATE_RS.contains("c.data_dir == dir"),
        "缓存命中必须比对数据目录"
    );
}

#[test]
fn e2e_build_keeps_synchronous_migration_path() {
    // A3 覆盖策略：e2e 不注入 executor（95 冻结 / 阶段二复用依赖同步进程内状态），
    // 生产后台链由 tests/state.rs 的 background_executor_* 单测 + 本文件断言钉住。
    // 若将来给 e2e 也注入 executor，必须先改 95/90 spec 的响应文本断言
    assert!(
        LIB_RS.contains("cfg!(not(feature = \"e2e\"))"),
        "e2e 仍走同步分支；改动时同步调整 E2E spec"
    );
    assert!(
        STATE_RS.contains("fn migration_guard("),
        "生产后台链必须有 RAII guard"
    );
}
