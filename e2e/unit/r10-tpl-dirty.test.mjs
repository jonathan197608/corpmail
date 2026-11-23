// 第十轮 P3-1：模板编辑器从富文本切回 HTML 源码时，程序写 textarea 必须派发
// 可冒泡 input，否则设置页脏判定与基线不刷新（与编辑器 change 路径不对称）。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const a = APP_JS.indexOf('// --- R10 tpl dirty core START ---');
const b = APP_JS.indexOf('// --- R10 tpl dirty core END ---');
assert.ok(a >= 0 && b > a, 'app.js 必须带 R10 tpl dirty core 锚点');
const CORE = APP_JS.slice(a, b);

test('切回 HTML 源码派发冒泡 input（第十轮 P3-1）', () => {
    const events = [];
    const st = {
        mode: 'rich',
        ta: {
            value: '',
            hidden: true,
            dispatchEvent(ev) { events.push(ev); },
        },
        ed: { getHtml: () => '<p>正文</p>' },
        bar: { hidden: false },
        area: { hidden: false },
        srcBtn: { innerHTML: '' },
    };
    const ctx = vm.createContext({
        MBX_IC: { pencil: '', keyboard: '' },
        Event: class {
            constructor(type, init) { this.type = type; this.bubbles = !!(init && init.bubbles); }
        },
    });
    vm.runInContext(CORE, ctx);
    ctx.mbxTplToggleSrc(st);
    assert.equal(st.ta.value, '<p>正文</p>');
    assert.equal(events.length, 1, '程序写值必须派发 input');
    assert.equal(events[0].type, 'input');
    assert.equal(events[0].bubbles, true);
    assert.equal(st.mode, 'src');
});
