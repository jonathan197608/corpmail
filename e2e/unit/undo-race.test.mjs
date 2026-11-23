// 第六轮 P3-5：发送撤销窗口倒计时与 undo 请求竞态的行为测试。
// 把 app.js 的 P5 core 原样放进 Node vm，用假 DOM / 假 htmx 驱动真实分支：
// 撤销在途时倒计时到点必须让路（不转 poll、不移除按钮），请求结束后按
// 成功/失败决定 poll 或恢复重试入口。旧实现会在到点时无条件 remove+poll，
// 迟到的失败改到已脱离 DOM 的节点上、重试入口丢失——本文件钉死该回归。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- P5 undo race core START ---';
const END = '// --- P5 undo race core END ---';

function makeEl() {
    const el = {
        className: '', textContent: '', innerHTML: '', type: '', disabled: false,
        isConnected: true, parentNode: null, children: [], handlers: {},
        appendChild(c) { this.children.push(c); c.parentNode = this; },
        remove() { this.parentNode = null; this.isConnected = false; },
        addEventListener(t, fn) { this.handlers[t] = fn; },
        click() { if (this.handlers.click) this.handlers.click(); },
    };
    return el;
}

function harness({ undoStatus = 'pending', connectedAtFail = true } = {}) {
    const calls = { ajax: [], fetch: [], toasts: [], timers: [], intervals: [], cleared: [] };
    const slot = makeEl();
    const doc = {
        createElement: () => makeEl(),
        getElementById: (id) => (id === 'toast' ? slot : null),
    };
    let fetchResolve;
    const ctx = vm.createContext({
        Promise,
        console,
        URLSearchParams,
        document: doc,
        location: { search: '', pathname: '/' },
        history: { replaceState() {} },
        HTMLElement: function () {},
        setInterval: (fn, ms) => { calls.intervals.push({ fn, ms }); return calls.intervals.length; },
        clearInterval: (h) => { calls.cleared.push(h); },
        setTimeout: (fn, ms) => { calls.timers.push({ fn, ms }); return calls.timers.length; },
        escapeHtml: (s) => String(s),
        MBX_IC: { check: '[ok]' },
        mbxOnFetchDone: null,
        mbxToast: (m, c, ms) => calls.toasts.push({ m, c, ms }),
        now: () => 1_700_000_000,
        fetch: (url, opts) => {
            calls.fetch.push({ url, opts });
            return new Promise((res) => { fetchResolve = res; });
        },
        htmx: {
            ajax: (method, url, opts) => {
                calls.ajax.push({ method, url, opts });
                if (undoStatus === 'resolve') return Promise.resolve();
                if (undoStatus === 'reject') return Promise.reject(new Error('409'));
                return new Promise((resolve, reject) => {
                    calls.settleUndo = { resolve, reject };
                });
            },
        },
    });
    const start = APP_JS.indexOf(START);
    const end = APP_JS.indexOf(END);
    assert.ok(start >= 0 && end > start, 'app.js 必须带 P5 core 锚点');
    vm.runInContext(APP_JS.slice(start, end), ctx);
    // core 依赖 env.now，但 IIFE 未纳入本段；直接调 core 注入完整 env
    ctx.mbxUndoToastCore('42', {
        doc, htmx: ctx.htmx, fetch: ctx.fetch,
        setInterval: ctx.setInterval, clearInterval: ctx.clearInterval,
        setTimeout: ctx.setTimeout, now: ctx.now,
        escapeHtml: ctx.escapeHtml, icon: ctx.MBX_IC,
        onFetchDone: null,
        toast: (m, c, ms) => calls.toasts.push({ m, c, ms }),
    });
    const undo = slot.children[0].children[1]; // toast -> [label, undo]
    return { ctx, calls, slot, undo, setFetchResult: (s) => fetchResolve(s), connectedAtFail };
}

test('纯决策：倒计时到点仅在撤销不在途时才转 poll', () => {
    const c = vm.createContext({});
    vm.runInContext(APP_JS.slice(APP_JS.indexOf(START), APP_JS.indexOf(END)), c);
    const f = vm.runInContext('mbxUndoCountdownShouldPoll', c);
    assert.equal(f(0, true), false, '撤销在途必须让路');
    assert.equal(f(0, false), true, '无在途请求才转 poll');
    assert.equal(f(2, false), false, '窗口未到不转 poll');
});

test('撤销在途时倒计时到点：不移除按钮、不发起状态轮询', async () => {
    const h = harness({ undoStatus: 'pending' });
    h.undo.click();
    assert.equal(h.calls.ajax.length, 1, '应发起 undo 请求');
    // 倒计时到点（left 8→...→0）
    const tick = h.calls.intervals[0].fn;
    for (let i = 0; i < 8; i++) tick();
    assert.equal(h.calls.fetch.length, 0, '撤销在途时到点不得转 poll');
    assert.equal(h.undo.isConnected, true, '撤销在途时按钮不得被移除');
    assert.equal(h.undo.disabled, true, '撤销在途按钮保持禁用');
});

test('撤销在途期间到点、随后失败：不吞失败态，按窗口已过转 poll', async () => {
    const h = harness({ undoStatus: 'pending' });
    h.undo.click();
    const tick = h.calls.intervals[0].fn;
    for (let i = 0; i < 8; i++) tick();
    assert.equal(h.calls.fetch.length, 0, '到点时撤销在途，未提前转 poll');

    // undo 迟到失败（窗口已过）：重试已无意义，须 toast 明示 + 转 poll 查真实状态
    h.calls.settleUndo.reject(new Error('409'));
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    assert.ok(h.calls.toasts.some((t) => t.m.includes('撤销发送失败')), '须 toast 明示失败');
    assert.equal(h.calls.fetch.length, 1, '窗口已过 + 失败后应转 poll 查真实状态');
    assert.equal(h.calls.cleared.length, 1, '转 poll 应清倒计时 timer');
});

test('撤销在途期间到点、随后成功：转 poll，不显示失败态', async () => {
    const h = harness({ undoStatus: 'pending' });
    h.undo.click();
    const tick = h.calls.intervals[0].fn;
    for (let i = 0; i < 8; i++) tick();
    h.calls.settleUndo.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(h.calls.fetch.length, 1, '成功且窗口已过也应转 poll 核对终态');
    assert.equal(h.undo.textContent, '撤销发送', '成功不得改成失败文案');
    assert.equal(h.calls.toasts.length, 0, '成功不得弹失败 toast');
});

test('正常到点（无在途撤销）：立即移除按钮并转 poll', () => {
    const h = harness();
    const tick = h.calls.intervals[0].fn;
    for (let i = 0; i < 8; i++) tick();
    assert.equal(h.undo.isConnected, false, '正常到点应移除撤销按钮');
    assert.equal(h.calls.fetch.length, 1, '正常到点应转 poll');
});

test('窗口未过 + 撤销失败：保留按钮可重试，不转 poll', async () => {
    const h = harness({ undoStatus: 'reject' });
    h.undo.click(); // 第 0 秒点击，窗口未过
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(h.undo.isConnected, true, '窗口未过失败须保留按钮');
    assert.equal(h.undo.disabled, false, '按钮须可重试');
    assert.equal(h.undo.textContent, '撤销失败，重试', '须显式回显');
    assert.equal(h.calls.fetch.length, 0, '窗口未过不转 poll');
});
