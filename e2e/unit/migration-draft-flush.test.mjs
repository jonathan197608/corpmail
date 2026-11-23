// A10：迁移发起前的草稿强制 flush 行为测试。
//
// 冻结位在 POST /settings/storage/move 的协议线程里置位，此后
// /compose/autosave 返回 409；迁移完成 1.5s 自动退出，没落库的内容全丢。
// 这里把 app.js 的 A10 core 原样放进 Node vm，用假 DOM / 假 htmx 驱动真实
// 分支：非撰写页短路、200/409 判定、失败一律取消迁移、afterRequest 兜底、
// 超时兜底，以及「flush 必须排在迁移 POST 之前」的顺序护栏。
//
// 运行：node --test e2e/unit/migration-draft-flush.test.mjs（e2e/run.sh 0/6 步自动跑）
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- A10 migration draft flush core START ---';
const END = '// --- A10 migration draft flush core END ---';

function harness({ withForm = true, status = 200, fireHandler = true } = {}) {
    // listeners 用数组（第六轮 P3-4）：单槽桩无法表达两路并发 onDone，
    // 正是它掩盖了「按 path 判身份导致两路 Promise 串线」的 bug。
    const calls = { ajax: [], listeners: [], removed: [], timers: [] };
    const draftIdEl = { value: '0' };
    const form = {
        isConnected: true,
        values: { to_addrs: 'a@b.test', subject: '未写完的邮件' },
        querySelector: (sel) =>
            sel === 'input[name="draft_id"]' ? draftIdEl : null,
    };
    const hint = { closest: (sel) => (sel === 'form' ? form : null) };
    const doc = {
        getElementById: (id) => (id === 'autosave-hint' ? (withForm ? hint : null) : null),
        body: {
            addEventListener: (type, fn) => { calls.listeners.push({ type, fn }); },
            removeEventListener: (type, fn) => {
                calls.removed.push(type);
                const i = calls.listeners.findIndex((l) => l.type === type && l.fn === fn);
                if (i >= 0) calls.listeners.splice(i, 1);
            },
        },
    };
    class FakeFormData {
        constructor(f) { this.entries = Object.entries(f.values || {}); }
        forEach(cb) { this.entries.forEach(([k, v]) => cb(v, k)); }
    }
    // 真实 DOM 事件会广播给所有监听器；桩必须快照遍历，否则测不出两路 onDone 串线
    calls.dispatch = (type, detail) => {
        for (const l of [...calls.listeners]) {
            if (l.type === type) l.fn({ detail });
        }
    };
    const ctx = vm.createContext({
        Promise,
        JSON,
        FormData: FakeFormData,
        setTimeout: (fn) => { calls.timers.push(fn); return calls.timers.length; },
        clearTimeout: () => {},
        document: doc,
        htmx: {
            ajax: (method, url, opts) => {
                calls.ajax.push({ method, url, opts });
                if (fireHandler && opts && opts.handler) {
                    Promise.resolve().then(() => opts.handler(null, {
                        xhr: {
                            status,
                            responseText: JSON.stringify({
                                ok: status >= 200 && status < 300,
                                draft_id: 7,
                            }),
                        },
                    }));
                }
                return Promise.resolve();
            },
        },
    });
    const start = APP_JS.indexOf(START);
    const end = APP_JS.indexOf(END);
    assert.ok(start >= 0 && end > start, 'app.js 必须带 A10 core 锚点');
    vm.runInContext(APP_JS.slice(start, end), ctx);
    return { ctx, calls, doc, draftIdEl, form, htmx: ctx.htmx };
}

test('非撰写页直接放行，不发 autosave 请求', async () => {
    const h = harness({ withForm: false });
    const r = await h.ctx.mbxFlushComposeDraft();
    assert.equal(r.ok, true);
    assert.equal(r.skipped, true);
    assert.equal(h.calls.ajax.length, 0, '设置页发起迁移不应产生额外请求');
});

test('有撰写表单时先发 autosave，200 才放行并回填 draft_id', async () => {
    const h = harness();
    const r = await h.ctx.mbxFlushComposeDraft();
    assert.equal(r.ok, true, '200 才算落库成功');
    assert.equal(h.calls.ajax.length, 1);
    assert.ok(
        h.calls.ajax[0].url.startsWith('/compose/autosave?__at='),
        `autosave URL 须带唯一 token：${h.calls.ajax[0].url}`
    );
    assert.equal(h.calls.ajax[0].method, 'POST');
    assert.equal(h.calls.ajax[0].opts.values.subject, '未写完的邮件');
    assert.equal(h.draftIdEl.value, '7', '新草稿必须回填隐藏域');
    assert.equal(h.calls.removed.length, 1, '请求结束须解绑 afterRequest 监听');
});

