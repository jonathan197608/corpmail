// A4/A10/A11/A17（audit 2026-11-16）：迁移模态失败态 / 确认取消链路的行为测试。
//
// 旧实现三条迁移启动失败分支（409 / fetch 异常 / pane 应用失败）只改文案不清理：
// 看门狗按 starting 阶段 30 分钟后把 409 覆盖成「仍在启动（继续等待）」。
// 静态断言看不出时序问题，这里把 app.js 的 A4 migration modal core 放进
// Node vm，用假 DOM + 假时钟驱动真实分支。
//
// 运行：node --test e2e/unit/migration-modal.test.mjs（e2e/run.sh 0/6 步自动跑）
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- A4 migration modal core START ---';
const END = '// --- A4 migration modal core END ---';

function core() {
    const start = APP_JS.indexOf(START);
    const end = APP_JS.indexOf(END);
    assert.ok(start >= 0 && end > start, 'app.js 必须带 A4 migration modal core 锚点');
    return APP_JS.slice(start, end);
}

// A4 锚点内引用（未在锚点内声明）的外部符号，由 vm 全局提供
function extGlobals() {
    const kept = {};
    for (const name of [
        'mbxFlushComposeDraft', 'mbxMigAfterFlush', 'mbxToast',
        'mbxMigSet', 'mbxMigPhasePct', 'mbxMigHideActions', 'mbxConfirmLeave',
        'mbxModalOpen', 'rebaselineAll', 'mbxPaneDirty',
    ]) kept[name] = null;
    return kept;
}

/// 假 DOM：只对象迁移模态链路用到的 API（getElementById / querySelector / 事件）
function fakeDom() {
    const mk = (id) => ({
        id, hidden: false, children: [],
        dataset: {}, style: {},
        _listeners: {},
        addEventListener(n, f) { (this._listeners[n] = this._listeners[n] || []).push(f); },
        removeEventListener() {},
        dispatchEvent() {},
        querySelector: () => null,
        setAttribute() {}, getAttribute: () => null, hasAttribute: () => false,
        removeAttribute() {},
        appendChild(c) { this.children.push(c); return c; },
        remove() {},
        focus() {},
        textContent: '', innerHTML: '',
    });
    const mask = mk('migrate-mask');
    const detail = mk('migrate-detail');
    const bar = mk('migrate-bar');
    const settingsPane = mk('settings-pane');
    const byId = { 'migrate-mask': mask, 'migrate-detail': detail, 'migrate-bar': bar, 'settings-pane': settingsPane };
    const body = mk('body');
    // H2：mask 的按钮桩必须持久（同一 selector 每次返回同一对象），
    // 否则 mbxMigHideActions 设的 hidden 无法被后续断言观测；
    // 原桩 querySelector 恒 null → hideActions/showError 的按钮操作是 no-op。
    const buttons = {
        keep: mk('button-keep-waiting'),
        retry: mk('button-migration-retry'), // R21：失败态「重新选择目录并重试」也须可观测
        close: mk('button-close'),
        card: mk('div-migrate-card'),
    };
    mask.querySelector = (sel) => {
        if (sel === '.migrate-card') return buttons.card;
        if (sel.includes('migration-keep-waiting')) return buttons.keep;
        if (sel.includes('migration-retry')) return buttons.retry;
        if (sel.includes('migration-close')) return buttons.close;
        return null;
    };
    return {
        document: {
            getElementById: (id) => byId[id] || null,
            // 迁移前置确认会读 .dir-path 取当前目录做文案；桩缺此方法会在
            // 确认前抛 TypeError，造成「确认没弹但 POST 也没发」的假绿
            querySelector: () => null,
            createElement: (tag) => mk(tag),
            activeElement: null,
            body,
        },
        __dom: { mask, detail, bar, settingsPane, body, buttons },
    };
}

