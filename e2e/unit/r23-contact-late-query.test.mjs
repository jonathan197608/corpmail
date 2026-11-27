// R23 前端状态机回归：blur 后迟到的联系人查询不得重弹建议框。
// 用 Node vm 原样驱动 app.js 的 mbxInitContacts / P7 invalidate / mbxContactQuery，
// 可控 fetch Promise 精确复现「防抖窗口内 blur → 200ms 查询仍发出 → 返回重弹」。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const app = readFileSync(
    path.join(path.dirname(fileURLToPath(import.meta.url)), '../../ui-dist/app.js'),
    'utf8'
);
const slice = (startMark, endMark) => {
    const a = app.indexOf(startMark);
    const b = app.indexOf(endMark, a);
    assert.ok(a >= 0 && b > a, `缺少锚点 ${startMark} / ${endMark}`);
    return app.slice(a, b);
};
const CORE_INIT = slice('function mbxInitContacts() {', '// --- P7 contact suggest core START ---');
const CORE_INVALIDATE = slice('// --- P7 contact suggest core START ---', '// --- P7 contact suggest core END ---');
const QUERY = slice('async function mbxContactQuery(inp, state) {', '// --- R9 contact pick core START ---');

function harness() {
    const handlers = {};
    const items = [];
    const box = {
        hidden: true,
        innerHTML: '',
        querySelectorAll: () => items,
    };
    const inp = {
        dataset: {},
        value: '张',
        parentElement: null,
        setAttribute() {},
        addEventListener(type, fn) { handlers[type] = fn; },
    };
    const timers = [];
    let timerSeq = 0;
    let fetchCalls = 0;
    let pending = null;
    let capturedState = null;
    const ctx = vm.createContext({
        document: {
            querySelectorAll: () => [inp],
            createElement: () => box,
            body: { appendChild() {} },
        },
        setTimeout(fn, ms) {
            const t = { id: ++timerSeq, fn, ms, cancelled: false };
            timers.push(t);
            return t;
        },
        clearTimeout(t) { if (t) t.cancelled = true; },
        mbxContactToken: () => '张',
        fetch(_url) {
            fetchCalls += 1;
            return new Promise((resolve, reject) => { pending = { resolve, reject }; });
        },
        encodeURIComponent,
        escapeHtml: (s) => String(s),
        mbxContactMark(state) { capturedState = state; },
        mbxContactPick() {},
        mbxContactKey() {},
    });
    vm.runInContext(CORE_INVALIDATE + '\n' + QUERY + '\n' + CORE_INIT, ctx);
    ctx.mbxInitContacts();
    return {
        box, timers,
        get fetchCalls() { return fetchCalls; },
        get items() { return capturedState ? capturedState.items : []; },
        fire(type) { handlers[type](); },
        fireQueryTimer() {
            const q = timers.find((t) => t.ms === 200 && !t.cancelled);
            assert.ok(q, '应有一个 200ms 防抖查询定时器');
            q.fn();
        },
        resolveFetch(data) {
            assert.ok(pending, 'fetch 应已被调用');
            pending.resolve({ ok: true, json: () => Promise.resolve(data) });
            pending = null;
        },
        async settle() {
            for (let i = 0; i < 5; i += 1) await Promise.resolve();
        },
    };
}

test('防抖窗口内 blur：迟到的查询响应不得重弹建议框', async () => {
    const h = harness();
    h.fire('input');            // invalidate + 200ms 防抖
    h.fireQueryTimer();         // 发出 fetch（此刻 seq=N）
    assert.equal(h.fetchCalls, 1, 'blur 前查询确实已发出');
    h.fire('blur');             // 清防抖 + 递 seq 作废在途
    h.resolveFetch([{ name: '张三', addr: 'zhang@x.cn' }]);
    await h.settle();
    assert.equal(h.box.hidden, true, 'blur 后焦点已离开，迟到的候选不得重弹建议框');
});

test('无 blur 时正常输入：查询响应仍应弹出建议框（修复未误伤主路径）', async () => {
    const h = harness();
    h.fire('input');
    h.fireQueryTimer();
    h.resolveFetch([{ name: '张三', addr: 'zhang@x.cn' }]);
    await h.settle();
    assert.equal(h.box.hidden, false, '未 blur 时建议框须正常展开');
});

test('blur 不调用 mbxContactInvalidate：保留候选供 150ms 内 focus 方向键选人（R17）', async () => {
    const h = harness();
    h.fire('input');
    h.fireQueryTimer();
    h.resolveFetch([{ name: '张三', addr: 'zhang@x.cn' }]);
    await h.settle();
    assert.equal(h.items.length, 1, '前置：候选已到达');
    h.fire('blur');
    assert.equal(
        h.items.length, 1,
        'blur 不得清空 items——若调 mbxContactInvalidate，R17「150ms 内重新 focus 方向键选人」失效'
    );
});
