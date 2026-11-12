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
const ROUTER_RS: &str = include_str!("../ui/router.rs");
const STATE_RS: &str = include_str!("../state.rs");
const LIB_RS: &str = include_str!("../lib.rs");
const STORAGE_TPL: &str = include_str!("../templates/settings_pane_storage.html");

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
    // R1（复查审计）：成功分支不得直接调 request_shutdown——SHUTDOWN 无复位
    // 路径，exit(0) 未生效时提前置位会让常驻线程停在检查点不再恢复（僵尸态）。
    // 停止位唯一合法来源是 app.run 的 ExitRequested|Exit（R62 已有）。
    let success = LIB_RS.find("Ok(msg) =>").expect("成功分支存在");
    // 用下一个 match 臂做边界：两端都是 find 结果（char boundary），
    // 不能写 success+900——中文注释会把切片切进 UTF-8 字符中间
    let rest = &LIB_RS[success..];
    let body = &rest[..rest.find("Err(e) =>").expect("失败分支存在")];
    assert!(
        !body.contains("request_shutdown();"),
        "迁移成功分支不得手动置停止位（交给退出事件）"
    );
    assert!(body.contains("wait_or_stop("), "给前端留渲染时间且可中断");
    assert!(body.contains("app.exit(0);"), "仍要自动退出");
    // 退出事件里的置位必须存在（唯一的合法置位点）
    let run = LIB_RS.find("app.run(").expect("事件循环存在");
    let run_rest = &LIB_RS[run..];
    let run_body = &run_rest[..run_rest.find("Reopen").unwrap_or(run_rest.len())];
    assert!(
        run_body.contains("request_shutdown();"),
        "ExitRequested|Exit 必须置停止位（R62）"
    );
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
fn production_migration_runs_through_injected_executor() {
    // R6（复查审计）：原断言要求 `cfg!(not(feature = "e2e"))` 必须存在，
    // 等于把「A3 未修（e2e 只能同步）」固化成测试保护，与审计方向相反。
    // 改为钉住生产契约本身：执行器注入点存在、跑在后台线程、成功回传 done 后退出；
    // 将来给 e2e 也注入 executor 时本测试不会挡路
    assert!(
        LIB_RS.contains("set_migration_executor("),
        "生产必须注入迁移执行器"
    );
    assert!(
        LIB_RS.contains("std::thread::Builder::new()"),
        "执行器必须在后台线程执行（协议回调不阻塞）"
    );
    assert!(
        LIB_RS.contains("app.emit(\"migration-done\""),
        "成功分支必须回传 done 事件"
    );
    assert!(
        LIB_RS.contains("app.exit(0);"),
        "成功分支必须自动退出"
    );
    assert!(
        STATE_RS.contains("fn migration_guard("),
        "生产后台链必须有 RAII guard"
    );
}

#[test]
fn migration_phase_reaches_storage_pane() {
    // R9：后台迁移期间 migration_pending 也为 true，模板若只看它就会宣告
    // 「已迁移，请重启」——必须另有 migration_running 分支（稍候文案）
    assert!(
        SETTINGS_RS.contains("migration_running:"),
        "SettingsCtx 必须携带进行中标志"
    );
    assert!(
        STORAGE_TPL.contains("{% if migration_running %}"),
        "存储页 banner 必须先判进行中"
    );
    let running = STORAGE_TPL
        .find("{% if migration_running %}")
        .expect("running 分支存在");
    let pending = STORAGE_TPL
        .find("{% elif migration_pending %}")
        .expect("pending 分支存在");
    assert!(running < pending, "进行中分支必须排在待重启之前");
    assert!(
        STORAGE_TPL.matches("{% if migration_running %}").count() >= 3,
        "banner 与「更换位置」「立即清理」两处禁用提示都要区分阶段"
    );
}

#[test]
fn migration_modal_resets_previous_failure_state() {
    // R11：复用遮罩时必须复位上一轮的进度条/文案/「返回」按钮，
    // 否则二次发起会带着错误态进场，用户一点就关掉正在跑的迁移遮罩
    let open = APP_JS
        .find("function mbxMigOpen()")
        .expect("mbxMigOpen 存在");
    let rest = &APP_JS[open..];
    let body = &rest[..rest.find("// 更新进度条").expect("下一段注释存在")];
    assert!(body.contains("bar.style.width = '0%'"), "进度条必须归零");
    assert!(
        body.contains("detail.textContent = '准备迁移…'"),
        "详情必须复位为初始文案"
    );
    assert!(body.contains("btn.hidden = true"), "「返回」必须重新隐藏");

    // R11 附带：关模态先解绑监听再判遮罩，避免早退导致监听泄漏
    let close = APP_JS
        .find("function mbxMigCloseModal()")
        .expect("mbxMigCloseModal 存在");
    let cbody = &APP_JS[close..];
    let cbody = &cbody[..cbody.find("async function mbxMigListen()").expect("下一函数存在")];
    let unlisten = cbody
        .find("if (mbxMigUnlisten)")
        .expect("解绑段存在");
    let guard = cbody.find("if (!mask) return;").expect("遮罩判空存在");
    assert!(unlisten < guard, "必须先解绑监听再判遮罩");
}

#[test]
fn wizard_path_does_not_call_migration_directly() {
    // R2（复查审计）：向导是协议回调线程，路由层不得再直呼同步迁移
    // （大库 VACUUM 会卡死「保存中…」）。唯一的迁移入口是 settings.rs 的
    // start_migration（后台 executor 链）。匹配调用形态 `.move_data_dir(`
    // 而非裸标识符——注释里允许讨论它
    assert!(
        !ROUTER_RS.contains(".move_data_dir("),
        "router.rs 不得直接调用同步迁移；向导走引导文案，迁移走设置页"
    );
    assert!(
        ROUTER_RS.contains("存储管理"),
        "向导引导文案必须指向 设置 → 存储管理"
    );
}