/// 在 vm 里执行 core，并注入可编程的 fetch / Tauri event / 时钟
function harness({ fetchImpl } = {}) {
    const dom = fakeDom();
    const intervals = [];
    let now = 0;
    const unlistenCalls = [];
    const listeners = {};
    const toasts = [];
    // H4：焦点陷阱释放计数——mbxModalOpen 返回的 release 被调用几次
    let releaseCalls = 0;
    // 默认自动确认：本 harness 的四条 A4 用例聚焦确认之后的 409 / 异常 /
    // background / 空响应分支；迁移前置确认本身由下面的独立用例验证。
    let confirmLeave = (msg, onLeave, ok, cancel, onCancel) => {
        if (onLeave) onLeave();
    };
    // 锚点外的前置函数：桩化（聚焦 A4 本身的行为，不重复测 flush 链路）
    const confirmLeaves = [];
    const ctx = vm.createContext({
        ...dom,
        ...extGlobals(),
        mbxFlushComposeDraft: async () => ({ ok: true }),
        mbxMigAfterFlush: (flushed, hooks) => (hooks && hooks.proceed ? hooks.proceed() : true),
        mbxToast: () => {},
        mbxMigSet: () => {},
        mbxMigPhasePct: () => 10,
        mbxMigHideActions: () => {},
        mbxModalOpen: () => () => { releaseCalls++; },
        CustomEvent: class { constructor(t, o) { this.type = t; this.detail = o && o.detail; } },
        __TAURI__: {
            dialog: { open: async () => '/tmp/dest' },
            // 保存处理器供测试注入迟到事件（P1-1：error 后的 progress 不得收起出口）
            event: {
                listen: async (name, fn) => {
                    (listeners[name] = listeners[name] || []).push(fn);
                    return () => unlistenCalls.push(name);
                },
            },
        },
        URLSearchParams: class { constructor() {} toString() { return ''; } },
        fetch: fetchImpl || (async () => ({ ok: true, status: 200, headers: { get: () => null }, text: async () => '' })),
        htmx: { process: () => {}, ajax: async () => {} },
        setInterval: (fn, ms) => {
            const handle = { fn, ms };
            intervals.push(handle);
            return handle;
        },
        clearInterval: (id) => {
            // 真浏览器语义：clear 后回调不再触发（harness 曾只计数、回调仍活，
            // 导致「看门狗已停」的用例被 fake 的僵尸 interval 反咬）
            if (id && typeof id === 'object') id.fn = null;
            else for (const it of intervals) it.fn = null;
        },
        Date: { now: () => now },
        setTimeout: (fn) => { timeouts.push(fn); return 0; },
        console: { error: () => {}, log: () => {} },
    });
    const timeouts = [];
    // mbxConfirmLeave 在锚点外：替换为可断言的桩（A10 用真实取消语义驱动）
    let confirmCalls = 0;
    ctx.mbxConfirmLeave = (msg, onLeave, ok, cancel, onCancel) => {
        confirmCalls++;
        return confirmLeave(msg, onLeave, ok, cancel, onCancel);
    };
    ctx.mbxMigCloseModal = null;
    vm.runInContext(core(), ctx);
    // window.__TAURI__ 引用同一个对象（core 源码经 window 前缀访问）
    vm.runInContext('var window = this;', ctx);
    ctx.window.__TAURI__ = ctx.__TAURI__;
    return {
        ctx, timeouts, toasts, unlistenCalls, intervals,
        setNow: (v) => { now = v; },
        dom: dom.__dom,
        releaseCalls: () => releaseCalls,
        confirmCalls: () => confirmCalls,
        run: () => ctx.mbxMoveDataDir(),
        runPrecheck: (ev) => ctx.mbxComposePrecheck(ev),
        runConfirmReset: (ev) => ctx.mbxComposeConfirmReset(ev),
        setConfirmLeave: (f) => { confirmLeave = f; },
        fireTicks: (n = 1) => {
            for (const it of intervals) if (it.fn) for (let i = 0; i < n; i++) it.fn();
        },
        intervalCount: () => intervals.filter((i) => i.fn).length,
        fire: (name, payload) => {
            for (const fn of listeners[name] || []) fn({ payload });
        },
    };
}

