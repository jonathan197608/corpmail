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
    const calls = { ajax: [], listeners: {}, removed: [], timers: [] };
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
            addEventListener: (type, fn) => { calls.listeners[type] = fn; },
            removeEventListener: (type, fn) => {
                calls.removed.push(type);
                if (calls.listeners[type] === fn) delete calls.listeners[type];
            },
        },
    };
    class FakeFormData {
        constructor(f) { this.entries = Object.entries(f.values || {}); }
        forEach(cb) { this.entries.forEach(([k, v]) => cb(v, k)); }
    }
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
    return { ctx, calls, doc, draftIdEl };
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
    assert.equal(h.calls.ajax[0].url, '/compose/autosave');
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
    assert.equal(typeof h.calls.listeners['htmx:afterRequest'], 'function');
    h.calls.listeners['htmx:afterRequest']({
        detail: { pathInfo: { requestPath: '/compose/autosave' }, xhr: { status: 200 } },
    });
    assert.equal((await pending).ok, true);
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
