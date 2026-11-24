// R14：设置页 rebaseline/leaving 两处状态机回归测试。
// 旧实现分别有：pane 后代局部 swap 误洗基线（测试连接后保存按钮变灰、
// 切 tab 静默丢改动）、click 阶段置 leaving（required 拦截/请求失败时
// beforeunload 放行丢改动）。用真实生产 core 片段在 vm 中跑行为断言。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');

function core(start, end) {
    const a = APP_JS.indexOf(start);
    const b = APP_JS.indexOf(end);
    assert.ok(a >= 0 && b > a, `app.js 必须带锚点 ${start}`);
    return APP_JS.slice(a, b);
}

const REBASE = core('// --- R14 settings rebaseline core START ---', '// --- R14 settings rebaseline core END ---');
const LEAVING = core('// --- R14 settings leaving core START ---', '// --- R14 settings leaving core END ---');

// 用真实 core 构造可调用的 rebaselineAll，返回 setCalls 记录
function makeRebase() {
    const form = { value: '编辑后', querySelector: (sel) => sel === 'button[type="submit"]' ? {} : null };
    const ctx = {
        WeakMap,
        form,
        pane: { id: 'settings-pane' },
        setCalls: [],
        document: { getElementById: () => ctx.pane },
        mbxSnapshotForm: () => form.value,
    };
    // 生产 core 直接放进闭包，注入依赖桩
    vm.createContext(ctx);
    vm.runInContext(`
        var base = { has: function(){ return false; }, set: function(f, v){ setCalls.push(v); }, get: function(){} };
        var snapshot = mbxSnapshotForm;
        function paneForms() { return [form]; }
        function refresh(f) {}
        ${REBASE}
        globalThis.__run = rebaselineAll;
    `, ctx);
    return { run: ctx.__run, setCalls: ctx.setCalls, form };
}

test('R14: pane 后代局部 swap（smtp-result）不重建基线', () => {
    const { run, setCalls } = makeRebase();
    // 局部片段：是 pane 的后代（closest 命中），但自身不含 pane
    const smtpResult = {
        id: 'smtp-result',
        querySelector: () => null,
        closest: (sel) => sel === '#settings-pane' ? { id: 'settings-pane' } : null,
    };
    run(smtpResult);
    assert.equal(setCalls.length, 0, 'pane 内局部 swap 不得重建任何表单基线');
});

test('R14: pane 自身与含 pane 的祖先仍重建基线（保留既有能力）', () => {
    for (const scope of [
        { id: 'settings-pane', querySelector: () => null },
        { id: 'body', querySelector: (sel) => sel === '#settings-pane' ? {} : null },
    ]) {
        const { run, setCalls } = makeRebase();
        run(scope);
        assert.equal(setCalls.length, 1, `scope=${scope.id} 应重建基线`);
        assert.equal(setCalls[0], '编辑后');
    }
});

test('R14: leaving 只在请求真正发出时置位，校验失败/响应失败复位', () => {
    const handlers = {};
    const invalidHandlers = [];
    const ctx = {
        document: {
            body: { addEventListener(t, fn) { handlers[t] = fn; } },
            addEventListener(t, fn, capture) { if (capture) invalidHandlers.push(fn); },
        },
    };
    vm.createContext(ctx);
    vm.runInContext(`var leaving = false;
        globalThis.__get = function(){ return leaving; };
        ${LEAVING}`, ctx);
    const get = ctx.__get;
    const saveElt = { closest: (s) => s.includes('/save') ? {} : null };
    const nonSaveElt = { closest: () => null };

    assert.equal(get(), false, '初始未置位');
    handlers['htmx:beforeRequest']({ detail: { elt: nonSaveElt } });
    assert.equal(get(), false, '非保存请求不得置位');
    handlers['htmx:beforeRequest']({ detail: { elt: saveElt } });
    assert.equal(get(), true, '保存请求发出后置位');
    handlers['htmx:responseError']({});
    assert.equal(get(), false, '响应失败必须复位，保留 beforeunload 提醒');
    handlers['htmx:beforeRequest']({ detail: { elt: saveElt } });
    invalidHandlers.forEach((fn) => fn({}));
    assert.equal(get(), false, 'required 校验失败（invalid）必须复位');
    handlers['htmx:beforeRequest']({ detail: { elt: saveElt } });
    handlers['htmx:afterSwap']({ detail: { target: { id: 'smtp-result' } } });
    assert.equal(get(), true, '非 pane 的 swap 不得复位');
    handlers['htmx:afterSwap']({ detail: { target: { id: 'settings-pane' } } });
    assert.equal(get(), false, '成功换入 pane 后复位');
});