test('入口修复：迁移前置确认取消不发起移动，确认后才发起', async () => {
    let calls = 0;
    const okResp = async () => {
        calls++;
        return {
            ok: true, status: 200,
            headers: { get: (k) => (k === 'X-CorpMail-Migration' ? 'background' : null) },
            text: async () => '',
        };
    };
    const cancelled = harness({ fetchImpl: okResp });
    cancelled.setConfirmLeave((msg, onLeave, ok, cancel, onCancel) => {
        if (onCancel) onCancel();
    });
    await cancelled.run();
    assert.equal(cancelled.confirmCalls(), 1, '选完目录后必须弹出前置确认');
    assert.equal(calls, 0, '取消前置确认后不得发起迁移 POST');
    assert.equal(vm.runInContext('mbxMigWatchdog', cancelled.ctx), null, '取消后不应留看门狗');

    const confirmed = harness({ fetchImpl: okResp });
    await confirmed.run();
    assert.equal(confirmed.confirmCalls(), 1, '确认路径也必须弹出前置确认');
    assert.equal(calls, 1, '确认后必须发起迁移 POST');
    assert.ok(vm.runInContext('mbxMigUnlisten', confirmed.ctx), '确认后应进入后台监听');
});

test('A4：409 启动失败停看门狗/解绑监听/释放焦点陷阱，30 分钟后无「继续等待」', async () => {
    const h = harness({
        fetchImpl: async () => ({
            ok: false, status: 409,
            headers: { get: () => null },
            text: async () => '迁移冻结中：已有一次迁移待重启生效',
        }),
    });
    await h.run();
    // 顶层 let 不挂到 context：经 eval 读值
    const watchdog = vm.runInContext('mbxMigWatchdog', h.ctx);
    const unlisten = vm.runInContext('mbxMigUnlisten', h.ctx);
    // 三态全清：看门狗 interval 停、Tauri 监听解绑、焦点陷阱释放
    assert.equal(watchdog, null, '看门狗必须停');
    assert.equal(unlisten, null, '监听必须解绑');
    // H4：abort 必须释放焦点陷阱（删 app.js:515 的 __mbxRelease 时此断言 fail）
    assert.ok(h.releaseCalls() >= 1, '409 分支必须释放焦点陷阱（调用 __mbxRelease）');
    // 假时钟拨 30 分+：没有任何 interval 在跑 → 不会出现「继续等待」
    h.setNow(31 * 60 * 1000);
    h.fireTicks(5);
    assert.ok(
        h.dom.detail.textContent.includes('迁移冻结中') || h.dom.detail.textContent.includes('409'),
        `失败文案应保留（实际：${h.dom.detail.textContent}）`,
    );
    assert.ok(
        !h.dom.detail.textContent.includes('继续等待'),
        '失败文案不得被看门狗覆盖成「继续等待」',
    );
});

test('A4：fetch 抛异常同样 abort（catch 分支）', async () => {
    const h = harness({
        fetchImpl: async () => { throw new Error('network down'); },
    });
    await h.run();
    assert.equal(vm.runInContext('mbxMigWatchdog', h.ctx), null, 'catch 分支也必须停看门狗');
    assert.equal(vm.runInContext('mbxMigUnlisten', h.ctx), null, 'catch 分支也必须解绑监听');
    assert.ok(h.releaseCalls() >= 1, 'catch 分支也必须释放焦点陷阱（H4）');
    assert.ok(
        h.dom.detail.textContent.includes('迁移启动失败'),
        `应显示启动失败文案（实际：${h.dom.detail.textContent}）`,
    );
});

test('A4：background 分支保持监听（不误伤正常后台迁移）', async () => {
    const h = harness({
        fetchImpl: async () => ({
            ok: true, status: 200,
            headers: { get: (k) => (k === 'X-CorpMail-Migration' ? 'background' : null) },
            text: async () => 'ignored',
        }),
    });
    await h.run();
    assert.ok(vm.runInContext('mbxMigUnlisten', h.ctx), 'background 分支必须保留事件监听');
    assert.ok(h.intervalCount() >= 1, 'background 分支必须保留看门狗');
});

