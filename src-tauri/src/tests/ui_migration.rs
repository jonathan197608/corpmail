//! 迁移模态的前端静态护栏（审计 A1/A2/A3）。
//!
//! 现有 E2E 一律用 `htmxAjax` 直发 `/settings/storage/move` 并断言响应文本，
//! 绕过 `mbxMoveDataDir` 的点击链——A1/A2 两个死锁就是这样漏网的。完整点击式
//! spec 在 `e2e/specs/90-failure.spec.mjs` F3b；这里用不依赖 GUI 的源码断言
//! 钉住「响应必须被消费」「[hidden] 必须显式补」「事件名前后端一致」三件事。

use super::MIGRATION_BACKGROUND_HEADER;
use super::{Resp, to_http_response};

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
fn seen_worker_respects_migration_freeze() {
    // R14：seen worker 是唯一不查冻结位的常驻线程——冻结期连 IMAP 并写旧库，
    // 重启后这次回写丢失（幂等，但白连一次）。必须跳过且不清标志
    let at = LIB_RS.find("fn spawn_seen_worker").expect("seen worker 存在");
    let rest = &LIB_RS[at..];
    let body = &rest[..rest
        .find("fn spawn_fetch_scheduler")
        .expect("下一函数存在")];
    let guard = body
        .find("if state.migration_pending()")
        .expect("必须查冻结位");
    let flush = body.find("mail::seen::flush(").expect("flush 调用存在");
    assert!(guard < flush, "冻结判定必须在 flush 之前");
    assert!(
        body.contains("跳过本轮已读回写"),
        "跳过要留痕（便于线上定位）"
    );
}

#[test]
fn progress_phase_has_no_dead_done_branch() {
    // R17：后端从不 emit phase="done"（100% 由 migration-done 直写），
    // 前端留 `case 'done'` 只会误导后续改动
    let at = APP_JS
        .find("function mbxMigPhasePct(")
        .expect("阶段换算函数存在");
    let rest = &APP_JS[at..];
    let body = &rest[..rest.find("// 失败态").expect("下一段存在")];
    assert!(!body.contains("'done'"), "不得再有 done 死分支");
    for phase in ["attach", "snapshot", "attachments", "pointer"] {
        assert!(body.contains(phase), "阶段 {phase} 必须仍被映射");
    }
}

#[test]
fn snapshot_does_not_hold_the_store_lock() {
    // R13：VACUUM INTO 曾在 with_conn 闭包里跑——快照期间全局 store 锁被独占，
    // 状态条轮询/estimate/边栏计数全部排队。只读连接快照必须留在锁外
    let at = STATE_RS
        .find("fn migrate_snapshot(")
        .expect("migrate_snapshot 存在");
    let rest = &STATE_RS[at..];
    let body = &rest[..rest
        .find("fn switch_data_dir_first_run(")
        .expect("下一函数存在")];
    assert!(
        body.contains("snapshot_db(&cur.join(\"corpmail.db\"), &new_db)"),
        "快照必须走独立连接的 snapshot_db"
    );
    // 匹配调用形态而非裸词：注释里讨论「VACUUM INTO」是允许的（本文件踩过
    // 「负向断言被自己的注释触发」的坑）
    assert!(
        !body.contains("execute(\"VACUUM INTO"),
        "VACUUM 不得回到 with_conn 借出的连接上（会重新独占 store 锁）"
    );
    assert!(
        STATE_RS.contains("SQLITE_OPEN_READ_ONLY"),
        "快照连接必须是只读标志"
    );
}

