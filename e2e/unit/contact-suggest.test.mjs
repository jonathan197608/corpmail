// 第七轮 P3-5：联系人建议 200ms 防抖窗口内按 Enter 不得插入过期建议。
// 旧实现：input 只清 timer，旧 items/active=0 仍在，Enter 直接选中上一轮
// 建议；R47 的 seq 守卫只挡迟到 fetch，挡不住查询尚未发出时的 Enter。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- P7 contact suggest core START ---';
const END = '// --- P7 contact suggest core END ---';
const a = APP_JS.indexOf(START);
const b = APP_JS.indexOf(END);
assert.ok(a >= 0 && b > a, 'app.js 必须带 P7 contact suggest core 锚点');
const CORE = APP_JS.slice(a, b);

function loadCore() {
    const ctx = vm.createContext({});
    return vm.runInContext(CORE + '\nmbxContactInvalidate', ctx);
}

// 按函数边界切片（而非全文件 includes）：确保测到的是 app.js 里真实跑的源码
function functionSource(name) {
    const start = APP_JS.indexOf('function ' + name + '(');
    assert.ok(start >= 0, `app.js 必须含 ${name}`);
    let depth = 0;
    let opened = false;
    for (let i = start; i < APP_JS.length; i++) {
        const c = APP_JS[i];
        if (c === '{') { depth += 1; opened = true; }
        else if (c === '}') {
            depth -= 1;
            if (opened && depth === 0) return APP_JS.slice(start, i + 1);
        }
    }
    assert.fail(`未找到 ${name} 函数结尾`);
}

test('失效后旧建议清空、序号递增、建议框隐藏', () => {
    const invalidate = loadCore();
    const state = {
        items: [{ name: '李四', addr: 'li@x.cn' }],
        active: 0,
        seq: 3,
        box: { hidden: false },
    };
    invalidate(state);
    assert.equal(state.items.length, 0, '旧建议须清空');
    assert.equal(state.active, -1);
    assert.equal(state.seq, 4, '在途响应须因 seq 失配被丢弃');
    assert.equal(state.box.hidden, true);
});

test('input 监听器派发真实事件时调用 core 失效函数并清掉旧建议', () => {
    const invalidate = loadCore(); // 真实 core，不是镜像
    const listeners = {};
    const timers = [];
    const cleared = [];
    let seeded = null;
    let queryCalls = 0;
    const box = { hidden: true };
    const inp = {
        dataset: {},
        parentElement: null,
        setAttribute() {},
        addEventListener(type, fn) { (listeners[type] ||= []).push(fn); },
        dispatchEvent(ev) { (listeners[ev.type] || []).forEach((fn) => fn(ev)); },
    };
    const ctx = vm.createContext({
        document: {
            querySelectorAll: () => [inp],
            createElement: () => box,
            body: { appendChild() {} },
        },
        location: { search: '' },
        // 与生产一致：监听器直接调用 core 函数
        mbxContactInvalidate: invalidate,
        // 首次防抖到期时把「上一轮建议」种进闭包 state，供第二次 input 验证清场
        mbxContactQuery: (input, state) => {
            queryCalls += 1;
            seeded = state;
            state.items = [{ name: '李四', addr: 'li@x.cn' }];
            state.active = 0;
            state.seq = 3;
            state.box.hidden = false;
        },
        mbxContactKey: () => {},
        clearTimeout: (id) => cleared.push(id),
        setTimeout: (fn, ms) => { timers.push({ fn, ms }); return timers.length; },
        Event: class { constructor(type) { this.type = type; } },
    });
    vm.runInContext(functionSource('mbxInitContacts'), ctx);
    ctx.mbxInitContacts();

    // 第一次真实 input：注册 200ms 防抖定时器
    inp.dispatchEvent(new ctx.Event('input'));
    assert.equal(timers.length, 1);
    assert.equal(timers[0].ms, 200);
    // 防抖到期：拿到闭包 state 并种入过期建议
    timers[0].fn();
    assert.equal(queryCalls, 1);
    assert.equal(seeded.items.length, 1, '前置条件：建议框里确有上一轮候选');

    // 第二次真实 input：必须经监听器调用 core 立即作废
    inp.dispatchEvent(new ctx.Event('input'));
    assert.equal(seeded.items.length, 0, 'input 监听器必须清空旧建议');
    assert.equal(seeded.active, -1);
    assert.equal(seeded.seq, 4, 'seq 必须递增，迟到响应才被丢弃');
    assert.equal(seeded.box.hidden, true);
    assert.ok(cleared.includes(1), '旧防抖定时器须先清掉');
    assert.equal(timers.length, 2, '新输入须重新排 200ms 防抖');
});