test('A4：200 空响应（pane 应用失败）第三分支同样 abort（复查 R5）', async () => {
    // fetch 200、无 background 头、body 空白 → mbxMigApplyPane 返回 false →
    // 走「迁移结果无法显示」失败态。此分支也必须停看门狗/解绑，否则 30 分钟后
    // 同样的「继续等待」覆盖问题（此前无用例，删掉 abort 无人发现）
    const h = harness({
        fetchImpl: async () => ({
            ok: true, status: 200,
            headers: { get: () => null }, // 非 background
            text: async () => '   ', // 空白 → applyPane false
        }),
    });
    await h.run();
    assert.equal(vm.runInContext('mbxMigWatchdog', h.ctx), null, '第三分支必须停看门狗');
    assert.equal(vm.runInContext('mbxMigUnlisten', h.ctx), null, '第三分支必须解绑监听');
    assert.ok(h.releaseCalls() >= 1, '第三分支也必须释放焦点陷阱（H4）');
    assert.ok(
        h.dom.detail.textContent.includes('迁移结果无法显示'),
        `应显示「迁移结果无法显示」（实际：${h.dom.detail.textContent}）`,
    );
});

test('P1-1：error 后迟到的 progress 不得收起「重试/返回」出口（R21）', async () => {
    const h = harness({
        fetchImpl: async () => ({
            ok: true, status: 200,
            headers: { get: (k) => (k === 'X-CorpMail-Migration' ? 'background' : null) },
            text: async () => '',
        }),
    });
    await h.run();
    // 后台迁移进行中：确认态按钮全收起
    assert.equal(h.dom.buttons.retry.hidden, true, '进行中不得露出重试');
    // 终态失败：露出重试/返回
    h.fire('migration-error', '目标目录不可写');
    assert.equal(h.dom.buttons.retry.hidden, false, '失败态必须露出「重新选择目录并重试」');
    assert.equal(h.dom.buttons.close.hidden, false, '失败态必须露出「返回」');
    assert.ok(h.dom.detail.textContent.includes('目标目录不可写'), '失败文案已显示');
    // 关键回归：看门狗超时但后台仍在跑时，迟到 progress 会到达前端。
    // 旧实现无条件 mbxMigHideActions() 把三枚按钮全收起，用户被锁死。
    h.fire('migration-progress', { phase: 'attachments', detail: '仍在复制附件' });
    assert.equal(h.dom.buttons.retry.hidden, false, '迟到 progress 不得收起重试按钮');
    assert.equal(h.dom.buttons.close.hidden, false, '迟到 progress 不得收起返回按钮');
    assert.ok(
        h.dom.detail.textContent.includes('目标目录不可写')
            && !h.dom.detail.textContent.includes('仍在复制附件'),
        '失败文案不得被迟到的进度覆盖（实际：' + h.dom.detail.textContent + '）',
    );
    assert.equal(vm.runInContext('mbxMigFailed', h.ctx), true, '失败终态标志已置位');
    assert.equal(vm.runInContext('mbxMigWatchdog', h.ctx), null, '失败态看门狗已停');
});

test('A4：二次 mbxMigOpen 复位上次失败态（复查 R5）', async () => {
    // 点名覆盖：二次进入模态时进度条/详情文案/失败按钮复位——复用旧 DOM 时
    // 若不复位，用户带着上一轮「迁移冻结中 + 返回」进场，一点就关掉正在跑的迁移
    const h = harness();
    // 先把共享 detail/bar 污损成上一轮失败态（mask 已由 fakeDom 提供，走复用分支）
    h.dom.detail.textContent = '迁移冻结中（上次失败）';
    h.dom.bar.style.width = '90%';
    // H2：失败态 mbxMigShowError 会把「返回」按钮设为可见；污损成该状态后，
    // 二次 open 必须经 mbxMigHideActions 复位为 hidden——删 app.js:426 调用时
    // 此断言 fail（原桩 querySelector 恒 null 无法观测该操作）。
    h.dom.buttons.close.hidden = false;
    h.dom.buttons.keep.hidden = false;
    vm.runInContext('mbxMigOpen();', h.ctx);
    assert.equal(h.dom.detail.textContent, '准备迁移…', '二次 open 应复位详情文案');
    assert.equal(h.dom.bar.style.width, '0%', '二次 open 应复位进度条');
    assert.equal(h.dom.buttons.close.hidden, true, '二次 open 必须复位「返回」按钮（H2）');
    assert.equal(h.dom.buttons.keep.hidden, true, '二次 open 必须复位「继续等待」按钮（H2）');
});

