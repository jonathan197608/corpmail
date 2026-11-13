// C19：迁移看门狗的**行为**测试。
//
// 旧护栏全是 APP_JS.contains(...) 静态断言：把 mbxMigStartWatchdog 里的三元阈值
// 改坏、或删掉进度事件复位，常量字符串还在、Rust 单测全绿，但慢盘会在 60s
// 时被误判成卡死。这里把 app.js 的 C19 watchdog core 原样放进 Node vm 执行，
// 用假时钟 + 手动驱动 setInterval 断言真实分支。
//
// 运行：node --test e2e/unit/migration-watchdog.test.mjs（e2e/run.sh 0/6 步自动跑）
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- C19 watchdog core START ---';
const END = '// --- C19 watchdog core END ---';

function watchdogCore() {
    const start = APP_JS.indexOf(START);
    const end = APP_JS.indexOf(END);
    assert.ok(start >= 0 && end > start, 'app.js 必须带 C19 watchdog core 锚点');
    return APP_JS.slice(start, end);
}

function harness() {
    let tick = null;
    let now = 0;
    let seq = 0;
    const calls = { prompt: 0, fail: 0, cleared: 0, registered: 0 };
    const ctx = vm.createContext({
        // 核心块读写但不声明的状态（在 app.js 里由外层 let 提供）
        mbxMigWatchdog: null,
        mbxMigPhase: '',
        mbxMigLastEvent: 0,
        Date: { now: () => now },
        setInterval: (fn) => {
            tick = fn;
            calls.registered += 1;
            return { id: ++seq };
        },
        clearInterval: () => { calls.cleared += 1; },
        mbxMigPromptStall: () => { calls.prompt += 1; },
        mbxMigShowError: () => { calls.fail += 1; },
    });
    vm.runInContext(watchdogCore(), ctx);
    return {
        ctx,
        calls,
        decision: (phase, elapsed) => ctx.mbxMigStallDecision(phase, elapsed),
        setNow: (v) => { now = v; },
        setPhase: (phase, lastEvent = 0) => {
            ctx.mbxMigPhase = phase;
            ctx.mbxMigLastEvent = lastEvent;
        },
        start: () => vm.runInContext('mbxMigStartWatchdog();', ctx),
        tick: () => {
            assert.ok(tick, '看门狗未注册 interval');
            tick();
        },
    };
}

test('决策函数：snapshot 30 分钟才提醒，其余阶段 60s 判死', () => {
    const h = harness();
    assert.equal(h.decision('snapshot', 1799999), null);
    assert.equal(h.decision('snapshot', 1800000), 'prompt');
    assert.equal(h.decision('attachments', 59999), null);
    assert.equal(h.decision('attachments', 60000), 'fail');
    assert.equal(h.decision('pointer', 60000), 'fail');
    assert.equal(h.decision('', 60000), 'fail');
    // C9：后台线程刚 spawn、validate_data_dir 尚未发出 attach 的窗口
    assert.equal(h.decision('starting', 60000), null, '启动宽限期不得按 60s 判死');
    assert.equal(h.decision('starting', 1799999), null);
    assert.equal(h.decision('starting', 1800000), 'prompt');
});

test('starting 阶段超时同样只提醒、不判死', () => {
    const h = harness();
    h.setPhase('starting');
    h.start();
    h.setNow(60_000);
    h.tick();
    assert.equal(h.calls.fail, 0, 'C9：慢盘校验 60s 无事件属正常');
    assert.equal(h.calls.prompt, 0);
    h.setNow(1_800_000);
    h.tick();
    assert.equal(h.calls.prompt, 1);
    assert.equal(h.calls.fail, 0);
});

test('snapshot 超时：到点走确认态，绝不能切失败态', () => {
    const h = harness();
    h.setPhase('snapshot');
    h.start();
    h.setNow(1799999);
    h.tick();
    assert.equal(h.calls.prompt, 0, '差 1ms 也不该提前打扰用户');
    assert.equal(h.calls.fail, 0);
    h.setNow(1800000);
    h.tick();
    assert.equal(h.calls.prompt, 1, '30 分钟到点必须提醒');
    assert.equal(h.calls.fail, 0, '慢盘 VACUUM 不得判死');
    assert.ok(h.calls.cleared >= 1, '触发后必须停掉定时器，避免重复弹窗');
});

test('非快照阶段：60s 无进展走失败态，不弹确认态', () => {
    const h = harness();
    h.setPhase('attachments');
    h.start();
    h.setNow(59999);
    h.tick();
    assert.equal(h.calls.fail, 0);
    h.setNow(60000);
    h.tick();
    assert.equal(h.calls.fail, 1);
    assert.equal(h.calls.prompt, 0);
});

test('事件复位计时：elapsed 按 Date.now() - mbxMigLastEvent 计算', () => {
    const h = harness();
    h.setPhase('snapshot', 1_000_000);
    h.start();
    h.setNow(1_000_000 + 1_799_999);
    h.tick();
    assert.equal(h.calls.prompt, 0);
    // 再来一条进度事件（复位 lastEvent）后，不该立刻又提醒
    h.setPhase('snapshot', 1_000_000 + 1_799_999);
    h.tick();
    assert.equal(h.calls.prompt, 0);
    h.setNow(1_000_000 + 1_799_999 + 1_800_000);
    h.tick();
    assert.equal(h.calls.prompt, 1, '复位后必须再等满 30 分钟');
});

test('继续等待：重启看门狗会重新注册定时器', () => {
    const h = harness();
    h.setPhase('snapshot');
    h.start();
    h.setNow(1800000);
    h.tick();
    assert.equal(h.calls.prompt, 1);
    // 模拟 mbxMigKeepWaiting：复位 lastEvent 后重启
    h.setPhase('snapshot', 1800000);
    h.start();
    assert.equal(h.calls.registered, 2, '「继续等待」必须重新注册定时器');
    h.setNow(1800000 + 1799999);
    h.tick();
    assert.equal(h.calls.prompt, 1, '新一轮未满 30 分钟不得重复提醒');
    h.setNow(1800000 + 1800000);
    h.tick();
    assert.equal(h.calls.prompt, 2, '下一轮到点再问一次');
});
