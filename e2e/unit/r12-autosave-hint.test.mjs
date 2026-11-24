// 第十二轮回归：R11 P2-6 的 skipped 提示在 5s 防抖主路径被 trigger().then
// 的「已自动保存」覆盖。此测试直接抽取 app.js 中 autosave IIFE 的
// composeForm/clock/trigger 三个纯函数在 vm 中执行，断言最终 hint 文案。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');

function triggerCore() {
    const a = APP_JS.indexOf('    function composeForm() {');
    const b = APP_JS.indexOf('    function schedule() {');
    assert.ok(a > 0 && b > a, 'autosave IIFE 锚点存在');
    return APP_JS.slice(a, b);
}

function run(result) {
    const hint = { innerHTML: '' };
    const form = { isConnected: true };
    hint.closest = () => form;
    const c = vm.createContext({
        document: { getElementById: (id) => (id === 'autosave-hint' ? hint : null) },
        mbxAutosaveRequest: () => Promise.resolve(result),
        htmx: {},
        escapeHtml: (s) => String(s),
        MBX_IC: { check: '✓' },
        Date,
        console,
    });
    vm.runInContext(triggerCore() + '\ntrigger();', c);
    return hint;
}

test('skipped>0 时最终提示必须是「已跳过」，不被「已自动保存」覆盖', async () => {
    const hint = run({ ok: true, skipped: 2 });
    await Promise.resolve(); // 让 .then 微任务跑完
    assert.match(hint.innerHTML, /2 个空\/损坏附件已跳过/, 'skipped 必须可见');
    assert.doesNotMatch(hint.innerHTML, /已自动保存/, '不能覆盖为假成功');
});

test('skipped=0 时仍显示正常已保存', async () => {
    const hint = run({ ok: true, skipped: 0 });
    await Promise.resolve();
    assert.match(hint.innerHTML, /已自动保存/);
    assert.doesNotMatch(hint.innerHTML, /跳过/);
});

test('写失败仍显示失败提示（不被 skipped 分支吞掉）', async () => {
    const hint = run({ ok: false });
    await Promise.resolve();
    assert.match(hint.innerHTML, /自动保存失败/);
});
