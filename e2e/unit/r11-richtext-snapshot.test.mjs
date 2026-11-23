// 第十一轮 P1-1：富文本 change 必须同步写 #body-html 隐藏域，否则 autosave
// 的 inflight FormData 快照永远读到上一轮旧值，慢请求下的在途编辑被吞。
// 直接从 app.js 抽取 editor change handler 的源码在 vm 执行（非手写镜像）。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');

// 取 mbxEditor.on('change', function () { ... }) 的函数体
const m = APP_JS.match(/mbxEditor\.on\('change',\s*function \(\) \{([\s\S]*?)\n        \}\);/);
assert.ok(m, 'app.js 必须含富文本 change handler');
const HANDLER_BODY = m[1];

class FakeEvent {
    constructor(type, init) { this.type = type; Object.assign(this, init || {}); }
}

test('富文本 change 同步写 body-html 隐藏域，且派发可冒泡 input（P1-1）', () => {
    const hidden = { value: '' };
    const ta = { events: [], dispatchEvent(ev) { this.events.push(ev); } };
    const c = vm.createContext({
        document: {
            getElementById: (id) => id === 'body-html' ? hidden : id === 'body-text' ? ta : null,
        },
        Event: FakeEvent,
        mbxEditor: { getHtml: () => '<p>新正文</p>' },
        mbxEnsureTableBorders: (html) => html.replace(/<table/gi, '<table border="1"'),
    });
    vm.runInContext('(function () {' + HANDLER_BODY + '\n})()', c);
    assert.equal(hidden.value, '<p>新正文</p>', 'change 时必须把最新 HTML 写入隐藏域');
    assert.equal(ta.events.length, 1, '仍须派发 input 让 dirty/autosave 感知');
    assert.equal(ta.events[0].bubbles, true, 'input 必须可冒泡到 body 委托');
});

test('净化口径一致：含表格的 HTML 加 border（P1-1）', () => {
    const hidden = { value: '' };
    const ta = { dispatchEvent() {} };
    const c = vm.createContext({
        document: { getElementById: (id) => id === 'body-html' ? hidden : id === 'body-text' ? ta : null },
        Event: FakeEvent,
        mbxEditor: { getHtml: () => '<table><tr><td>x</td></tr></table>' },
        mbxEnsureTableBorders: (html) => html.replace(/<table/gi, '<table border="1"'),
    });
    vm.runInContext('(function () {' + HANDLER_BODY + '\n})()', c);
    assert.match(hidden.value, /<table border="1"/, '与 configRequest 同一净化口径');
});
