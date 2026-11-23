// 第九轮 P1/P2-1：撰写页里凡是程序改写隐藏表单域的操作，都必须派发可冒泡的
// input 事件，否则 capture 阶段的 dirty 标记与 body 冒泡阶段的 5s autosave
// 都看不到变更——用户加了附件/选了联系人，离开页面却静默丢失。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');

function coreOf(start, end) {
    const a = APP_JS.indexOf(start);
    const b = APP_JS.indexOf(end);
    assert.ok(a >= 0 && b > a, `app.js 必须带 core 锚点：${start}`);
    return APP_JS.slice(a, b);
}

class FakeEvent {
    constructor(type, init) {
        this.type = type;
        this.bubbles = !!(init && init.bubbles);
    }
}

function makeInput(initial) {
    return {
        value: initial,
        events: [],
        dispatchEvent(ev) { this.events.push(ev); },
        focus() {},
    };
}

function attachHarness(files = ['/tmp/报告.pdf']) {
    const attach = makeInput('[]');
    const keep = makeInput('[1,2]');
    const ul = { innerHTML: '' };
    const ctx = vm.createContext({
        document: {
            getElementById: (id) =>
                id === 'attach-paths' ? attach : id === 'keep-ids' ? keep : id === 'picked-list' ? ul : null,
            querySelector: () => null,
        },
        Event: FakeEvent,
        JSON,
        console,
        escapeHtml: (s) => s,
        MBX_IC: { clip: '' },
        window: { __TAURI__: { dialog: { open: async () => files } } },
    });
    const api = vm.runInContext(
        coreOf('// --- R9 attach dirty core START ---', '// --- R9 attach dirty core END ---') +
        '\n({ mbxPickAttachments, mbxRemovePicked, mbxRemoveKept, renderPicked })',
        ctx,
    );
    return { attach, keep, ul, api };
}

test('选文件后 attach-paths 收到冒泡 input 事件（第九轮 P1）', async () => {
    const h = attachHarness();
    await h.api.mbxPickAttachments();
    assert.deepEqual(JSON.parse(h.attach.value), ['/tmp/报告.pdf']);
    assert.equal(h.attach.events.length, 1);
    assert.equal(h.attach.events[0].type, 'input');
    assert.equal(h.attach.events[0].bubbles, true, '不冒泡则 body 上的 autosave 监听收不到');
});

test('移除待存附件同样触发冒泡 input', async () => {
    const h = attachHarness(['/tmp/a.pdf']);
    await h.api.mbxPickAttachments();
    h.attach.events.length = 0;
    h.api.mbxRemovePicked(0);
    assert.equal(h.attach.value, '[]');
    assert.equal(h.attach.events.length, 1);
    assert.equal(h.attach.events[0].type, 'input');
    assert.equal(h.attach.events[0].bubbles, true);
});

test('移除已入库附件：keep-ids 收到冒泡 input（第九轮 P1）', () => {
    const h = attachHarness();
    const li = { remove() { this.removed = true; } };
    const btn = { getAttribute: () => null, closest: () => li };
    h.api.mbxRemoveKept(2, btn);
    assert.equal(h.keep.value, '[1]');
    assert.equal(h.keep.events.length, 1);
    assert.equal(h.keep.events[0].type, 'input');
    assert.equal(h.keep.events[0].bubbles, true);
});

test('服务端回填的 renderPicked(false) 不派发，避免触发下一轮 autosave', () => {
    const h = attachHarness();
    h.api.renderPicked(false);
    assert.equal(h.attach.events.length, 0, '回填不是用户编辑，不能再调度 autosave');
});

test('联系人建议选中派发的 input 必须冒泡（第九轮 P2-1）', () => {
    const ctx = vm.createContext({ Event: FakeEvent, JSON, console });
    const pick = vm.runInContext(
        coreOf('// --- R9 contact pick core START ---', '// --- R9 contact pick core END ---') +
        '\nmbxContactPick',
        ctx,
    );
    const inp = makeInput('z');
    const state = {
        items: [{ name: '张三', addr: 'zhang@example.com' }],
        box: { hidden: false },
    };
    pick(inp, state, 0);
    assert.equal(inp.value, '张三 <zhang@example.com>, ');
    assert.equal(inp.events.length, 1);
    assert.equal(inp.events[0].type, 'input');
    assert.equal(inp.events[0].bubbles, true, '不冒泡则 autosave 收不到选中结果');
});