test('A17：二次进入 mbxConfirmLeave 先释放旧模态的 focus trap', () => {
    // 用真实 app.js 的 mbxConfirmLeave（在 C14 之外的通用模态段）验证：
    // 连开两次（不关第一次），第一次的 release 必须被调用、body 无残留 inert
    const leaveStart = APP_JS.indexOf('function mbxConfirmLeave(');
    const leaveEnd = APP_JS.lastIndexOf('}', APP_JS.indexOf('// htmx 2.0.6 的 hx-confirm', leaveStart)) + 1;
    const modalStart = APP_JS.indexOf('function mbxModalOpen(');
    const modalEnd = APP_JS.indexOf('// 通用离开确认模态', modalStart);
    assert.ok(leaveStart > 0 && leaveEnd > leaveStart, `mbxConfirmLeave 边界异常 ${leaveStart}..${leaveEnd}`);

    const released = [];
    const inertAdded = [];
    const mkEl = (tag) => {
        const el = {
            tagName: tag, dataset: {}, _l: {}, _kids: [],
            focus: () => {}, offsetWidth: 10, offsetHeight: 10,
            addEventListener(n, f) { (this._l[n] = this._l[n] || []).push(f); },
            removeEventListener() {},
            querySelector(sel) {
                if (sel === 'p') return mkEl('p');
                if (sel === '[data-leave-cancel]') return mkEl('button');
                if (sel === '[data-leave-ok]') return mkEl('button');
                if (sel === '.migrate-card' || sel === '.mbx-leave-card') return mkEl('div');
                if (sel === '[data-mbx-action="migration-close"]' || sel === '[data-mbx-action="migration-keep-waiting"]') return mkEl('button');
                return null;
            },
            hasAttribute: () => false,
            querySelectorAll: () => [],
            closest: () => null,
            getAttribute: () => null,
            setAttribute(n, v) { if (n === 'inert') inertAdded.push(this.id || this.tagName); },
            removeAttribute() {},
            appendChild(c) { this._kids.push(c); return c; },
            remove() { this.removed = true; },
            set textContent(v) { this._t = v; }, get textContent() { return this._t || ''; },
            set innerHTML(v) { this._h = v; }, get innerHTML() { return this._h || ''; },
        };
        el.parentNode = null; // appendChild 时由 body 设置
        return el;
    };
    let currentConfirm = null;
    const dom = {
        getElementById: (id) => (id === 'mbx-leave-confirm' ? currentConfirm : null),
        createElement: (tag) => {
            const el = mkEl(tag);
            el.id = '';
            el.parentNode = { remove: () => { el.removed = true; } };
            return el;
        },
        body: {
            children: [],
            appendChild(c) { c.parentNode = { remove: () => { c.removed = true; } }; currentConfirm = c; },
        },
        activeElement: null,
    };
    // mbxModalOpen 以 context 属性形式预置（真实现不执行——vm 顶层 function
    // 声明会遮蔽后设的同名 context 属性，必须先桩后跑 mbxConfirmLeave）
    const ctx = vm.createContext({
        document: dom,
        // 桩：返回可观测 release，并把 __mbxRelease 挂回节点
        mbxModalOpen: (root) => {
            const rel = () => released.push(root.id || 'anon');
            if (root) root.__mbxRelease = rel;
            return rel;
        },
    });
    vm.runInContext(APP_JS.slice(leaveStart, leaveEnd), ctx);

    ctx.mbxConfirmLeave('第一次', null);
    const first = currentConfirm;
    assert.ok(first, '第一次应创建模态');
    ctx.mbxConfirmLeave('第二次', null); // 不关第一次，直接开第二次
    assert.ok(first.removed, '旧模态节点应被移除');
    assert.equal(
        released.length, 1,
        `旧模态的 release 必须被调用（实际 ${released.length} 次）`,
    );
    assert.ok(first.__mbxRelease !== undefined, 'release 应挂到 mask.__mbxRelease');
});

