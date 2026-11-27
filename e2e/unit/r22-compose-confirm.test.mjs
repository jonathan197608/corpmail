// R22 前端状态机回归：定时捕获与发送确认的置位/清理、迁移确认单槽替换、
// 看门狗超时失败态出口、模板富文本空内容归一。
// 用 Node vm 原样驱动 app.js 的 core 段——静态 includes 挡不住调用点漂移。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const slice = (startMark, endMark) => {
    const a = APP_JS.indexOf(startMark);
    const b = APP_JS.indexOf(endMark, a);
    assert.ok(a >= 0 && b > a, `缺少锚点 ${startMark} / ${endMark}`);
    return APP_JS.slice(a, b);
};
const functionSource = (name) => {
    const start = APP_JS.indexOf('function ' + name + '(');
    assert.ok(start >= 0, `app.js 必须含函数 ${name}`);
    let depth = 0;
    let opened = false;
    for (let i = start; i < APP_JS.length; i++) {
        if (APP_JS[i] === '{') { depth += 1; opened = true; }
        else if (APP_JS[i] === '}') {
            depth -= 1;
            if (opened && depth === 0) return APP_JS.slice(start, i + 1);
        }
    }
    assert.fail(`函数 ${name} 未闭合`);
};

function composeHarness(pickerValue) {
    const listeners = [];
    const confirms = [];
    const notes = [];
    const schedHidden = { id: 'sched-at', value: '' };
    const schedInput = { id: 'sched-input', value: pickerValue, min: '' };
    const card = {
        firstChild: null,
        querySelector: () => null,
        insertBefore: (n) => notes.push(n.textContent),
    };
    const form = {
        querySelector: (sel) => {
            if (sel === '#keep-ids') return { value: '[1]' };
            if (sel === '#body-mode') return { value: 'text' };
            if (sel === '#body-text') return { value: '' };
            const m = /^\[name="(.+)"\]$/.exec(sel);
            if (m) return { value: m[1] === 'to_addrs' ? 'a@b.c' : '' };
            return null;
        },
        closest: () => card,
    };
    const elt = {
        id: 'sched-send-btn',
        dataset: {},
        matches: (s) => s === '#sched-send-btn' || s === 'button[hx-post="/compose/send"]',
        closest: (s) => (s === 'form' ? form : null),
        click: () => {},
    };
    const ctx = vm.createContext({
        document: {
            body: { addEventListener: (type, fn) => { (listeners[type] ||= []).push(fn); } },
            getElementById: (id) => (id === 'sched-at' ? schedHidden : id === 'sched-input' ? schedInput : null),
            createElement: () => ({
                className: '', textContent: '', innerHTML: '',
                querySelector: () => null,
            }),
            querySelector: () => null,
        },
        mbxEditor: null,
        pickedPaths: [],
        mbxSchedLocalMin: () => '2030-01-01T00:00',
        mbxSchedValidationError: () => '',
        mbxConfirmLeave: (msg, onConfirm) => { confirms.push(msg); onConfirm(); },
        setTimeout: () => 0,
        Date,
    });
    // 两个 core 按生产顺序注册到同一 body：sched capture 先，precheck 后
    vm.runInContext(slice('// --- R10 sched capture core START ---', '// --- R10 sched capture core END ---'), ctx);
    vm.runInContext(slice('// --- C14 compose confirm core START ---', '// --- C14 compose confirm core END ---'), ctx);
    return {
        elt, confirms, notes,
        fire() {
            const ev = {
                detail: { elt, parameters: {} },
                defaultPrevented: false,
                preventDefault() { this.defaultPrevented = true; },
            };
            for (const fn of listeners['htmx:configRequest'] || []) fn(ev);
            return ev.defaultPrevented;
        },
    };
}

test('定时缺失被捕获时不弹空正文确认，且清掉已置位的确认标志', () => {
    const h = composeHarness('');
    assert.equal(h.fire(), true, 'sched capture 必须拦下请求');
    assert.equal(h.confirms.length, 0, '已被前序监听器取消的请求不得再弹确认');
    assert.deepEqual(h.elt.dataset, {}, '本次请求不得留下任何确认标志');

    // 旧实现：确认回调置位后 550ms 重触发仍被 sched capture 拦下，标志残留
    h.elt.dataset.mbxBodyOk = '1';
    h.elt.dataset.mbxSubjOk = '1';
    assert.equal(h.fire(), true);
    assert.equal(h.elt.dataset.mbxBodyOk, undefined, '被拦下的重触发必须清正文确认标志');
    assert.equal(h.elt.dataset.mbxSubjOk, undefined, '被拦下的重触发必须清主题确认标志');
});

