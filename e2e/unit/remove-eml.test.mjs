// 第八轮 P2-2：从草稿移除自动附加的 .eml 时，必须同步撤销「作为附件转发」意图，
// 否则下一次 autosave 表单仍带 attach_eml_from=1，服务端会把刚删的 .eml 重新加回。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const CORE = (() => {
    const a = APP_JS.indexOf('// --- P8 remove eml core START ---');
    const b = APP_JS.indexOf('// --- P8 remove eml core END ---');
    assert.ok(a >= 0 && b > a, 'app.js 必须带 P8 remove eml core 锚点');
    return APP_JS.slice(a, b);
})();

function harness({ isEml = false } = {}) {
    const keepInput = { value: '[1,2]' };
    const emlInput = { value: '9' };
    const li = { remove() { this.removed = true; } };
    const btn = {
        getAttribute: (name) => (name === 'data-eml-remove' && isEml ? '1' : null),
        closest: (sel) => (sel === 'li' ? li : null),
    };
    const ctx = vm.createContext({
        document: {
            getElementById: (id) => (id === 'keep-ids' ? keepInput : null),
            querySelector: (sel) => (sel === 'input[name="attach_eml_from"]' ? emlInput : null),
        },
        JSON,
        console,
    });
    vm.runInContext(CORE, ctx);
    ctx.mbxRemoveKept(2, btn);
    return { keepInput, emlInput, li };
}

test('移除普通附件：只从 keep_ids 剔除，不改 eml 意图', () => {
    const h = harness({ isEml: false });
    assert.equal(h.keepInput.value, '[1]', '目标附件须从 keep_ids 剔除');
    assert.equal(h.emlInput.value, '9', '普通附件不得清 eml 意图');
    assert.equal(h.li.removed, true, '行须移除');
});

test('移除自动附加的 .eml：同步把 attach_eml_from 清零（P2-2）', () => {
    const h = harness({ isEml: true });
    assert.equal(h.keepInput.value, '[1]');
    assert.equal(h.emlInput.value, '0', '移除 eml 必须撤销转发意图，防止下次 autosave 复活');
});
