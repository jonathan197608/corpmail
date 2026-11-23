// 第七轮 P2-4：Esc 关附件预览时不得连带触发快捷键模块的「返回列表」。
// 两个 handler 都挂在 document 上、按注册顺序同步执行：预览模块先关 modal
// （hidden=true），快捷键模块随后读到 previewOpen=false 就会 back.click()。
// 用 stopImmediatePropagation 让本次 Esc 止于预览模块。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- P7 esc preview core START ---';
const END = '// --- P7 esc preview core END ---';
const CORE = (() => {
    const a = APP_JS.indexOf(START);
    const b = APP_JS.indexOf(END);
    assert.ok(a >= 0 && b > a, 'app.js 必须带 P7 esc preview core 锚点');
    return APP_JS.slice(a, b);
})();

function harness({ previewOpen = true } = {}) {
    const calls = { close: 0, back: 0, stopped: 0 };
    const modal = { hidden: !previewOpen };
    const handlers = [];
    const doc = {
        getElementById: (id) => (id === 'preview-modal' ? modal : null),
        addEventListener: (type, fn) => {
            if (type === 'keydown') handlers.push(fn);
        },
        querySelector: (sel) =>
            sel === '.topbar .back' ? { click: () => { calls.back++; } } : null,
    };
    const ctx = vm.createContext({
        document: doc,
        mbxClosePreview: () => { calls.close++; modal.hidden = true; },
        console,
    });
    vm.runInContext(CORE, ctx);
    // 模拟快捷键模块（注册在预览模块之后）的 Esc 返回分支
    vm.runInContext(`
        document.addEventListener('keydown', function (ev) {
            if (ev.metaKey || ev.ctrlKey || ev.altKey) return;
            var m = document.getElementById('preview-modal');
            var previewOpen = m && !m.hidden;
            if (ev.key === 'Escape') {
                if (previewOpen) return;
                var back = document.querySelector('.topbar .back');
                if (back) { ev.preventDefault(); back.click(); }
            }
        });
    `, ctx);
    return { calls, dispatch(key) {
        const ev = {
            key,
            stopImmediatePropagation() { calls.stopped++; this.__stopped = true; },
            preventDefault() {},
        };
        // 真实事件分发：按注册顺序广播，stopImmediatePropagation 后中止
        for (const fn of handlers) {
            if (ev.__stopped) break;
            fn(ev);
        }
    } };
}

test('预览打开时 Esc 只关预览，不退回列表', () => {
    const h = harness({ previewOpen: true });
    h.dispatch('Escape');
    assert.equal(h.calls.close, 1, '须关闭预览');
    assert.equal(h.calls.stopped, 1, '须阻止事件继续传播');
    assert.equal(h.calls.back, 0, '不得同时退回列表');
});

test('预览未打开时 Esc 正常退回列表', () => {
    const h = harness({ previewOpen: false });
    h.dispatch('Escape');
    assert.equal(h.calls.close, 0, '无预览可关');
    assert.equal(h.calls.stopped, 0, '不消费事件');
    assert.equal(h.calls.back, 1, '须退回列表');
});

test('非 Esc 键不影响两条分支', () => {
    const h = harness({ previewOpen: true });
    h.dispatch('j');
    assert.equal(h.calls.close, 0);
    assert.equal(h.calls.back, 0);
});