#[test]
fn migration_modal_has_stall_watchdog() {
    // R10：后端 panic 或被不可靠挂载卡死时前端收不到任何事件。旧实现只有三个
    // 事件处理器、没有定时器，模态会永久停在最后一帧，用户只能强杀。
    assert!(
        APP_JS.contains("MBX_MIG_STALL_MS"),
        "无进展阈值必须是具名常量"
    );
    assert!(
        APP_JS.contains("setInterval(") && APP_JS.contains("mbxMigWatchdog"),
        "必须起看门狗定时器"
    );
    assert!(
        APP_JS.contains("MBX_MIG_SNAPSHOT_STALL_MS = 1800000"),
        "快照阶段要单独放宽阈值到 30 分钟：VACUUM 是一次性阻塞、期间零事件，\
         且机械盘/网络盘写入过慢时远超常速估计，60s 甚至 5 分钟都会自伤"
    );
    assert!(
        APP_JS.contains("长时间无进展"),
        "超时必须切到可关闭的失败态并说明后台仍在跑"
    );
    let progress = APP_JS
        .find("add('migration-progress'")
        .expect("progress 监听存在");
    let rest = &APP_JS[progress..];
    assert!(
        rest.contains("mbxMigLastEvent = Date.now();"),
        "每个事件都要重置看门狗计时"
    );
    // 后端兜底：panic 不走 Err 分支，必须 catch_unwind 后 emit migration-error
    assert!(
        LIB_RS.contains("std::panic::catch_unwind("),
        "后台迁移线程必须捕获 panic"
    );
    assert!(
        LIB_RS.contains("panic_text(payload.as_ref())"),
        "panic 必须转成可显示的迁移错误"
    );
    assert!(
        LIB_RS.contains("fn panic_text("),
        "panic payload 提取必须抽成函数（&str/String 两种形态）"
    );
}

#[test]
fn migration_terminal_events_hide_stall_actions() {
    // C4：2b94399 的 30 分钟确认态只在 migration-progress 收起按钮。
    // done / error 到达后「继续等待」仍可点：错误态点它会覆盖错误文案并重启
    // 30 分钟看门狗，把已终止的线程伪装成仍在跑。
    let done = APP_JS
        .find("add('migration-done'")
        .expect("done 监听存在");
    let dbody = &APP_JS[done..];
    let dbody = &dbody[..dbody.find("add('migration-error'").expect("error 监听存在")];
    assert!(
        dbody.contains("mbxMigHideActions()"),
        "成功终态必须收起确认态按钮（100% 上不该还挂着「继续等待 / 返回」）"
    );

    let err = APP_JS
        .find("function mbxMigShowError(")
        .expect("mbxMigShowError 存在");
    let ebody = &APP_JS[err..];
    let ebody = &ebody[..ebody.find("// 关闭迁移模态").expect("下一段注释存在")];
    assert!(
        ebody.contains("mbxMigHideActions()"),
        "失败态必须先收起「继续等待」再显示「返回」"
    );

    let hide = APP_JS
        .find("function mbxMigHideActions()")
        .expect("mbxMigHideActions 存在");
    let hbody = &APP_JS[hide..];
    let hbody = &hbody[..hbody.find("// 失败态").expect("下一段注释存在")];
    assert!(
        hbody.contains("migration-keep-waiting") && hbody.contains("keep.hidden = true"),
        "复位函数必须真的隐藏「继续等待」"
    );
}

