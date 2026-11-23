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

test('输入事件确实调用失效函数（不是各写一份镜像）', () => {
    assert.ok(
        APP_JS.includes('mbxContactInvalidate(state);'),
        'input 监听必须调用 core 失效函数'
    );
});
