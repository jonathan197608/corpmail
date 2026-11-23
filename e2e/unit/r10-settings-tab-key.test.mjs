// 第十轮 H1：设置页 tab 的键盘激活此前只有「对整个 app.js 做 contains」的静态
// 护栏，而 tab.click() 在「离开确认」里还有第二处——删掉 A15 的实现快速门禁
// 仍全绿。这里把生产 core 直接放进 vm，断言 Enter/Space 真的派发原生 click。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const a = APP_JS.indexOf('// --- R10 settings tab key core START ---');
const b = APP_JS.indexOf('// --- R10 settings tab key core END ---');
assert.ok(a >= 0 && b > a, 'app.js 必须带 R10 settings tab key core 锚点');
const CORE = APP_JS.slice(a, b);

function fire(key, { repeat = false, isTab = true } = {}) {
    const handlers = [];
    const calls = { click: 0, prevented: 0 };
    const tab = { click() { calls.click++; } };
    const doc = {
        addEventListener: (type, fn) => { if (type === 'keydown') handlers.push(fn); },
    };
    vm.runInContext(CORE, vm.createContext({ document: doc, console }));
    assert.equal(handlers.length, 1, '必须注册一个 keydown 委托');
    handlers[0]({
        key,
        repeat,
        target: {
            closest: (sel) => (isTab && sel === '.settings-tabs .stab' ? tab : null),
        },
        preventDefault() { calls.prevented++; },
    });
    return calls;
}

test('Enter 激活 tab：preventDefault + 原生 click（A15）', () => {
    const calls = fire('Enter');
    assert.equal(calls.prevented, 1);
    assert.equal(calls.click, 1, '必须走 tab.click() 与真实点击同路');
});

test('Space / Spacebar 同样激活', () => {
    for (const key of [' ', 'Spacebar']) {
        const calls = fire(key);
        assert.equal(calls.click, 1, `${key} 须激活 tab`);
        assert.equal(calls.prevented, 1);
    }
});

test('其他键、长按重复、非 tab 目标一律不激活', () => {
    for (const opts of [
        { key: 'j' },
        { key: 'Enter', repeat: true },
        { key: 'Enter', isTab: false },
    ]) {
        const calls = fire(opts.key, opts);
        assert.equal(calls.click, 0, `不应激活：${JSON.stringify(opts)}`);
        assert.equal(calls.prevented, 0);
    }
});
