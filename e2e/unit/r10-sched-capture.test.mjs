// 第十轮 P1-4：立即发送不能在 capture 阶段清空 #sched-at。旧实现在预校验/
// 确认模态取消请求后不恢复，隐藏域永久空而控件仍有值 → 下一次「定时发送」
// 变成立即发送、存草稿丢计划。新实现只在 configRequest 里覆盖当次参数。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const a = APP_JS.indexOf('// --- R10 sched capture core START ---');
const b = APP_JS.indexOf('// --- R10 sched capture core END ---');
assert.ok(a >= 0 && b > a, 'app.js 必须带 R10 sched capture core 锚点');
const CORE = APP_JS.slice(a, b);

// R17：validationError 参数化——旧 harness 只走 happy path（picker 恒有值、
// 校验桩恒空），两条 preventDefault 拦截分支从未被触及（删掉拦截仍全绿）。
function harness({ pickerValue = '2026-11-24T10:00', validationError = '' } = {}) {
    const hidden = { value: pickerValue };
    const cleared = { value: '0' };
    const picker = { value: pickerValue };
    const handlers = [];
    const calls = { prevented: 0 };
    const body = {
        addEventListener: (type, fn) => { if (type === 'htmx:configRequest') handlers.push(fn); },
    };
    const doc = {
        body,
        getElementById: (id) =>
            id === 'sched-at' ? hidden : id === 'sched-cleared' ? cleared
            : id === 'sched-input' ? picker : null,
        createElement: () => ({ className: '', textContent: '', remove() {}, parentNode: null }),
        querySelector: () => ({ insertBefore() {} }),
    };
    const ctx = vm.createContext({
        document: doc,
        Date,
        console,
        setTimeout: () => 0,
        mbxSchedLocalMin: () => '2026-11-24T09:00',
        mbxSchedValidationError: () => validationError,
    });
    vm.runInContext(CORE, ctx);
    return {
        hidden, picker, handlers, calls,
        dispatch(elt, parameters) {
            const ev = { detail: { elt, parameters }, preventDefault() { calls.prevented++; } };
            handlers.forEach((h) => h(ev));
        },
    };
}

const sendBtn = {
    id: '',
    matches: (sel) => sel === 'button[hx-post="/compose/send"]',
};
const schedBtn = {
    id: 'sched-send-btn',
    matches: (sel) => sel === '#sched-send-btn',
};

test('立即发送只覆盖当次参数，不破坏隐藏域（第十轮 P1-4）', () => {
    const h = harness();
    const params = { scheduled_at: '2026-11-24T10:00', subject: 'x' };
    h.dispatch(sendBtn, params);
    assert.equal(params.scheduled_at, '', '本次请求须按立即发送提交');
    assert.equal(params.subject, 'x', '其他参数不受影响');
    assert.equal(h.hidden.value, '2026-11-24T10:00', '隐藏域必须保留，供后续取消/改定时使用');
    assert.equal(h.calls.prevented, 0);
});

test('定时发送保持隐藏域值并正常放行（回归）', () => {
    const h = harness();
    const params = { scheduled_at: '2026-11-24T10:00' };
    h.dispatch(schedBtn, params);
    assert.equal(params.scheduled_at, '2026-11-24T10:00');
    assert.equal(h.hidden.value, '2026-11-24T10:00');
    assert.equal(h.calls.prevented, 0);
});

test('立即发送的 configRequest 不注册 click capture 监听', () => {
    const h = harness();
    assert.equal(h.handlers.length, 1, '只应有 configRequest 一个监听');
});

// R17 P1（测试基建 fail-open）：定时发送未选时刻必须拦截。旧 harness 的
// picker 恒有值，这条分支从未被测试；删掉 app.js 的 preventDefault 仍全绿，
// 而用户点「定时发送」未选时刻会误以「立即发送」落地（撤销窗口仅 8s）。
test('R17 定时发送未选时刻必须拦截', () => {
    const h = harness({ pickerValue: '' });
    const params = { scheduled_at: '' };
    h.dispatch(schedBtn, params);
    assert.equal(h.calls.prevented, 1, '未选时刻必须 preventDefault');
    assert.equal(params.scheduled_at, '', '不得改写参数');
});

test('R17 定时时刻校验失败必须拦截', () => {
    const h = harness({ validationError: '时刻不合法' });
    const params = { scheduled_at: 'bad' };
    h.dispatch(schedBtn, params);
    assert.equal(h.calls.prevented, 1, '校验失败必须 preventDefault');
    assert.equal(params.scheduled_at, 'bad', '不得改写参数');
});