test('定时刻合法时确认流程照常放行（修复未误伤正常发送）', () => {
    const h = composeHarness('2030-01-01T08:00');
    assert.equal(h.fire(), true, '第一次应弹正文确认');
    assert.match(h.confirms[0], /正文为空/);
    assert.equal(h.elt.dataset.mbxBodyOk, '1');
    assert.equal(h.fire(), true, '第二次应弹主题确认');
    assert.equal(h.elt.dataset.mbxSubjOk, '1');
    assert.equal(h.fire(), false, '第三次正常放行');
    assert.equal(h.confirms.length, 2);
});

test('mbxConfirmLeave 单槽替换必须补偿旧模态的取消回调', () => {
    let stored = null;
    const calls = [];
    const mask = {
        innerHTML: '',
        hidden: false,
        addEventListener: () => {},
        querySelector: (s) => ({ textContent: '', addEventListener: () => {}, dataset: {} }),
        remove: () => calls.push('remove'),
    };
    const ctx = vm.createContext({
        document: {
            getElementById: () => stored,
            createElement: () => mask,
            body: { appendChild() {} },
        },
        mbxModalOpen: () => () => calls.push('release'),
    });
    vm.runInContext(functionSource('mbxConfirmLeave'), ctx);
    ctx.mbxConfirmLeave('第一次', () => {}, '确认', '取消', () => calls.push('cancel1'));
    stored = mask;
    assert.equal(typeof mask.__mbxOnCancel, 'function');
    ctx.mbxConfirmLeave('第二次', () => {}, '确认', '取消', () => calls.push('cancel2'));
    assert.ok(calls.includes('cancel1'), '被顶掉的旧确认必须 resolve(false)，否则迁移 Promise 悬挂');
    assert.ok(calls.indexOf('release') < calls.indexOf('remove'), '先释放焦点陷阱再移除节点');
    assert.ok(!calls.includes('cancel2'), '新模态的取消回调不得在注册时被误触发');
});

test('看门狗超时失败态只留返回出口，后端终态失败仍保留重试', () => {
    const retry = { hidden: false };
    const close = { hidden: false };
    const mask = {
        querySelector: (s) => (s.includes('retry') ? retry : close),
    };
    const ctx = vm.createContext({
        document: { getElementById: () => mask },
        mbxMigAbort: () => {},
        mbxMigHideActions: () => {},
    });
    vm.runInContext(functionSource('mbxMigShowError'), ctx);
    ctx.mbxMigShowError('后台仍在跑', true);
    assert.equal(retry.hidden, true, '超时态重试必撞 409，不能露出');
    assert.equal(close.hidden, false, '必须留下返回出口');
    ctx.mbxMigShowError('后端确认失败');
    assert.equal(retry.hidden, false, '后端终态失败仍可就地重试');

    const wd = slice('// --- C19 watchdog core START ---', '// --- C19 watchdog core END ---');
    assert.match(wd, /mbxMigShowError\([\s\S]*?,\s*true\);/, '看门狗超时调用点必须传 bgStillRunning=true');
});

test('富文本切源码时空段落归一为空串，有效内容原样保留', () => {
    const ctx = vm.createContext({});
    vm.runInContext(functionSource('mbxTplNormalizeHtml'), ctx);
    assert.equal(ctx.mbxTplNormalizeHtml('<p><br></p>'), '');
    assert.equal(ctx.mbxTplNormalizeHtml('<p>&nbsp;</p>'), '');
    assert.equal(ctx.mbxTplNormalizeHtml('<p>收到，我已核对</p>'), '<p>收到，我已核对</p>');
});

test('富→源切换真的调用归一函数（钉死调用点而非仅测纯函数）', () => {
    const ctx = vm.createContext({
        Event: class { constructor(type) { this.type = type; } },
        MBX_IC: { pencil: '✎' },
        mbxTplBuild: () => {},
    });
    vm.runInContext(
        functionSource('mbxTplNormalizeHtml') + '\n' + functionSource('mbxTplToggleSrc'),
        ctx,
    );
    const ta = { value: 'old', hidden: true, dispatchEvent() {} };
    const st = {
        mode: 'rich',
        ed: { getHtml: () => '<p><br></p>' },
        ta,
        bar: { hidden: true },
        area: { hidden: false },
        srcBtn: { innerHTML: '' },
    };
    ctx.mbxTplToggleSrc(st);
    assert.equal(ta.value, '', '切源码必须归一空壳 HTML，否则保存后插入空段落');
    assert.equal(st.mode, 'src');
});