test('A10：主题确认取消后，标志清空——再清空正文会重新弹「正文为空」', () => {
    const C14_START = '// --- C14 compose confirm core START ---';
    const C14_END = '// --- C14 compose confirm core END ---';
    const cs = APP_JS.indexOf(C14_START);
    const ce = APP_JS.indexOf(C14_END);
    assert.ok(cs >= 0 && ce > cs, 'app.js 必须带 C14 core 锚点');

    const confirms = [];
    const notes = [];
    const card = {
        firstChild: null,
        querySelector: () => null,
        insertBefore: (node) => notes.push(node.textContent),
    };
    const elt = {
        dataset: {},
        matches: (s) => s === 'button[hx-post="/compose/send"]',
        closest: (s) => (s === 'form' ? form : null),
        click: () => {},
    };
    const fieldValues = { to_addrs: 'a@b.c', subject: '', body_text: '' };
    const form = {
        querySelector: (sel) => {
            if (sel === '#keep-ids') return { value: '[1]' };
            if (sel === '#body-mode') return { value: 'text' };
            if (sel === '#body-text') return { value: fieldValues.body_text };
            const m = /^\[name="(.+)"\]$/.exec(sel);
            if (m) return { value: fieldValues[m[1]] || '' };
            return null;
        },
        closest: () => card,
    };

    // mbxConfirmLeave 桩：默认**取消**路径（点「返回修改」）——驱动 onCancel
    let mode = 'cancel';
    const ctx = vm.createContext({
        document: {
            body: {
                addEventListener: (name, fn) => {
                    (ctx.__handlers = ctx.__handlers || {})[name] = fn;
                },
            },
            createElement: (tag) => ({ className: '', textContent: '' }),
        },
        mbxEditor: null,
        pickedPaths: [],
        setTimeout: () => 0,
        mbxConfirmLeave: (msg, onLeave, ok, cancel, onCancel) => {
            confirms.push(msg);
            if (mode === 'confirm') { if (onLeave) onLeave(); }
            else if (onCancel) onCancel();
        },
    });
    vm.runInContext(APP_JS.slice(cs, ce), ctx);

    const fire = () => {
        let prevented = false;
        ctx.mbxComposePrecheck({
            detail: { elt },
            preventDefault: () => { prevented = true; },
        });
        return prevented;
    };

    // ① 空正文（带附件）→ 弹「正文为空」；确认（置 bodyOk）
    mode = 'confirm';
    assert.equal(fire(), true);
    assert.match(confirms[0], /正文为空/);
    assert.equal(elt.dataset.mbxBodyOk, '1');

    // ② 仍空主题 → 弹「主题为空」；**取消**（A10 onCancel 清全部标志）
    mode = 'cancel';
    assert.equal(fire(), true);
    assert.match(confirms[1], /主题为空/);
    assert.equal(elt.dataset.mbxBodyOk, undefined, 'A10：取消必须清掉 bodyOk');
    assert.equal(elt.dataset.mbxSubjOk, undefined, 'A10：取消必须清掉 subjOk');

    // ③ 重填正文并发送（有主题、有正文）→ 正常放行不弹窗
    mode = 'confirm';
    fieldValues.body_text = '补一句说明';
    fieldValues.subject = '有主题';
    assert.equal(fire(), false, '补齐后应直接放行');
    assert.equal(confirms.length, 2);

    // ④ 再清空正文（保主题）→ 必须重新弹「正文为空」（旧实现 bodyOk 残留会静默跳过）
    fieldValues.body_text = '';
    assert.equal(fire(), true);
    assert.equal(confirms.length, 3, '再次空正文必须重新确认');
    assert.match(confirms[2], /正文为空/);
});

