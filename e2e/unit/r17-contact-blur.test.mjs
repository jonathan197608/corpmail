// R18：R17 建的静态正则护栏会被注释命中（删掉 focus 的 clearTimeout，
// 在注释里写同名字串仍全绿）。改成 vm 行为测试：真的派发 blur → focus →
// 到时，断言建议框不会被旧 blur 定时器隐藏。
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
const START = 'function mbxInitContacts() {';
const END = '// --- P7 contact suggest core START ---';
const start = app.indexOf(START);
const end = app.indexOf(END);
assert.ok(start >= 0 && end > start, 'app.js 必须有 mbxInitContacts 主体');
const CORE = app.slice(start, end);

function harness() {
    const handlers = {};
    const box = { hidden: true };
    const inp = {
        dataset: {},
        parentElement: null,
        setAttribute() {},
        addEventListener(type, fn) { handlers[type] = fn; },
    };
    const timers = [];
    let seq = 0;
    const ctx = vm.createContext({
        document: {
            querySelectorAll: () => [inp],
            createElement: () => box,
            body: { appendChild() {} },
        },
        setTimeout(fn, ms) {
            const t = { id: ++seq, fn, ms, cancelled: false };
            timers.push(t);
            return t;
        },
        clearTimeout(t) { if (t) t.cancelled = true; },
        mbxContactInvalidate() {},
        // 模拟已有候选：query 返回后建议框展开，focus 才有可见性可断言
        mbxContactQuery(_inp, state) { state.items = [{}]; state.box.hidden = false; },
        mbxContactKey() {},
    });
    vm.runInContext(CORE, ctx);
    ctx.mbxInitContacts();
    return {
        handlers, timers, box,
        fire(type) { handlers[type](); },
        // 只有未被 clear 的定时器才真正到期
        flush() {
            for (const t of timers) if (!t.cancelled) t.fn();
        },
    };
}

test('R18 blur 后重新 focus：旧隐藏定时器不得再隐藏建议框', () => {
    const h = harness();
    h.fire('input');
    h.timers[0].fn(); // query 返回：候选到达、建议框展开
    assert.equal(h.box.hidden, false, '前置：建议框已展开');
    h.fire('blur');
    assert.equal(h.timers[1].ms, 150, 'blur 隐藏定时器为 150ms');
    h.fire('focus');
    h.flush();
    assert.equal(h.box.hidden, false, 'focus 必须取消在途 blur 定时器');
});

test('R18 连续 blur 只保留最后一个隐藏定时器', () => {
    const h = harness();
    h.fire('input');
    h.timers[0].fn();
    h.fire('blur');
    const first = h.timers[1];
    h.fire('blur');
    assert.equal(first.cancelled, true, '新 blur 必须取消上一个隐藏定时器');
    h.fire('focus');
    h.flush();
    assert.equal(h.box.hidden, false, 'focus 后建议框须保持可见');
});
