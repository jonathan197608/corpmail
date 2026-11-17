// 复查四 P3-1：撰写页定时时刻前端校验。后端已有 now+60s 兜底，
// 这里把同一口径的纯函数放进 Node vm 直接驱动，防止坏实现只靠后端整页跳回。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- P4 sched validation core START ---';
const END = '// --- P4 sched validation core END ---';

function core() {
    const start = APP_JS.indexOf(START);
    const end = APP_JS.indexOf(END);
    assert.ok(start >= 0 && end > start, 'app.js 必须带 P4 sched validation core 锚点');
    return APP_JS.slice(start, end);
}

const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(core(), sandbox);

function localInput(ms) {
    const d = new Date(ms);
    const p2 = (n) => String(n).padStart(2, '0');
    return d.getFullYear() + '-' + p2(d.getMonth() + 1) + '-' + p2(d.getDate())
        + 'T' + p2(d.getHours()) + ':' + p2(d.getMinutes());
}

test('空值由调用方另作提示，纯校验函数不误报', () => {
    assert.equal(sandbox.mbxSchedValidationError('', Date.now()), '');
});

test('过去时刻与 60 秒内都必须在前端拦下', () => {
    const now = new Date(2026, 10, 17, 15, 0, 0).getTime();
    assert.match(
        sandbox.mbxSchedValidationError(localInput(now - 3600_000), now),
        /晚于当前时间/
    );
    assert.match(
        sandbox.mbxSchedValidationError(localInput(now + 30_000), now),
        /晚于当前时间/
    );
    // datetime-local 精度到分钟：now+60s 边界值必须拒绝，与后端 <= now+60 一致
    assert.match(
        sandbox.mbxSchedValidationError(localInput(now + 60_000), now),
        /晚于当前时间/
    );
});

test('远期合法时刻放行，min 属性随当前时间滚动（秒非零也必须可选）', () => {
    const now = new Date(2026, 10, 17, 15, 0, 0).getTime();
    assert.equal(sandbox.mbxSchedValidationError(localInput(now + 3600_000), now), '');
    const min = sandbox.mbxSchedLocalMin(now);
    assert.match(min, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/);
    assert.ok(new Date(min).getTime() >= now + 60_000, 'min 不得早于 now+1 分钟');
    // 复查五 P2-3：秒数≠0 时 min 向上取整，控件允许的最小值必须能通过校验，
    // 否则用户选了 min 仍被拒，必须手动跳到下一分钟
    const skewed = new Date(2026, 10, 17, 15, 0, 30).getTime();
    const skewedMin = sandbox.mbxSchedLocalMin(skewed);
    assert.ok(new Date(skewedMin).getTime() >= skewed + 60_000, '秒非零时 min 需向上取整');
    assert.equal(
        sandbox.mbxSchedValidationError(skewedMin, skewed), '',
        '控件允许的 min 值不得被前端校验拒绝'
    );
});
