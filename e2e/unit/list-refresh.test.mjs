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

test('A11：mbxRefreshSide 的 htmx.ajax reject 有 toast 兜底（无 unhandledRejection）', async () => {
    // C15 锚点外的侧栏刷新：直接截取整段源码进 vm，替换 htmx.ajax 为 reject
    const fnStart = APP_JS.indexOf('function mbxRefreshSide()');
    assert.ok(fnStart > 0, 'mbxRefreshSide 必须存在');
    const fnEnd = APP_JS.indexOf('\n}\n', fnStart) + 3; // 顶层函数收尾 + 空行
    const src = APP_JS.slice(fnStart, fnEnd);
    assert.match(src, /\.catch/, 'A11：mbxRefreshSide 必须带 .catch 兜底');

    let toastMsg = null;
    let rejectionUnhandled = false;
    const ctx = vm.createContext({
        document: {
            querySelector: (sel) =>
                sel === 'aside.side .folder.on'
                    ? { getAttribute: () => '/list?folder=sent' }
                    : sel === 'aside.side' ? {} : null,
        },
        htmx: {
            ajax: () => Promise.reject(new Error('协议层失败')),
        },
        mbxToast: (m) => { toastMsg = m; },
    });
    ctx.addEventListener = () => {};
    // R9：只移除本用例新增的监听器——removeAllListeners 会误删 node:test 自身的
    const onReject = () => { rejectionUnhandled = true; };
    process.on('unhandledRejection', onReject);
    try {
        vm.runInContext(src + '\nmbxRefreshSide();', ctx);
        // 等微任务队列排空（reject 的 catch 在 Promise 链上）
        await new Promise((r) => setTimeout(r, 20));
        assert.ok(!rejectionUnhandled, '不得产生 unhandledRejection');
        assert.ok(toastMsg && toastMsg.includes('侧栏刷新失败'), `应有 toast（实际 ${toastMsg}）`);
    } finally {
        process.removeListener('unhandledRejection', onReject);
    }
});

test('跨页全选：计数 total-exclude、提交 select_all + exclude_ids（第六轮护栏）', () => {
    const START2 = '// --- B10 batch selection core START ---';
    const END2 = '// --- B10 batch selection core END ---';
    const c = vm.createContext({ URLSearchParams });
    vm.runInContext(APP_JS.slice(APP_JS.indexOf(START2), APP_JS.indexOf(END2)), c);
    const count = vm.runInContext('mbxBatchSelectedCount', c);
    const params = vm.runInContext('mbxBatchSelectionParams', c);

    const all = { mode: 'all', exclude: [3, 7], total: 10, ids: [] };
    assert.equal(count(all), 8, 'all 模式计数 = 总数 - 排除数');
    const p = params(all, new URLSearchParams());
    assert.equal(p.get('select_all'), '1');
    assert.equal(p.get('exclude_ids'), '[3,7]', '排除集合须原样提交');
    assert.equal(p.has('ids'), false, 'all 模式不得同时带 ids');

    // 排除数超过总数时计数钳到 0，不出现负数
    assert.equal(count({ mode: 'all', exclude: [1, 2, 3], total: 2, ids: [] }), 0);

    const ids = { mode: 'ids', ids: [1, 3], exclude: [], total: 10 };
    assert.equal(count(ids), 2, 'ids 模式计数 = 勾选数');
    const p2 = params(ids, new URLSearchParams());
    assert.equal(p2.get('ids'), '[1,3]');
    assert.equal(p2.has('select_all'), false);
});

test('app.js 批量状态机确实调用抽出的纯函数（不是各写一份）', () => {
    assert.ok(
        APP_JS.includes('return mbxBatchSelectedCount(stateBox);') &&
            APP_JS.includes('mbxBatchSelectionParams(stateBox, params);'),
        'selectedCount/apply 必须调用 B10 core 纯函数，否则测试覆盖的是镜像副本'
    );
});

// R17 P2：批量操作 htmx.ajax 无 .catch —— 传输层失败（onerror/timeout，
// 不到服务端故不触发全局 responseError）时 unhandledRejection + 界面无反应。
test('R17 批量操作 htmx.ajax 带 reject 兜底', () => {
    const at = APP_JS.indexOf("htmx.ajax('POST', '/emails/batch'");
    assert.ok(at >= 0, '批量提交点存在');
    const end = APP_JS.indexOf('};', at);
    const branch = APP_JS.slice(at, end > at ? end : at + 400);
    assert.match(branch, /\.catch\(/, '批量提交必须捕获 reject（网络层失败兜底）');
    assert.match(branch, /mbxToast/, '失败须 toast 告知用户');
});