test('A12：mbxMigApplyPane 换入后派发 htmx:afterSettle 且事件形状正确（rebaseline 依赖）', () => {
    const fnStart = APP_JS.indexOf('function mbxMigApplyPane(');
    const fnEnd = APP_JS.indexOf('\n}', fnStart) + 2;
    assert.ok(fnStart > 0, 'mbxMigApplyPane 必须存在');
    const src = APP_JS.slice(fnStart, fnEnd);

    // 捕获真实派发的事件对象——只做源码字符串断言无法鉴别 detail.target 写错
    const dispatched = [];
    // H3：模拟真实 DOM 换入——replaceWith 后 next 必须在 body 子树内，
    // dispatchEvent 时记录 contains(next)。把 dispatch 移到 replaceWith 之前
    // 则 contains=false（事件到不了挂在 body 的 rebaselineAll），此断言 fail。
    let bodyChildren = [];
    const body = { contains: (n) => bodyChildren.includes(n) };
    const oldPane = {
        replaceWith(next) { oldPane._replaced = next; bodyChildren = [next]; },
    };
    const nextPane = {
        _events: [],
        _containsAtDispatch: null,
        dispatchEvent(ev) {
            this._events.push(ev);
            ev._containsAtDispatch = body.contains(this);
            dispatched.push(ev);
            return true;
        },
    };
    const ctx = vm.createContext({
        document: {
            getElementById: (id) => (id === 'settings-pane' ? oldPane : null),
            body,
            createElement: (tag) => {
                assert.equal(tag, 'template');
                return {
                    set innerHTML(v) { this._html = v; },
                    get innerHTML() { return this._html || ''; },
                    content: { firstElementChild: nextPane },
                };
            },
        },
        // CustomEvent 桩：保留构造参数供断言（vm 内无原生 CustomEvent）
        CustomEvent: class {
            constructor(type, init) {
                this.type = type;
                this.bubbles = !!(init && init.bubbles);
                this.detail = init && init.detail;
            }
        },
        window: { htmx: { process() { nextPane._processed = true; } } },
    });
    vm.runInContext(src + '\nmbxMigApplyPane("<div class=settings-pane></div>");', ctx);

    assert.equal(oldPane._replaced, nextPane, '旧 pane 应被 new 替换');
    assert.equal(nextPane._processed, true, '新 pane 应被 htmx.process');
    assert.equal(dispatched.length, 1, '应恰派发一个事件');
    const ev = dispatched[0];
    assert.equal(ev.type, 'htmx:afterSettle', `事件类型应为 afterSettle（实际 ${ev.type}）`);
    assert.equal(ev.bubbles, true, '事件必须冒泡（rebaselineAll 挂在 body）');
    assert.equal(ev.detail && ev.detail.target, nextPane, 'detail.target 必须是换入的 pane 节点');
    // H3：派发必须在换入之后——节点已在 body 子树，冒泡事件才能到 rebaselineAll
    assert.equal(ev._containsAtDispatch, true, '派发时 next 必须已在 body 子树（先 replaceWith 后 dispatch）');
});

test('mbxPaneDirty：无 form 的收信页必须返回 false（dirty 不误报）（H5）', () => {
    // H5：旧用例用 indexOf('function mbxPaneDirty') 定位，而源码是
    // `window.mbxPaneDirty = function ()`，子串不存在 → idx<0 直接 return，
    // 整条用例空跑。改为定位真实定义并提取函数体用桩驱动，正反两条断言
    // 确保用例真的执行（不再有恒不触发的护栏）。
    const idx = APP_JS.indexOf('window.mbxPaneDirty = function ()');
    assert.ok(idx > 0, 'mbxPaneDirty 必须存在（H5：旧定位恒 -1 使用例空跑）');
    const tail = '\n    };';
    const end = APP_JS.indexOf(tail, idx) + tail.length;
    assert.ok(end > idx, 'mbxPaneDirty 函数体边界异常');
    const src = APP_JS.slice(idx, end);

    const ctx = vm.createContext({
        window: {},
        // 有基线、快照有变化：若有 form 则应报脏；无 form 时 some 空数组 → false
        base: { has: () => true, get: () => 'baseline' },
        paneForms: () => [], // 收信页无 form
        snapshot: () => 'changed',
    });
    vm.runInContext(src, ctx);
    assert.equal(ctx.window.mbxPaneDirty(), false, '无 form 页面必须为 false');
    // 鉴别：同样的基线与快照，仅把 form 变为存在 → 必须报脏，证明用例真跑
    ctx.paneForms = () => [{}];
    assert.equal(ctx.window.mbxPaneDirty(), true, '有 form 且值变化必须为 true（H5 鉴别力）');
});
