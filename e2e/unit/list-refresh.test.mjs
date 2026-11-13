// C15：列表定向刷新的行为测试。
//
// 旧实现三处缺口：URL 直接取 location.search（忽略搜索框里未提交的词）、
// 只换 #mail-list 却不重建行勾选（「已选 N 封」但行全空，批量删除作用于
// 看不见的行）、.then() 没有 .catch()（刷新失败静默）。这里把 app.js 的
// C15 core 原样放进 Node vm，用假 DOM / 假 htmx 驱动真实分支。
//
// 运行：node --test e2e/unit/list-refresh.test.mjs（e2e/run.sh 0/6 步自动跑）
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- C15 list refresh core START ---';
const END = '// --- C15 list refresh core END ---';

function harness({ search = '?folder=inbox&q=old', typed = '新词', ajax } = {}) {
    const calls = { urls: [], events: [], toasts: [] };
    const els = {
        'list-area': {},
        'ba-q': { value: 'old' },
        'ba-total': { value: '3' },
        'mail-list': { dataset: { total: '7' } },
    };
    const ctx = vm.createContext({
        URLSearchParams,
        location: { search },
        document: {
            getElementById: (id) => els[id] || null,
            querySelector: (sel) =>
                sel === 'form.search input[name="q"]' ? { value: typed } : null,
            body: { dispatchEvent: (e) => calls.events.push(e.type) },
        },
        CustomEvent: class {
            constructor(type) { this.type = type; }
        },
        htmx: {
            ajax: (method, url) => {
                calls.urls.push({ method, url });
                return ajax ? ajax() : Promise.resolve();
            },
        },
        mbxToast: (text, cls, stayMs) => calls.toasts.push({ text, cls, stayMs }),
    });
    const start = APP_JS.indexOf(START);
    const end = APP_JS.indexOf(END);
    assert.ok(start >= 0 && end > start, 'app.js 必须带 C15 core 锚点');
    vm.runInContext(APP_JS.slice(start, end), ctx);
    return { ctx, calls, els };
}

test('刷新 URL 带未提交搜索词，清空时删掉过期 q，其余参数保留', () => {
    const h = harness();
    assert.equal(
        h.ctx.mbxRefreshQuery('?folder=inbox&q=old', '新词'),
        '?folder=inbox&q=%E6%96%B0%E8%AF%8D',
    );
    assert.equal(h.ctx.mbxRefreshQuery('?folder=inbox&q=old', ''), '?folder=inbox');
    assert.equal(h.ctx.mbxRefreshQuery('', ''), '');
    assert.equal(
        h.ctx.mbxRefreshQuery('?per=20&sort=date_asc', 'x'),
        '?per=20&sort=date_asc&q=x',
        'folder/per/sort 等筛选上下文不得丢',
    );
});

test('勾选重建：ids 只保留仍在本页的行，all 模式按 exclude 反选', () => {
    const h = harness();
    assert.equal(
        Array.from(h.ctx.mbxSelectionPlan({ mode: 'ids', ids: [1, 3] }, [1, 2, 3, 4])).join(','),
        'true,false,true,false',
        '新出现的行（2/4）不得被自动选中',
    );
    assert.equal(
        Array.from(h.ctx.mbxSelectionPlan({ mode: 'all', exclude: [2] }, [1, 2, 3])).join(','),
        'true,false,true',
    );
});

test('刷新带当前搜索词、同步批量上下文、更新总数并派发重建事件', async () => {
    const h = harness();
    await h.ctx.mbxRefreshList();
    assert.equal(h.calls.urls.length, 1);
    assert.equal(h.calls.urls[0].method, 'GET');
    assert.equal(
        h.calls.urls[0].url,
        '/list/rows?folder=inbox&q=%E6%96%B0%E8%AF%8D',
        '不得沿用 location.search 里的旧 q',
    );
    assert.equal(h.els['ba-q'].value, '新词', '批量栏 q 必须与显示的行一致');
    assert.equal(h.els['ba-total'].value, '7', '总数应跟随行片段 data-total');
    assert.ok(
        h.calls.events.includes('mbx:listRowsRefreshed'),
        '必须通知批量状态机按 stateBox 重建勾选',
    );
    assert.equal(h.calls.toasts.length, 0);
});

test('刷新失败落 toast，不再静默', async () => {
    const h = harness({ ajax: () => Promise.reject(new Error('network')) });
    await h.ctx.mbxRefreshList();
    assert.equal(h.calls.toasts.length, 1);
    assert.match(h.calls.toasts[0].text, /刷新失败/);
    assert.equal(h.calls.toasts[0].cls, 'err');
    assert.equal(h.calls.events.length, 0, '失败时不得派发重建事件');
});

test('不在列表页时不发请求', () => {
    const h = harness();
    h.els['list-area'] = null;
    h.ctx.mbxRefreshList();
    assert.equal(h.calls.urls.length, 0);
});