test('409（冻结期）算失败，mbxMigAfterFlush 必须走 cancel', async () => {
    const h = harness({ status: 409 });
    const r = await h.ctx.mbxFlushComposeDraft();
    assert.equal(r.ok, false);
    let proceeded = 0;
    let cancelled = 0;
    const allowed = h.ctx.mbxMigAfterFlush(r, {
        proceed: () => { proceeded += 1; return true; },
        cancel: () => { cancelled += 1; return false; },
    });
    assert.equal(allowed, false, '失败不得放行迁移');
    assert.equal(proceeded, 0);
    assert.equal(cancelled, 1);
});

test('flush 成功才调用 proceed', () => {
    const h = harness();
    let proceeded = 0;
    let cancelled = 0;
    const allowed = h.ctx.mbxMigAfterFlush({ ok: true }, {
        proceed: () => { proceeded += 1; return true; },
        cancel: () => { cancelled += 1; return false; },
    });
    assert.equal(allowed, true);
    assert.equal(proceeded, 1);
    assert.equal(cancelled, 0);
});

test('handler 未到达时 htmx:afterRequest 兜底判定，不把成功误判成失败', async () => {
    const h = harness({ fireHandler: false });
    const pending = h.ctx.mbxFlushComposeDraft();
    assert.ok(h.calls.listeners.some((l) => l.type === 'htmx:afterRequest'));
    h.calls.dispatch('htmx:afterRequest', {
        pathInfo: { requestPath: h.calls.ajax[0].url }, xhr: { status: 200 },
    });
    assert.equal((await pending).ok, true);
});

test('并发两路 autosave：一路的 afterRequest 不得 settle 掉另一路（第六轮 P3-4）', async () => {
    const h = harness({ fireHandler: false });
    // 第八轮 P3-6：同一 form 的并发调用已合并为一次请求（防双草稿）。
    // token 防串线针对的是「不同 form 的两路请求」（如两个撰写上下文），
    // 故这里用两个不同的 form 对象构造真正的两路并发。
    const formB = {
        isConnected: true,
        values: { subject: '另一封' },
        querySelector: (sel) => (sel === 'input[name="draft_id"]' ? { value: '0' } : null),
    };
    const pA = h.ctx.mbxAutosaveRequest(h.form, h.doc, h.htmx);
    const pB = h.ctx.mbxAutosaveRequest(formB, h.doc, h.htmx);
    assert.equal(h.calls.ajax.length, 2);
    assert.notEqual(
        h.calls.ajax[0].url,
        h.calls.ajax[1].url,
        '两路请求须带不同 token'
    );
    assert.equal(
        h.calls.listeners.filter((l) => l.type === 'htmx:afterRequest').length,
        2,
        '两路各装一个 onDone'
    );

    // A 的 200 响应广播：只应 settle A，B 的 onDone 须按 token 忽略
    h.calls.dispatch('htmx:afterRequest', {
        pathInfo: { requestPath: h.calls.ajax[0].url }, xhr: { status: 200 },
    });
    assert.equal((await pA).ok, true, 'A 应被自身响应 settle');
    let bSettledEarly = false;
    pB.then(() => { bSettledEarly = true; });
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(bSettledEarly, false, 'B 不得被 A 的响应串线 settle');

    // B 的 409 响应广播：B 判失败（自身 status）
    h.calls.dispatch('htmx:afterRequest', {
        pathInfo: { requestPath: h.calls.ajax[1].url }, xhr: { status: 409 },
    });
    assert.equal((await pB).ok, false, 'B 的 ok 只由自身 status 决定');
});

test('请求悬挂到超时也判失败，迁移不会被无限期挂住', async () => {
    const h = harness({ fireHandler: false });
    const pending = h.ctx.mbxFlushComposeDraft();
    assert.equal(h.calls.timers.length, 1, '必须装超时兜底');
    h.calls.timers[0]();
    const r = await pending;
    assert.equal(r.ok, false);
    assert.equal(r.timeout, true);
});

test('app.js 里 flush 必须排在迁移 POST 之前', () => {
    const flushAt = APP_JS.indexOf('await mbxFlushComposeDraft()');
    const postAt = APP_JS.indexOf("fetch('/settings/storage/move'");
    assert.ok(flushAt > 0 && postAt > flushAt,
        'flush 必须在 POST /settings/storage/move 之前，否则冻结位已置位、autosave 必被 409');
});

test('同一 form 并发两路 autosave 只发一次请求，避免双草稿（第八轮 P3-6）', async () => {
    const h = harness();
    // 迁移 flush 与 5s 防抖共用同一个 form：两路 draft_id 均为 0，
    // 若各发一次 POST，后端会 INSERT 两份重复草稿（一份成幽灵）。
    const pA = h.ctx.mbxFlushComposeDraft();
    const pB = h.ctx.mbxFlushComposeDraft();
    assert.equal(h.calls.ajax.length, 1, '同一 form 的两路调用须合并为一次请求');
    assert.equal(pA, pB, '两路应复用同一个 Promise');
    await Promise.all([pA, pB]);
    // 请求结束后 inflight 槽须清空，下一次编辑能正常再发一次
    const later = h.ctx.mbxFlushComposeDraft();
    await later;
    assert.equal(h.calls.ajax.length, 2, '在途请求结束后新调用应发新请求');
});