#[test]
fn migration_snapshot_stall_asks_user_before_failing() {
    // R10 收紧：快照是单次阻塞调用，磁盘过慢时 30 分钟也可能不够。超时不得直接
    // 判死切失败态——必须提醒用户，由用户决定是否继续等下一个 30 分钟。
    // （直接判死 = 把仍在正常写的慢盘迁移显示成失败，用户会去重启/重试，本次迁移作废）
    assert!(
        APP_JS.contains("function mbxMigPromptStall()"),
        "快照超时必须有独立的确认态函数（走失败态就错了）"
    );
    let prompt = APP_JS
        .find("function mbxMigPromptStall()")
        .expect("mbxMigPromptStall 存在");
    let pbody = &APP_JS[prompt..];
    let pbody = &pbody[..pbody.find("function mbxMigKeepWaiting()").expect("下一函数存在")];
    assert!(
        pbody.contains("超过 30 分钟") && pbody.contains("继续等待"),
        "确认文案要写清超时时长与两个出口"
    );
    assert!(
        pbody.contains("关闭窗口不会中止"),
        "必须说明关窗不中止后台迁移，别让用户以为点「返回」就是取消"
    );
    // 「继续等待」按钮存在且默认隐藏（只在确认态出现）
    assert!(
        APP_JS.contains("data-mbx-action=\"migration-keep-waiting\""),
        "模态骨架要有「继续等待」按钮"
    );
    // 点击「继续等待」必须复位计时并重启看门狗（否则一次确认后看门狗永久失效）
    let keep = APP_JS
        .find("function mbxMigKeepWaiting()")
        .expect("mbxMigKeepWaiting 存在");
    let kbody = &APP_JS[keep..];
    let kbody = &kbody[..kbody
        .find("async function mbxPickSetupDir(")
        .expect("下一函数存在")];
    assert!(
        kbody.contains("mbxMigLastEvent = Date.now();"),
        "继续等待必须复位无进展计时"
    );
    assert!(
        kbody.contains("mbxMigStartWatchdog();"),
        "继续等待必须重启看门狗（30 分钟后再问一次）"
    );
    // 委托分发接上
    assert!(
        APP_JS.contains("case 'migration-keep-waiting': mbxMigKeepWaiting();"),
        "data-mbx-action 委托必须分发 migration-keep-waiting"
    );
    // 看门狗核心（C19）：阈值与分派集中在不碰 DOM 的一段，由 Node vm 行为测试
    // 真正执行；这里只钉住「这段存在且两个出口都接上」
    let core_start = APP_JS
        .find("// --- C19 watchdog core START ---")
        .expect("看门狗核心块存在");
    let core = &APP_JS[core_start..];
    let core = &core[..core
        .find("// --- C19 watchdog core END ---")
        .expect("核心块结束标记存在")];
    assert!(
        core.contains("function mbxMigStallDecision("),
        "超时判定必须抽成纯函数，便于行为测试"
    );
    assert!(core.contains("mbxMigPromptStall();"), "快照超时走确认态");
    assert!(
        core.contains("mbxMigShowError("),
        "非快照阶段超时仍走失败态（60s 卡死判定不变）"
    );
    // 阶段推进后确认按钮必须收起（提醒失效，避免误点重置计时）
    let progress = APP_JS
        .find("add('migration-progress'")
        .expect("progress 监听存在");
    let pbody2 = &APP_JS[progress..];
    let pbody2 = &pbody2[..pbody2.find("add('migration-done'").expect("done 监听存在")];
    assert!(
        pbody2.contains("keep.hidden = true"),
        "收到新进度事件必须收起「继续等待」按钮"
    );
}

#[test]
fn extra_headers_reach_the_http_response() {
    // R12：A1 契约三段是 ① settings.rs 塞 extra_headers → ② to_http_response
    // 写进协议层响应 → ③ app.js 读头分支。R5 的用例只测到 ①，
    // 若 ② 的循环被重构掉，R5 仍绿而生产直接复现 A1 死锁（模态等一个永不到来的事件）
    let resp = Resp {
        status: 200,
        content_type: "text/html; charset=utf-8".into(),
        cache_control: "no-store",
        body: b"<div id=\"settings-pane\"></div>".to_vec(),
        extra_headers: vec![(MIGRATION_BACKGROUND_HEADER.to_string(), "background".to_string())],
    };
    let http = to_http_response(resp);
    assert_eq!(
        http.headers()
            .get(MIGRATION_BACKGROUND_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("background"),
        "附加头必须真的落到 HTTP 响应上"
    );
    assert!(
        http.headers().contains_key("content-security-policy"),
        "附加头不得挤掉 CSP（同一 builder 上顺序追加）"
    );
    // 非 HTML 响应同样透传：该标记与内容类型无关
    let plain = to_http_response(Resp {
        status: 409,
        content_type: "text/plain; charset=utf-8".into(),
        cache_control: "no-store",
        body: b"x".to_vec(),
        extra_headers: vec![(MIGRATION_BACKGROUND_HEADER.to_string(), "background".to_string())],
    });
    assert_eq!(
        plain
            .headers()
            .get(MIGRATION_BACKGROUND_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("background")
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
    assert!(body.contains("mbxMigHideActions()"), "打开时必须统一复位按钮");
    // C4：复位函数本体必须真的隐藏两个按钮（「调用了一个空函数」不算修好）
    let hide = APP_JS
        .find("function mbxMigHideActions()")
        .expect("mbxMigHideActions 存在");
    let hbody = &APP_JS[hide..];
    let hbody = &hbody[..hbody.find("// 失败态").expect("下一段注释存在")];
    assert!(
        hbody.contains("keep.hidden = true") && hbody.contains("back.hidden = true"),
        "「返回」与「继续等待」都必须重新隐藏"
    );

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
