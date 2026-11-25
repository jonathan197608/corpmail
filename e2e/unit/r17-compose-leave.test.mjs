// R17 P1：撰写页 leaving 标志在「请求未发出」路径卡死 → beforeunload
// 放行，未保存正文静默丢失。与 R14 设置页同款缺口：旧实现 click 阶段置位，
// 但撰写页 configRequest 有两条 preventDefault 路径（收件人为空/空正文确认
// 点「返回修改」），请求根本没发、beforeRequest 未触达。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const APP = readFileSync(
  path.join(path.dirname(fileURLToPath(import.meta.url)), '../../ui-dist/app.js'),
  'utf8'
);
const START = '// --- R17 compose leave core START ---';
const END = '// --- R17 compose leave core END ---';

function harness() {
  const start = APP.indexOf(START);
  const end = APP.indexOf(END);
  assert.ok(start >= 0 && end > start, 'app.js 必须带 R17 compose leave core 锚点');
  const core = APP.slice(start, end);

  const listeners = { document: {}, body: {}, window: {} };
  function add(store, type, fn) {
    (store[type] = store[type] || []).push(fn);
  }
  function fire(store, type, detail, extra) {
    const ev = Object.assign({ type, detail: detail || {}, preventDefault() {} }, extra || {});
    for (const fn of store[type] || []) fn(ev);
  }
  const form = {
    contains: () => true,
    querySelector: () => null,
  };
  const hint = { closest: () => form };
  const document = {
    readyState: 'complete',
    getElementById: (id) => (id === 'autosave-hint' ? hint : null),
    addEventListener: (t, f) => add(listeners.document, t, f),
    body: {
      addEventListener: (t, f) => add(listeners.body, t, f),
    },
  };
  const window = { addEventListener: (t, f) => add(listeners.window, t, f) };
  const ctx = {
    document,
    window,
    setTimeout: (fn) => { fn(); return 1; },
    MBX_IC: { dot: '' },
    mbxConfirmLeave: () => {},
    Event: function () {},
  };
  vm.createContext(ctx);
  vm.runInContext(core, ctx);

  return {
    // 模拟用户输入（置 edited=true）
    edit() {
      fire(listeners.body, 'input', { target: {} });
      fire(listeners.body, 'change', { target: {} });
    },
    // 模拟点发送/存草稿按钮（旧实现在这里就置 leaving，请求未必发出）
    clickSave() {
      const btn = {
        closest(sel) {
          return typeof sel === 'string' && sel.indexOf('button[') >= 0 ? btn : null;
        },
      };
      fire(listeners.document, 'click', {}, { target: btn });
    },
    // 模拟真实发出的请求（htmx:beforeRequest 带 requestPath）
    beforeRequest(requestPath) {
      fire(listeners.body, 'htmx:beforeRequest', {
        elt: form,
        pathInfo: { requestPath },
      });
    },
    responseError() { fire(listeners.body, 'htmx:responseError', {}); },
    invalid() { fire(listeners.document, 'invalid', {}); },
    // 返回 beforeunload 是否弹确认（ev.preventDefault 被调用）
    unloadBlocks() {
      let blocked = false;
      for (const fn of listeners.window.beforeunload || []) {
        fn({ preventDefault() { blocked = true; }, returnValue: '' });
      }
      return blocked;
    },
  };
}

test('R17 仅 click 未发请求时 beforeunload 必须仍提醒（不卡 leaving）', () => {
  const h = harness();
  h.edit();
  assert.equal(h.unloadBlocks(), true, '前置：脏正文应提醒');
  // 关键：必须真的派发 click。旧实现在 click 阶段置 leaving → 下一句
  // beforeunload 直接 return、不提醒；只调两次 unloadBlocks 是假绿。
  h.clickSave();
  assert.equal(h.unloadBlocks(), true, '仅点击未发请求不得放行（旧实现在此失败）');
});

test('R17 请求失败/校验拦截复位 leaving，beforeunload 恢复提醒', () => {
  const h = harness();
  h.edit();
  h.beforeRequest('/compose/send');
  assert.equal(h.unloadBlocks(), false, '请求发出后放行');
  h.responseError();
  assert.equal(h.unloadBlocks(), true, '响应失败须复位，恢复提醒');
  h.beforeRequest('/compose/send');
  h.invalid();
  assert.equal(h.unloadBlocks(), true, 'HTML5 校验拦截须复位');
});

test('R17 autosave 请求不得置 leaving', () => {
  const h = harness();
  h.edit();
  h.beforeRequest('/compose/autosave');
  assert.equal(h.unloadBlocks(), true, '自动保存不是已保存意图');
});
