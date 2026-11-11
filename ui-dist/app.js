// CorpMail 前端辅助（M2：附件选择/另存，__TAURI__.dialog；设计 §2/§5.1）
// 全局 500ms 防抖由 htmx 触发器表达式承担（hx-trigger="... once:500ms"，设计 §7）

// ---------- 内联图标（2026-11-10）：与模板 icons.html 同几何 ----------
// 只拼接固定路径字符串（无用户输入）；样式由 .ic 类统一（1em、stroke 1.8、currentColor）。
// check/x/dot（2026-11-10 二批）：文本状态符号 ✓✗✕● 也改 SVG——不同字体
// 下线重与基线不一，统一后与功能图标视觉一致
var MBX_IC = (function () {
    function svg(body) {
        return '<svg class="ic" viewBox="0 0 24 24" aria-hidden="true" focusable="false">' + body + '</svg>';
    }
    return {
        clip: svg('<path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48"/>'),
        star: svg('<polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/>'),
        starFill: svg('<polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2" fill="currentColor" stroke="none"/>'),
        pencil: svg('<path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/>'),
        keyboard: svg('<rect x="2" y="6" width="20" height="12" rx="2"/><path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M6 14h.01M18 14h.01M9 14h6"/>'),
        check: svg('<polyline points="20 6 9 17 4 12"/>'),
        x: svg('<path d="M18 6 6 18"/><path d="m6 6 12 12"/>'),
        dot: svg('<circle cx="12" cy="12" r="5" fill="currentColor" stroke="none"/>')
    };
})();

// 通用离开确认模态：Tauri 的 macOS WKWebView（wry）未实现 JS confirm 的 UI 代理，
// window.confirm 只会返回 false 且不弹窗，故改用页面内模态（跨平台一致）。
// message 支持换行（CSS white-space: pre-line）；onLeave 在用户点确认按钮时调用。
// okLabel/cancelLabel 可选（默认「放弃并离开/继续编辑」，模板降级等场景可换文案）。
function mbxConfirmLeave(message, onLeave, okLabel, cancelLabel) {
    var old = document.getElementById('mbx-leave-confirm');
    if (old) old.remove();
    var mask = document.createElement('div');
    mask.id = 'mbx-leave-confirm';
    mask.className = 'mbx-leave-mask';
    mask.innerHTML = '<div class="mbx-leave-card" role="alertdialog" aria-modal="true">'
        + '<p></p>'
        + '<div class="mbx-leave-actions">'
        + '<button type="button" class="btn" data-leave-cancel></button>'
        + '<button type="button" class="btn danger" data-leave-ok></button>'
        + '</div></div>';
    mask.querySelector('p').textContent = message;
    mask.querySelector('[data-leave-cancel]').textContent = cancelLabel || '继续编辑';
    mask.querySelector('[data-leave-ok]').textContent = okLabel || '放弃并离开';
    document.body.appendChild(mask);
    function close() { if (mask.parentNode) mask.remove(); }
    mask.addEventListener('click', function (ev) {
        if (ev.target === mask) { close(); return; } // 点遮罩 = 取消
        if (ev.target.closest('[data-leave-cancel]')) { close(); return; }
        if (ev.target.closest('[data-leave-ok]')) { close(); if (onLeave) onLeave(); return; }
    });
    // Esc = 取消
    mask.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape') { ev.stopPropagation(); close(); }
    });
    var ok = mask.querySelector('[data-leave-ok]');
    if (ok) ok.focus();
}

// htmx 2.0.6 的 hx-confirm 硬编码调用 window.confirm（不读 htmx.config.confirm）
// ——在 Tauri macOS WKWebView 上恒返 false 且不弹窗，邮件删除/联系人删除/
// 立即清理/删除备份等全部 hx-confirm 按钮会静默失效（点了没反应）。
// htmx:confirm 事件 + 异步 issueRequest 是官方异步确认口：preventDefault
// 取消同步路径，页面内模态确认后直接 issueRequest 发起请求——不再重走
// 触发器（无防抖窗口、无死循环）。无 hx-confirm 的请求 question 为空、
// 必须 什么都不做（preventDefault 会误拦所有请求）
document.body.addEventListener('htmx:confirm', function (evt) {
    var question = evt.detail && evt.detail.question;
    if (!question) return;
    evt.preventDefault();
    var issue = evt.detail.issueRequest;
    mbxConfirmLeave(question, function () {
        if (issue) issue();
    }, '确定', '取消');
});

// 启动引导动画移除（M5：base.html 内联 #boot，脚本就绪即撤）
(function removeBoot() {
    const boot = document.getElementById('boot');
    if (boot) boot.remove();
})();

// 属性值可能用单引号包裹，' 必须一起转义（只转 " 时 ' onmouseover=… 可逃逸）
function escapeHtml(s) {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;')
            .replace(/>/g, '&gt;').replace(/"/g, '&quot;')
            .replace(/'/g, '&#39;');
}

// ---------- 撰写页：待保存附件（dialog 选路径，存草稿时随表单提交） ----------

let pickedPaths = [];

async function mbxPickAttachments() {
    try {
        const files = await window.__TAURI__.dialog.open({ multiple: true });
        if (!files) return;
        for (const f of (Array.isArray(files) ? files : [files])) {
            if (!pickedPaths.includes(f)) pickedPaths.push(f);
        }
        renderPicked();
    } catch (e) {
        console.error('[mbx] 打开文件对话框失败', e);
    }
}

function mbxRemovePicked(i) {
    pickedPaths.splice(i, 1);
    renderPicked();
}

function renderPicked() {
    const input = document.getElementById('attach-paths');
    if (input) input.value = JSON.stringify(pickedPaths);
    const ul = document.getElementById('picked-list');
    if (!ul) return;
    ul.innerHTML = pickedPaths.map((p, i) => {
        const name = p.split(/[\\/]/).pop();
        return `<li class="att-row picked"><span class="att-clip">${MBX_IC.clip}</span>` +
               `<span class="att-name">${escapeHtml(name)}</span>` +
               `<button type="button" class="btn small" onclick="mbxRemovePicked(${i})">移除</button></li>`;
    }).join('');
}

// 已入库附件的移除：从 keep_ids 剔除（存草稿时服务端裁剪）
function mbxRemoveKept(aid, btn) {
    const input = document.getElementById('keep-ids');
    try {
        const ids = JSON.parse(input.value || '[]');
        const idx = ids.indexOf(aid);
        if (idx >= 0) ids.splice(idx, 1);
        input.value = JSON.stringify(ids);
    } catch (e) {
        input.value = '[]';
    }
    btn.closest('li').remove();
}

// ---------- 详情页：附件另存为（dialog 选保存路径 → htmx POST） ----------
// 请求护栏（M5 防抖审计）：在途期间重复点击忽略
let mbxSaveBusy = false;

async function mbxSaveAttachment(emailId, attachId, filename) {
    if (mbxSaveBusy) return;
    mbxSaveBusy = true;
    try {
        const dest = await window.__TAURI__.dialog.save({ defaultPath: filename });
        if (!dest) return;
        await htmx.ajax('POST', `/emails/${emailId}/attachments/${attachId}`, {
            target: '#toast',
            swap: 'innerHTML',
            values: { dest }
        });
    } catch (e) {
        console.error('[mbx] 保存对话框失败', e);
    } finally {
        mbxSaveBusy = false;
    }
}

// 全部另存（A4）：选目录 → 服务端逐附件落盘（同名自动加序号，不覆盖）
async function mbxSaveAllAttachments(emailId) {
    if (mbxSaveBusy) return;
    mbxSaveBusy = true;
    try {
        const dir = await window.__TAURI__.dialog.open({ directory: true });
        if (!dir) return;
        await htmx.ajax('POST', `/emails/${emailId}/attachments/all`, {
            target: '#toast',
            swap: 'innerHTML',
            values: { dest_dir: dir }
        });
    } catch (e) {
        console.error('[mbx] 全部另存失败', e);
    } finally {
        mbxSaveBusy = false;
    }
}

// 导出 .eml（B4）：选保存文件 → 服务端按库内字段重建 RFC 5322 报文写盘。
// 默认文件名为主题清洗结果（服务端已算好；空主题回落 corpmail-{id}.eml）
let mbxExportBusy = false;

async function mbxExportEml(emailId, filename) {
    if (mbxExportBusy) return;
    mbxExportBusy = true;
    try {
        const dest = await window.__TAURI__.dialog.save({
            defaultPath: filename || `corpmail-${emailId}.eml`
        });
        if (!dest) return;
        await htmx.ajax('POST', `/emails/${emailId}/export-eml`, {
            target: '#toast',
            swap: 'innerHTML',
            values: { dest }
        });
    } catch (e) {
        console.error('[mbx] 导出 .eml 失败', e);
    } finally {
        mbxExportBusy = false;
    }
}

// 导出 Markdown：选保存文件 → 服务端组装 front matter + 正文 + 附件清单的
// MD 写盘（供 AI 分析处理）。与 .eml 导出同一交互模式，共用防抖标志。
async function mbxExportMd(emailId, filename) {
    if (mbxExportBusy) return;
    mbxExportBusy = true;
    try {
        const dest = await window.__TAURI__.dialog.save({
            defaultPath: filename || `corpmail-${emailId}.md`
        });
        if (!dest) return;
        await htmx.ajax('POST', `/emails/${emailId}/export-md`, {
            target: '#toast',
            swap: 'innerHTML',
            values: { dest }
        });
    } catch (e) {
        console.error('[mbx] 导出 Markdown 失败', e);
    } finally {
        mbxExportBusy = false;
    }
}

// 数据目录迁移（M5）：系统对话框选目录 → POST 迁移（在线快照，重启生效）。
// 响应为存储 pane 片段：成功显示"待生效目录"，失败显示错误区。
// 请求护栏（M5 防抖审计）：弹窗+请求全程只允许一次在途
let mbxMoveBusy = false;

async function mbxMoveDataDir() {
    if (mbxMoveBusy) return;
    mbxMoveBusy = true;
    try {
        const dir = await window.__TAURI__.dialog.open({ directory: true });
        if (!dir) return;
        await htmx.ajax('POST', '/settings/storage/move', {
            target: '#settings-pane',
            swap: 'outerHTML',
            values: { dest: dir }
        });
    } catch (e) {
        console.error('[mbx] 打开目录对话框失败', e);
    } finally {
        mbxMoveBusy = false;
    }
}

// 向导页数据目录选择（与设置·存储页 mbxMoveDataDir 同源，仅选目录不迁移——
// 迁移由向导保存时按表单值处理）：选中后写回表单 input，用户仍可手改
async function mbxPickSetupDir(btn) {
    try {
        const dir = await window.__TAURI__.dialog.open({ directory: true });
        if (!dir) return;
        const input = btn.closest('.port-row')?.querySelector('input[name="data_dir"]');
        if (input) input.value = dir;
    } catch (e) {
        console.error('[mbx] 打开目录对话框失败', e);
    }
}

// 打印邮件（B5）：@media print 只保留信头/正文/附件清单，系统对话框由 WebView 弹出
function mbxPrintEmail() {
    window.print();
}

// 附件预览（M5）：img / pdf / text 三类内嵌渲染
// text 类服务端强制 text/plain 返回（防 text/html 脚本执行），前端 pre 渲染

function mbxPreviewAttachment(url, kind, name) {
    const modal = document.getElementById('preview-modal');
    const body = document.getElementById('pm-body');
    const nameEl = document.getElementById('pm-name');
    if (!modal || !body) return;
    // 每次打开先复位 Office 专用状态（宽版面板/工具栏/渲染实例）
    if (window.mbxOfficeReset) window.mbxOfficeReset();
    if (nameEl) nameEl.textContent = name;
    body.innerHTML = '';
    if (kind === 'docx' || kind === 'xlsx' || kind === 'pptx' || kind === 'xls') {
        if (window.mbxPreviewOffice) {
            window.mbxPreviewOffice(url, kind, name);
        } else {
            body.textContent = '预览组件未加载，请重启应用后重试。';
        }
        modal.hidden = false;
        return;
    }
    if (kind === 'img') {
        const img = document.createElement('img');
        img.src = url;
        img.alt = name;
        body.appendChild(img);
    } else if (kind === 'pdf') {
        const frame = document.createElement('iframe');
        frame.src = url;
        frame.title = name;
        body.appendChild(frame);
    } else if (kind === 'text') {
        fetch(url)
            .then(r => r.ok ? r.text() : Promise.reject(new Error('HTTP ' + r.status)))
            .then(t => {
                const pre = document.createElement('pre');
                pre.className = 'pm-text';
                pre.textContent = t;
                body.appendChild(pre);
            })
            .catch(() => {
                body.textContent = '预览加载失败';
            });
    } else {
        // 未支持内嵌预览的格式：统一给出可预期反馈（M5 验收反馈）
        const tip = document.createElement('div');
        tip.className = 'pm-unsupported';
        tip.textContent = '该格式暂不支持内嵌预览，请使用"另存为"后用系统应用打开。';
        body.appendChild(tip);
    }
    modal.hidden = false;
}

function mbxClosePreview() {
    const modal = document.getElementById('preview-modal');
    if (!modal) return;
    if (window.mbxOfficeReset) window.mbxOfficeReset();
    modal.hidden = true;
    const body = document.getElementById('pm-body');
    if (body) body.innerHTML = '';  // 释放 img/iframe 资源
}

// Esc 关闭预览
document.addEventListener('keydown', ev => {
    if (ev.key === 'Escape') mbxClosePreview();
});

// 读信页操作下拉（R2/批次二）：回复▾ split 与 ⋯ 菜单
// 事件委托：点 ▾/⋯ 切换菜单；点菜单项后收起；Esc/点击外部关闭
document.addEventListener('click', ev => {
    const toggle = ev.target.closest('[data-split-toggle]');
    if (toggle) {
        const split = toggle.closest('.split');
        const menu = split && split.querySelector('.split-menu');
        if (menu) menu.hidden = !menu.hidden;
        ev.stopPropagation();
        return;
    }
    // 点击菜单项（链接/按钮）后收起当前打开的菜单
    if (ev.target.closest('.split-menu a, .split-menu button')) {
        const menu = ev.target.closest('.split-menu');
        if (menu) menu.hidden = true;
    } else {
        document.querySelectorAll('.split-menu:not([hidden])').forEach(m => { m.hidden = true; });
    }
});

// Esc 同时收起展开中的操作菜单
document.addEventListener('keydown', ev => {
    if (ev.key === 'Escape') {
        document.querySelectorAll('.split-menu:not([hidden])').forEach(m => { m.hidden = true; });
    }
});

// ---------- 收取状态：按钮/设置入口禁用 + 完成后局部刷新 ----------
// 状态片段轮询（收取中 2s / 空闲 10s）只更新自身文字；后台收取结束时服务端
// 下发一次 HX-Trigger 事件 mbx:fetchDone，由此按当前页面做定向局部刷新。
// 不做整页 location.reload()：Tauri 自定义协议下 reload 行为不稳定，
// 且会丢失正在撰写的草稿与阅读位置。
// data-running 标记由状态片段下发：收取中（含后台定时收取）禁用「收取新邮件」
// 按钮与侧栏设置入口（避免拉取中误入设置改范围/重拉等造成竞态）。
function mbxSyncFetchButton() {
    const status = document.getElementById('fetch-status');
    const running = !!(status && status.getAttribute('data-running') === 'true');
    const btn = document.getElementById('fetch-btn');
    if (btn) btn.disabled = running;
    const gear = document.querySelector('a.settings-entry');
    if (gear) {
        gear.classList.toggle('disabled', running);
        gear.title = running ? '收取中，完成后可进入设置' : '设置';
    }
}
// click 兜底：即使 focus/键盘等绕过 pointer-events，仍阻止 disabled 齿轮跳转
document.body.addEventListener('click', ev => {
    const gear = ev.target.closest('a.settings-entry');
    if (gear && gear.classList.contains('disabled')) ev.preventDefault();
});
document.body.addEventListener('htmx:afterSwap', mbxSyncFetchButton);
document.addEventListener('DOMContentLoaded', mbxSyncFetchButton);

// ---------- 设置·收件同步：「重新拉取」仅在首次拉取范围变化时出现 ----------
// 服务端把已保存范围写在 data-first-fetch-initial；单选变化与之不一致才
// 显示按钮（防误触全量重拉）。保存/重拉返回的新片段经 afterSwap 重新校准。
function mbxRefetchToggle() {
    const row = document.querySelector('[data-first-fetch-initial]');
    const btn = document.getElementById('refetch-row');
    if (!row || !btn) return;
    const sel = row.querySelector('input[name="first_fetch_days"]:checked');
    btn.hidden = !sel || sel.value === row.dataset.firstFetchInitial;
}
document.body.addEventListener('change', ev => {
    if (ev.target && ev.target.name === 'first_fetch_days') mbxRefetchToggle();
});
document.body.addEventListener('htmx:afterSwap', mbxRefetchToggle);
document.addEventListener('DOMContentLoaded', mbxRefetchToggle);

// 后台收取完成：刷新当前页的动态区域（列表片段 + 侧栏计数），URL 与滚动位置不动
function mbxOnFetchDone() {
    // 1) 列表页：按当前地址（含未读/外部/搜索/页码）取列表片段换入
    // 列表页（含顶栏"共 N 封"）：#list-area 存在即列表页——非列表页
    // （详情/撰写/设置）没有该容器，自动跳过，不影响正在阅读/撰写的内容
    const list = document.getElementById('list-area');
    if (list) {
        htmx.ajax('GET', location.pathname + location.search, {
            target: '#list-area', swap: 'outerHTML'
        });
    }
    // 2) 侧栏（计数/高亮/状态条）：folder 取侧栏当前高亮项，避免非列表页丢失
    const on = document.querySelector('aside.side .folder.on');
    let folder = 'inbox';
    if (on) {
        const m = /folder=([a-z]+)/.exec(on.getAttribute('href') || '');
        if (m) folder = m[1];
    }
    const side = document.querySelector('aside.side');
    if (side) {
        htmx.ajax('GET', '/side/frame?folder=' + folder, {
            target: 'aside.side', swap: 'outerHTML'
        });
    }
}
document.body.addEventListener('mbx:fetchDone', mbxOnFetchDone);

// ---------- 星标翻转（M2-1）：mbx:starDone 事件携带新态 ----------
// 后端 POST /emails/{id}/star 空体 + 事件头；前端按所在页面分派：
// - 列表页：星标置顶会重排行序 → 重拉当前列表片段换入（保持过滤/页码，
//   与收取完成同路径；侧栏计数不随星标变，一并刷新无副作用）
// - 详情页：无重排，就地翻转按钮态（星形图标 + on 类 + aria-pressed）
document.body.addEventListener('mbx:starDone', function (ev) {
    var starred = !!(ev.detail && ev.detail.starred);
    if (document.getElementById('list-area')) {
        htmx.ajax('GET', location.pathname + location.search, {
            target: '#list-area', swap: 'outerHTML'
        });
        return;
    }
    var btn = document.querySelector('.star-btn[data-mail-id]');
    if (!btn) return;
    btn.classList.toggle('on', starred);
    btn.setAttribute('aria-pressed', starred ? 'true' : 'false');
    btn.title = starred ? '取消星标（不再置顶）' : '星标：置顶显示';
    // 详情页按钮带文字（列表页按钮仅星形图标）、两形态统一一套文案
    if (btn.textContent.indexOf('星') >= 0) {
        btn.innerHTML = starred ? MBX_IC.starFill + ' 已星标' : MBX_IC.star + ' 星标';
    } else {
        btn.innerHTML = MBX_IC.starFill;
    }
});

// ---------- 列表页：发送后瞬时反馈（M1-6）+ 撤销发送窗口（A1） ----------
// 发送跳回列表携带 just_sent=N（compose_send 排入撤销窗口时附加）：
// - 前 8 秒：toast 显示倒计时 + 「撤销发送」（POST /undo-send 回撰写页）
// - 倒计时结束：转为"邮件发送中…"，轮询 /emails/{id}/status 至终态：
//   sent → "已发送"（ok）2.5s 后淡出；failed → 错误文案（err）停留 6s
// - 终态后刷新列表区+侧栏（复用收取完成同一刷新路径），"发送中"行落定
// - 1.2s 间隔轮询；30s 未终态（SMTP 卡顿）不再打扰，提示稍后到发件箱查看
// URL 参数即刻清除（replaceState 不触发导航），刷新页面不会重复提示
(function () {
    var params = new URLSearchParams(location.search);
    var id = params.get('just_sent');
    if (!id || !document.getElementById('list-area')) return;
    params.delete('just_sent');
    var clean = params.toString();
    history.replaceState(null, '', location.pathname + (clean ? '?' + clean : ''));
    var slot = document.getElementById('toast');
    if (!slot) return;
    var UNDO_SECS = 8;
    var left = UNDO_SECS;
    var toast = document.createElement('div');
    toast.className = 'toast busy';
    var label = document.createElement('span');
    label.textContent = '邮件已进入发送队列（' + left + ' 秒内可撤销）';
    toast.appendChild(label);
    var undo = document.createElement('button');
    undo.type = 'button';
    undo.className = 'btn small toast-undo';
    undo.textContent = '撤销发送';
    undo.addEventListener('click', function () {
        undo.disabled = true;
        // htmx 会跟随 HX-Redirect 整页跳到撰写页（继续编辑）
        htmx.ajax('POST', '/emails/' + id + '/undo-send', { target: 'body', swap: 'none' });
    });
    toast.appendChild(undo);
    slot.appendChild(toast);
    var started = Date.now();
    // icon 前缀仅允许本文件固定 SVG 字符串；文字统一 escapeHtml 后入 innerHTML
    // （toast 文案可能携带服务端错误信息，不能裸拼）
    var settle = function (text, cls, stayMs, icon) {
        toast.className = 'toast ' + cls;
        toast.innerHTML = (icon ? MBX_IC[icon] + ' ' : '') + escapeHtml(text);
        // 终态刷新：列表区（过滤/页码保持）+侧栏计数，复用收取完成路径
        if (window.mbxOnFetchDone) mbxOnFetchDone();
        setTimeout(function () { toast.remove(); }, stayMs);
    };
    var poll = function () {
        fetch('/emails/' + id + '/status', { cache: 'no-store' })
            .then(function (r) { return r.ok ? r.json() : null; })
            .then(function (s) {
                if (!s) return settle('发送状态查询失败，请在发件箱确认', 'err', 6000);
                if (s.status === 'sent') return settle('已发送', 'ok', 2500, 'check');
                if (s.status === 'failed') {
                    return settle('发送失败：' + (s.error || '未知原因'), 'err', 6000);
                }
                // queued/sending：继续轮询；超 30s 停止（SMTP 卡顿不动声色等太久）
                if (Date.now() - started > 30000) {
                    return settle('发送仍在进行，稍后可在发件箱确认', 'busy', 6000);
                }
                setTimeout(poll, 1200);
            })
            .catch(function () { return settle('网络异常，发送状态未知', 'err', 6000); });
    };
    // 撤销倒计时；到点后移除按钮并转入发送轮询
    var timer = setInterval(function () {
        left -= 1;
        if (left > 0) {
            label.textContent = '邮件已进入发送队列（' + left + ' 秒内可撤销）';
            return;
        }
        clearInterval(timer);
        if (undo.parentNode) undo.remove();
        label.textContent = '邮件发送中…';
        poll();
    }, 1000);
})();

// ---------- 撰写页：收件人/抄送联系人建议（模糊匹配姓名或地址） ----------
function mbxContactToken(inp) {
    const v = inp.value;
    const idx = Math.max(v.lastIndexOf(','), v.lastIndexOf(';'));
    return v.slice(idx + 1);
}

function mbxInitContacts() {
    // 密送框同样接入（M1-3 引入；与收/抄送同体验）
    document.querySelectorAll('input[name="to_addrs"], input[name="cc_addrs"], input[name="bcc_addrs"]').forEach(inp => {
        if (inp.dataset.mbxContacts === '1') return;
        inp.dataset.mbxContacts = '1';
        inp.setAttribute('autocomplete', 'off');
        const host = inp.parentElement;
        if (host) host.style.position = 'relative';
        const box = document.createElement('div');
        box.className = 'contacts-pop';
        box.hidden = true;
        (host || document.body).appendChild(box);
        const state = { box, items: [], active: -1, timer: 0 };

        inp.addEventListener('input', () => {
            clearTimeout(state.timer);
            state.timer = setTimeout(() => mbxContactQuery(inp, state), 200);
        });
        inp.addEventListener('keydown', ev => mbxContactKey(inp, state, ev));
        inp.addEventListener('blur', () => setTimeout(() => { box.hidden = true; }, 150));
        inp.addEventListener('focus', () => { if (state.items.length) box.hidden = false; });
    });
}

async function mbxContactQuery(inp, state) {
    const q = mbxContactToken(inp).trim();
    if (!q) { state.box.hidden = true; state.items = []; return; }
    try {
        const r = await fetch('/contacts/suggest?q=' + encodeURIComponent(q), { cache: 'no-store' });
        if (!r.ok) return;
        state.items = await r.json();
        // 默认高亮首项：输入姓名后直接回车即可选中（无需先按 ↓），
        // 与「快速输入收件人」的目标一致；Esc/Ctrl 不影响输入
        state.active = state.items.length ? 0 : -1;
        if (!state.items.length) { state.box.hidden = true; return; }
        state.box.innerHTML = state.items.map((c, i) => {
            const name = c.name || c.addr;
            const addr = c.name ? `<span class="ci-addr">${escapeHtml(c.addr)}</span>` : '';
            return `<div class="contacts-item" data-i="${i}"><span class="ci-name">${escapeHtml(name)}</span>${addr}</div>`;
        }).join('');
        state.box.querySelectorAll('.contacts-item').forEach(el => {
            el.addEventListener('mousedown', ev => {
                ev.preventDefault();
                mbxContactPick(inp, state, Number(el.dataset.i));
            });
        });
        mbxContactMark(state);
        state.box.hidden = false;
    } catch (e) {
        // 建议失败不打扰输入
    }
}

function mbxContactPick(inp, state, i) {
    const c = state.items[i];
    if (!c) return;
    const v = inp.value;
    const idx = Math.max(v.lastIndexOf(','), v.lastIndexOf(';'));
    const head = idx >= 0 ? v.slice(0, idx + 1) : '';
    const token = c.name ? `${c.name} <${c.addr}>` : c.addr;
    inp.value = head + (head ? ' ' : '') + token + ', ';
    state.box.hidden = true;
    state.items = [];
    inp.dispatchEvent(new Event('input'));
    inp.focus();
}

function mbxContactKey(inp, state, ev) {
    if (state.box.hidden || !state.items.length) return;
    if (ev.key === 'ArrowDown') {
        ev.preventDefault();
        state.active = (state.active + 1) % state.items.length;
        mbxContactMark(state);
    } else if (ev.key === 'ArrowUp') {
        ev.preventDefault();
        state.active = (state.active - 1 + state.items.length) % state.items.length;
        mbxContactMark(state);
    } else if (ev.key === 'Enter') {
        if (state.active >= 0) { ev.preventDefault(); mbxContactPick(inp, state, state.active); }
    } else if (ev.key === 'Escape') {
        state.box.hidden = true;
    }
}

function mbxContactMark(state) {
    state.box.querySelectorAll('.contacts-item').forEach((el, i) => {
        el.classList.toggle('on', i === state.active);
    });
}

document.addEventListener('DOMContentLoaded', mbxInitContacts);

// ---------- 设置页 tab 高亮（stab 自身不重渲染，点击时同步 .on） ----------
document.addEventListener("click", function (e) {
    var stab = e.target.closest(".stab");
    if (!stab) { return; }
    document.querySelectorAll(".settings-tabs .stab").forEach(function (s) { s.classList.remove("on"); });
    stab.classList.add("on");
});

// ---------- 向导"测试并保存"：进行中锁定表单，结束后恢复 ----------
// 双通道真实连接测试最长 ~15s（并行后）。htmx:configRequest触发时参数已收集完毕，
// 此处禁用 inputs 不影响本次提交序列化；afterRequest 无论成败都解锁，
// 保证失败重试（结果片段换入 #test-results 后表单必须可编辑）。
// 按钮禁用/恢复由 hx-disabled-elt 负责，此处只补文案与全表单锁定。
function mbxLockSetupForm(form, lock) {
    if (!form) return;
    form.querySelectorAll('input, select, textarea').forEach(function (el) {
        if (lock) {
            // 只标记"被我们禁用"的元素：天然 disabled（如有）解锁时不误开
            if (!el.disabled) {
                el.disabled = true;
                el.dataset.mbxLocked = '1';
            }
        } else if (el.dataset.mbxLocked) {
            el.disabled = false;
            delete el.dataset.mbxLocked;
        }
    });
    var back = form.querySelector('a.btn.link');
    if (back) back.classList.toggle('disabled', lock);
    var submit = form.querySelector('button[type=submit]');
    if (submit) {
        if (lock) {
            submit.dataset.mbxLabel = submit.textContent;
            submit.textContent = submit.dataset.busyLabel || '测试中…';
        } else if (submit.dataset.mbxLabel) {
            submit.textContent = submit.dataset.mbxLabel;
        }
    }
}

document.body.addEventListener('htmx:configRequest', function (ev) {
    var elt = ev.detail && ev.detail.elt;
    if (elt && elt.classList && elt.classList.contains('setup-card')) {
        mbxLockSetupForm(elt, true);
    }
});

document.body.addEventListener('htmx:afterRequest', function (ev) {
    var elt = ev.detail && ev.detail.elt;
    if (elt && elt.classList && elt.classList.contains('setup-card')) {
        mbxLockSetupForm(elt, false);
    }
});

// ---------- 撰写页：富文本编辑器（wangEditor 5，输出 HTML；纯文本只可单向升级） ----------
// 挂载/工具栏由编辑器渲染；提交时经 htmx:configRequest 把 getHtml() 注入参数。
var mbxEditor = null; // wangEditor 实例（富文本模式下存在）

function mbxReadInitialHtml() {
    const el = document.getElementById('rt-initial');
    return el ? el.value : '';
}

// 创建（或重建）编辑器：initial 为 HTML 字符串（已是净化后的安全内容）
function mbxBuildEditor(initial) {
    if (!window.wangEditor) return;
    // 升级/重建前销毁旧实例（editor 与 toolbar 成对销毁，防重复挂载）
    if (mbxEditor) {
        try { mbxEditor.destroy(); } catch (e) { /* 已销毁则忽略 */ }
        mbxEditor = null;
    }
    const { createEditor, createToolbar } = window.wangEditor;
    const editorConfig = {
        placeholder: '输入正文…（支持表格 / 颜色 / 列表 / 链接 / 图片）',
        MENU_CONF: {
            // 本地图片统一转 data URI 内嵌（企业内网无外置上传端点；
            // 保存/发送由后端净化管线只放行 data:image/*）
            uploadImage: {
                customUpload(file, insertFn) {
                    const reader = new FileReader();
                    reader.onload = () => insertFn(reader.result, file.name, '');
                    reader.readAsDataURL(file);
                }
            }
        }
    };
    try {
        mbxEditor = createEditor({
            selector: '#editor-container',
            html: initial,
            config: editorConfig,
            mode: 'default'
        });
        createToolbar({
            selector: '#toolbar-container',
            editor: mbxEditor,
            // 邮件场景不需要视频与全屏
            config: { excludeKeys: ['group-video', 'fullScreen'] },
            mode: 'default'
        });
    } catch (e) {
        console.error('编辑器初始化失败', e);
    }
}

// 纯文本 → 富文本（单向）：转义后按换行拆 <br> 作为初始内容；创建成功才切换显示
function mbxUpgradeHtml() {
    const ta = document.getElementById('body-text');
    const plainBox = document.getElementById('body-plain');
    const htmlBox = document.getElementById('body-html-box');
    const mode = document.getElementById('body-mode');
    let initial = '';
    if (ta) {
        const esc = ta.value
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;');
        initial = '<p>' + esc.replace(/\r?\n/g, '<br>') + '</p>';
    }
    mbxBuildEditor(initial);
    if (mbxEditor) {
        if (plainBox) plainBox.hidden = true;
        if (htmlBox) htmlBox.hidden = false;
        // 升级入口已移出 #body-plain（与插入模板同一工具行），需单独隐藏
        const tools = document.getElementById('plain-tools');
        if (tools) tools.hidden = true;
        if (mode) mode.value = 'html';
    }
}

// htmx 发请求前注入编辑器当前 HTML，并执行"超大内嵌图片转附件"策略：
// ≥2MB 的 data:image 从正文剥离为 inline_images 参数（后端落为附件），
// 编辑器内替换为占位说明，防止 base64 撑爆单封体积。
// 注：不能靠 submit 事件刷新 hidden——htmx 的 submit 处理先注册
//（defer 顺序 htmx→app.js），后注册的监听来不及更新字段值。
var MBX_INLINE_IMG_LIMIT = 2 * 1024 * 1024;

function mbxExtractOversizedInlineImages(html) {
    var converted = [];
    var out = html.replace(/<img[^>]*src="(data:image\/[^"]+)"[^>]*>/gi, function (tag, uri) {
        // data:<mime>;base64,<payload>
        var m = uri.match(/^data:(image\/[a-z0-9.+-]+);base64,(.*)$/i);
        if (!m) return tag;
        var payload = m[2];
        // base64 长度 ≈ 字节 * 4/3
        var approxBytes = Math.floor(payload.length * 3 / 4);
        if (approxBytes < MBX_INLINE_IMG_LIMIT) return tag;
        var ext = (m[1].split('/')[1] || 'png').split('+')[0];
        converted.push({ name: 'image.' + ext, mime: m[1], data: payload });
        return '<p>〔图片超过 2MB，已转为附件：image.' + ext + '〕</p>';
    });
    return { html: out, converted: converted };
}

function mbxMigrateInlineImages() {
    if (mbxEditor) {
        var r = mbxExtractOversizedInlineImages(mbxEditor.getHtml());
        if (r.converted.length > 0) {
            mbxEditor.setHtml(r.html);
            return r.converted;
        }
    }
    return [];
}

// 提交归属表单判定：evt.elt 可能是 form（存草稿 submit）也可能是发送按钮本身
//（type=button + hx-include="closest form" 的 click 触发）。旧实现用
// elt.querySelector('#body-html')：button 的 querySelector 只查其自身子树，
// 找不到兄弟节点里的编辑器 hidden——发送路径 body_html 从未注入成功，
// HTML 转发邮件以空正文发出（2026-11-03 修复）。统一以 closest('form')
// 判定，两种触发路径等价。
function mbxComposeForm(evtElt) {
    if (!evtElt || typeof evtElt.closest !== 'function') return null;
    return evtElt.closest('form');
}

// wangEditor 输出的 <table> 无 border 属性（slate 模型不存边框）。
// 外发到企业客户端（Outlook/Foxmail）按属性画线，内联 style 可能被剥或
// 忽略——统一补 border="1"（HTML 属性形态兼容性最广）；已有 border 属性
// 则不动（转发场景源邮件的自带宽度/间距样式保持原样）。
// 净化器已放行 table 的 border 属性（ec3441b），此处补的属性可安全过净化
function mbxEnsureTableBorders(html) {
    return html.replace(/<table(?![^>]*\bborder\s*=)/gi, '<table border="1"');
}

document.body.addEventListener('htmx:configRequest', function (ev) {
    var detail = ev.detail || {};
    var form = mbxComposeForm(detail.elt);
    var hidden = document.getElementById('body-html');
    if (mbxEditor && form && hidden && form.contains(hidden) && detail.parameters) {
        var converted = mbxMigrateInlineImages();
        if (converted.length > 0) {
            detail.parameters['inline_images'] = JSON.stringify(converted);
            var note = document.querySelector('.compose-note-inline') || document.createElement('div');
            note.className = 'compose-note ok compose-note-inline';
            note.innerHTML = MBX_IC.check + ' ' + converted.length + ' 张超过 2MB 的内嵌图片已转为附件，将随邮件发送';
            var card = document.querySelector('.compose-card');
            if (card) card.insertBefore(note, card.firstChild);
        }
        // 双保险：hidden.value 同步 + 参数显式覆盖（后者优先生效）。
        // 外发表格补 border=1（M1-5）：编辑器自建表格在收件方客户端才有分隔线
        hidden.value = mbxEnsureTableBorders(mbxEditor.getHtml());
        detail.parameters['body_html'] = hidden.value;
    }
});

// ---------- 撰写页：草稿自动保存（M3-1） ----------
// 停止输入 5s（防抖）→ 静默 POST /compose/autosave（与存草稿同一条保存路径，
// 返回轻量 JSON 无跳转）。首个 draft_id 回填隐藏域，此后自动保存变"覆盖"
// 不会堆积草稿；空表单后端守一道不落库。
// 请求经 htmx 发出：source=表单 使 htmx:configRequest 的 detail.elt 指向 form，
// 复用既有 wangEditor body_html/inline_images 注入（与存草稿完全同参形态）；
// handler 自定义接管响应（htmx 2.x ajax context 支持），无任何 swap 副作用。
(function () {
    var DELAY_MS = 5000;
    var timer = null;

    function composeForm() {
        var el = document.getElementById('autosave-hint');
        return el ? el.closest('form') : null;
    }

    function clock() {
        var t = new Date();
        function z(n) { return (n < 10 ? '0' : '') + n; }
        return z(t.getHours()) + ':' + z(t.getMinutes());
    }

    function trigger() {
        var form = composeForm();
        if (!form || !form.isConnected) return; // 已导航离开撰写页
        var values = {};
        try {
            new FormData(form).forEach(function (v, k) { values[k] = v; });
        } catch (e) { return; }
        htmx.ajax('POST', '/compose/autosave', {
            source: form,
            values: values,
            handler: function (_elt, info) {
                var xhr = info && info.xhr;
                if (!xhr || xhr.status !== 200) return;
                var res = null;
                try { res = JSON.parse(xhr.responseText); } catch (e) { return; }
                if (!res || !res.ok || res.draft_id <= 0) return;
                // 只有"本次新创建"才回填（编辑已有草稿时表单本来就有值；
                // 新建模板渲染 draft_id=0，与未创建等价）
                var idEl = form.querySelector('input[name="draft_id"]');
                if (idEl && (!idEl.value || idEl.value === '0')) {
                    idEl.value = String(res.draft_id);
                }
                var hint = document.getElementById('autosave-hint');
                if (hint) hint.innerHTML = MBX_IC.check + ' 已自动保存 ' + escapeHtml(clock());
            }
        });
        // 下轮防抖：保存后再有输入才会触发，不做周期性轮询
    }

    function schedule() {
        if (timer) clearTimeout(timer);
        timer = setTimeout(function () {
            timer = null;
            trigger();
        }, DELAY_MS);
    }

    document.body.addEventListener('input', function (ev) {
        var form = composeForm();
        if (!form || !form.contains(ev.target)) return;
        schedule();
    });
    // select 等不冒泡 input 的控件兜底
    document.body.addEventListener('change', function (ev) {
        var form = composeForm();
        if (!form || !form.contains(ev.target)) return;
        schedule();
    });
})();

// ---------- 撰写页：定时发送（M4-2） ----------
// 状态承载：#sched-at 隐藏域存 datetime-local 控件原始值（"YYYY-MM-DDTHH:MM"），
// 服务端 save_compose_form 统一解析成 epoch。两条发送路径同端点同表单：
// - 立即发送（button[hx-post="/compose/send"]:not(#sched-send-btn)）→ 点击即清空
//   hidden（capture 阶段抢在 htmx 收集参数之前）→ 提交 scheduled_at="" = 立即发
// - 定时发送（#sched-send-btn）→ htmx:configRequest 校验控件非空（空则拦请求
//   并提示），hidden 已由 change/input 同步为控件值 → 提交 = 定时
// 存草稿：hidden 原样提交（定时草稿保留计划；清空控件后存草稿 = 取消定时）
(function () {
    function hidden() { return document.getElementById('sched-at'); }
    function picker() { return document.getElementById('sched-input'); }
    function sync() {
        var h = hidden(), p = picker();
        if (h && p) h.value = p.value || '';
    }
    // 控件变化即同步隐藏域（change 覆盖清空/重选；input 覆盖手输部分）
    document.body.addEventListener('input', function (ev) {
        if (ev.target && ev.target.id === 'sched-input') sync();
    });
    document.body.addEventListener('change', function (ev) {
        if (ev.target && ev.target.id === 'sched-input') sync();
    });
    // 立即发送：capture 阶段清空（htmx 用冒泡监听，capture 先行——否则参数已收集）
    document.body.addEventListener('click', function (ev) {
        var t = ev.target;
        if (!t || !t.closest) return;
        var btn = t.closest('button[hx-post="/compose/send"]');
        if (btn && btn.id !== 'sched-send-btn') {
            var h = hidden();
            if (h) h.value = '';
        }
    }, true);
    // 定时发送：请求前校验控件已选（空则拦下并提示，模式同 M1-2 预校验）
    document.body.addEventListener('htmx:configRequest', function (ev) {
        var detail = ev.detail || {};
        var elt = detail.elt;
        if (!elt || !elt.matches || !elt.matches('#sched-send-btn')) return;
        var p = picker();
        if (!p || !p.value) {
            ev.preventDefault();
            var h = hidden();
            if (h) h.value = '';
            var note = document.createElement('div');
            note.className = 'compose-note err';
            note.textContent = '请先选择定时发送时刻（或点击「发送」立即发送）';
            var card = document.querySelector('.compose-card');
            if (card) card.insertBefore(note, card.firstChild);
            setTimeout(function () { if (note.parentNode) note.remove(); }, 4000);
        }
    });
})();

// ---------- 撰写页：发送前预校验（M1-2） ----------
// 挂 htmx:configRequest、精确匹配发送按钮（存草稿允许不完整内容，不校验）。
// preventDefault 即取消本次请求（htmx 在全部监听器执行完后统一检查），
// 此时 beforeRequest 未触达，hx-disabled-elt 不会留死锁禁用态。
// 校验顺序：无收件人（硬拦）→ 空正文（无附件硬拦 / 有附件确认）→ 无主题（确认）。
document.body.addEventListener('htmx:configRequest', function (ev) {
    var detail = ev.detail || {};
    var elt = detail.elt;
    if (!elt || !elt.matches || !elt.matches('button[hx-post="/compose/send"]')) return;
    var form = elt.closest('form');
    if (!form) return;
    // 每次校验先清旧错误条：校验失败插入新条，修正后重发则干净出发
    // （不放行态残留误导"仍未通过"），服务端错误条不受影响（无 .precheck 类）
    var card = form.closest('.compose-card');
    var oldNote = card && card.querySelector('.compose-note.precheck');
    if (oldNote) oldNote.remove();
    var field = function (name) {
        var el = form.querySelector('[name="' + name + '"]');
        return el ? String(el.value || '').trim() : '';
    };
    // 拦截：表单顶部错误条（.err 复用服务端错误样式；.precheck 供下次先清旧条）
    var fail = function (msg) {
        if (card) {
            var note = document.createElement('div');
            note.className = 'compose-note err precheck';
            note.textContent = msg;
            card.insertBefore(note, card.firstChild);
        }
        ev.preventDefault();
    };
    // 1) 收件人：to + cc + bcc 均空——SMTP 必拒，前端即时拦下，避免
    //    "存进草稿→入队→异步发失败"一轮往返才见错误（纯密送是合法形态）
    if (!field('to_addrs') && !field('cc_addrs') && !field('bcc_addrs')) {
        fail('请填写收件人、抄送或密送后再发送');
        return;
    }
    // 2) 正文：按当前模式取值。HTML 模式以 DOM 探针判定——wangEditor
    //    空内容 getHtml() 返回 <p><br></p>，textContent 为空即可判空；
    //    只有图片（无文字）视为有正文（贴图发送是完整语义）
    var modeEl = form.querySelector('#body-mode');
    var htmlMode = modeEl && modeEl.value === 'html' && mbxEditor;
    var emptyBody;
    if (htmlMode) {
        var probe = document.createElement('div');
        probe.innerHTML = mbxEditor.getHtml();
        emptyBody = (probe.textContent || '').trim() === '' && !probe.querySelector('img');
    } else {
        var ta = form.querySelector('#body-text');
        emptyBody = !ta || !(ta.value || '').trim();
    }
    if (emptyBody) {
        // 有附件/已保存附件时空正文是合法诉求（"只发附件"），降级为确认
        var kept = form.querySelector('#keep-ids');
        var hasAttach = pickedPaths.length > 0;
        if (!hasAttach && kept) {
            try { hasAttach = JSON.parse(kept.value || '[]').length > 0; } catch (e) { /* 空串按无附件 */ }
        }
        if (!hasAttach) {
            fail('正文不能为空；如只想发送附件，请在正文补充一句说明');
            return;
        }
        // 页面内模态确认（原 window.confirm 在 macOS WKWebView 不弹窗且恒返
        // false，空正文带附件的邮件根本发不出去）。确认后带标志重触发：
        // 重入校验时凭标志放行，避免再次弹窗死循环
        if (elt.dataset.mbxBodyOk !== '1') {
            ev.preventDefault();
            mbxConfirmLeave('正文为空，仅发送附件，确定吗？', function () {
                elt.dataset.mbxBodyOk = '1';
                // 550ms 后重触发：避开按钮 hx-trigger 的 500ms 防抖窗口
                setTimeout(function () { elt.click(); }, 550);
            }, '仍要发送', '返回修改');
            return;
        }
        delete elt.dataset.mbxBodyOk;
    }
    // 3) 主题为空：确认放行（企业邮件无主题易被过滤/忽视，确认防手滑）。
    //    同样走页面内模态（window.confirm 在 macOS 不可用），独立标志防与
    //    正文确认叠加时互相吞
    if (!field('subject')) {
        if (elt.dataset.mbxSubjOk !== '1') {
            ev.preventDefault();
            mbxConfirmLeave('主题为空，确定直接发送吗？', function () {
                elt.dataset.mbxSubjOk = '1';
                setTimeout(function () { elt.click(); }, 550);
            }, '仍要发送', '返回修改');
            return;
        }
        delete elt.dataset.mbxSubjOk;
    }
});

function mbxRtInit() {
    if (!document.getElementById('editor-container')) return;
    // 富文本模式（回复/转发 HTML 邮件、HTML 草稿）：直接挂载，初始内容
    // 来自服务端净化的 blockquote/正文；纯文本模式保持 textarea（升级按钮触发）
    const box = document.getElementById('body-html-box');
    if (box && !box.hidden) {
        mbxBuildEditor(mbxReadInitialHtml());
    }
}
document.addEventListener('DOMContentLoaded', mbxRtInit);

// ---------- 设置页保存按钮：配置无变化时禁用 ----------
// 进入/保存后按表单当前值建立基线；任一表单控件变化即启用保存，无修改则保持禁用。
// 排除 disabled（服务端只读字段）与 submit/button/file；password 空=保持不变，
// 清回空串视为无变化。sync 的开关改 hidden 值不走 input/change，需补 click 监听。
// 每次 swap 后经 htmx:afterSettle 重建基线（保存结果/切 Tab 均被覆盖）。
(function () {
    var base = typeof WeakMap !== 'undefined' ? new WeakMap() : null;
    function snapshot(form) {
        var out = {}, els = form.querySelectorAll('input, select, textarea');
        for (var i = 0; i < els.length; i++) {
            var el = els[i];
            if (el.disabled || !el.name) continue;
            var t = (el.type || '').toLowerCase();
            if (t === 'submit' || t === 'button' || t === 'file') continue;
            out[el.name] = t === 'radio' || t === 'checkbox'
                ? (el.checked ? el.value : '')
                : el.value;
        }
        return JSON.stringify(out);
    }
    function paneForm(node) {
        var f = node && node.closest ? node.closest('#settings-pane form') : null;
        return f;
    }
    function refresh(form) {
        if (!base || !form) return;
        var b = form.querySelector('button[type="submit"]');
        if (b) b.disabled = base.get(form) === snapshot(form);
    }
    function rebaseline(form) {
        if (!base || !form || !form.querySelector('button[type="submit"]')) return;
        base.set(form, snapshot(form));
        refresh(form);
    }
    document.addEventListener('input', function (ev) { refresh(paneForm(ev.target)); });
    document.addEventListener('change', function (ev) { refresh(paneForm(ev.target)); });
    document.addEventListener('click', function (ev) {
        var t = ev.target;
        var f = paneForm(t);
        if (f && t.classList && t.classList.contains('toggle')) refresh(f);
    });
    document.body.addEventListener('htmx:afterSettle', function (ev) {
        var pane = ev.detail && ev.detail.elt;
        var f = pane
            ? (pane.matches && pane.matches('#settings-pane form') ? pane
               : (pane.querySelector ? pane.querySelector('#settings-pane form') : null))
            : null;
        rebaseline(f);
    });
    document.addEventListener('DOMContentLoaded', function () {
        rebaseline(document.querySelector('#settings-pane form'));
    });
})();

// ---------- 列表批量操作（M1-1）：页内精确 + 跨页全选 ----------
// 状态机：
// - ids 模式：勾选的行 id 集合（页内）；
// - all 模式：表头全选触发跨页全选，exclude 为反勾选集合
//   （「选择全部 N 封」> 全部 N 封 · 排除 k 封）
// 提交两种形态：ids=[..] 或 select_all=1&exclude_ids=[..]（附列表筛选上下文）
// 操作后服务端回 mail_list 片段（#list-area 整段换入）+ mbx:fetchDone（边栏）
(function () {
    var stateBox = { mode: 'ids', ids: [], exclude: [], total: 0 };

    function val(id) {
        var el = document.getElementById(id);
        return el ? el.value : '';
    }
    function num(id) {
        return parseInt(val(id), 10) || 0;
    }
    // 当前筛选的检索态：检索场景禁用跨页全选（前后端口径一致）
    function isSearch() { return (val('ba-q') || '').trim() !== ''; }

    function bar() { return document.getElementById('batch-bar'); }
    function items() {
        return Array.prototype.slice.call(document.querySelectorAll('.mbx-item'));
    }

    function selectedCount() {
        if (stateBox.mode === 'all') {
            return Math.max(stateBox.total - stateBox.exclude.length, 0);
        }
        return stateBox.ids.length;
    }

    function render() {
        if (!bar()) return;
        var n = selectedCount();
        var countEl = document.getElementById('ba-count');
        var allLink = document.getElementById('ba-select-all');
        // 工具栏常驻：按选中数切换按钮禁用态（位置固定不跳动）
        var btns = document.querySelectorAll('#batch-bar [data-batch], #batch-bar #ba-clear');
        Array.prototype.forEach.call(btns, function (b) { b.disabled = n === 0; });
        if (!countEl) return;
        if (stateBox.mode === 'all') {
            var ex = stateBox.exclude.length;
            countEl.textContent = ex > 0
                ? '已选全部 ' + stateBox.total + ' 封 · 排除 ' + ex + ' 封'
                : '已选全部 ' + stateBox.total + ' 封';
            if (allLink) allLink.hidden = true;
        } else {
            countEl.textContent = n > 0 ? '已选 ' + n + ' 封' : '';
            // 跨页入口：非检索态即显示「选择全部 N 封」（总命中 = 筛选全集上限；
            // 单页亦显示——点即全选当前筛选全部，语义确定，用户不需记"有无下一页"）
            if (allLink) {
                allLink.hidden = isSearch() || n === 0;
                allLink.textContent = '选择全部 ' + stateBox.total + ' 封';
            }
        }
        // 表头三态
        var all = document.getElementById('batch-all');
        if (all) {
            var checked = items().filter(function (c) { return c.checked; }).length;
            all.checked = checked > 0 && checked === items().length;
            all.indeterminate = checked > 0 && checked < items().length;
        }
    }

    function resetSelection() {
        stateBox = { mode: 'ids', ids: [], exclude: [], total: num('ba-total') };
        items().forEach(function (c) { c.checked = false; });
        render();
    }

    function collectIds() {
        return items().filter(function (c) { return c.checked; })
            .map(function (c) { return parseInt(c.value, 10); });
    }

    function apply(action, confirmMsg) {
        var params = new URLSearchParams();
        params.set('action', action);
        params.set('folder', val('ba-folder') || 'inbox');
        params.set('unread', val('ba-unread') || '0');
        params.set('external', val('ba-external') || '0');
        var q = (val('ba-q') || '').trim();
        if (q) params.set('q', q);
        params.set('page', val('ba-page') || '1');
        // 每页条数（M4/紧凑化）：批量后回列表片段保持用户的分页规格
        var per = val('ba-per');
        if (per && per !== '10') params.set('per', per);
        // 排序（A2）：非默认才带，批量后列表片段保持当前排序
        var sort = val('ba-sort');
        if (sort && sort !== 'date_desc') params.set('sort', sort);
        // 高级检索（B1）：批量后列表片段保持筛选条件
        var from = (val('ba-from') || '').trim();
        if (from) params.set('from', from);
        var df = val('ba-df');
        if (df) params.set('df', df);
        var dt = val('ba-dt');
        if (dt) params.set('dt', dt);
        if (val('ba-att') === '1') params.set('att', '1');
        if (stateBox.mode === 'all') {
            params.set('select_all', '1');
            params.set('exclude_ids', JSON.stringify(stateBox.exclude));
        } else {
            params.set('ids', JSON.stringify(stateBox.ids));
        }
        // 批量删除确认：页面内模态（原 window.confirm 在 macOS WKWebView
        // 不弹窗恒返 false，批量删除会静默失效）
        var submit = function () {
            htmx.ajax('POST', '/emails/batch', {
                target: '#list-area', swap: 'outerHTML',
                values: Object.fromEntries(params.entries())
            });
        };
        if (confirmMsg) {
            mbxConfirmLeave(confirmMsg, submit, '确定删除', '取消');
            return;
        }
        submit();
    }

    function confirmText(action) {
        var n = selectedCount();
        if (action === 'delete') {
            return '确定删除选中的 ' + n + ' 封邮件？删除后无法恢复（含附件），草稿也会一并删除。';
        }
        return null;
    }

    document.body.addEventListener('change', function (ev) {
        var t = ev.target;
        if (!t.classList) return;
        if (t.id === 'batch-all') {
            if (t.checked) {
                // 表头全选：若跨页可用则提示入口，仍先做页内全选
                items().forEach(function (c) { c.checked = true; });
                stateBox.mode = 'ids';
                stateBox.ids = collectIds();
            } else {
                // 取消全选：若在跨页模式则整体退出，否则清页内
                resetSelection();
            }
            render();
            return;
        }
        if (t.classList.contains('mbx-item')) {
            var id = parseInt(t.value, 10);
            if (stateBox.mode === 'all') {
                // 跨页模式下行勾选 = 排除/撤销排除
                var idx = stateBox.exclude.indexOf(id);
                if (t.checked && idx >= 0) stateBox.exclude.splice(idx, 1);
                if (!t.checked && idx < 0) stateBox.exclude.push(id);
            } else {
                stateBox.ids = collectIds();
            }
            render();
        }
    });

    document.body.addEventListener('click', function (ev) {
        var t = ev.target;
        if (!t.closest) return;
        // 复选框位于行链接之外的独立 label 内（2026-11-03 结构修正）：
        // 原生勾选/原生 change 事件天然工作，无需任何 click 拦截
        var allLink = t.closest('#ba-select-all');
        if (allLink) {
            ev.preventDefault();
            stateBox.mode = 'all';
            stateBox.exclude = [];
            items().forEach(function (c) { c.checked = true; });
            render();
            return;
        }
        var clearBtn = t.closest('#ba-clear');
        if (clearBtn) {
            resetSelection();
            return;
        }
        var btn = t.closest('[data-batch]');
        if (btn) {
            var action = btn.getAttribute('data-batch');
            var sel = selectedCount();
            if (sel === 0) return;
            apply(action, confirmText(action));
        }
    });

    // 片段换入（首次加载/批量操作返回/收取刷新）重建基线
    function init() {
        if (!bar()) return;
        stateBox = { mode: 'ids', ids: [], exclude: [], total: num('ba-total') };
        render();
    }
    document.body.addEventListener('htmx:afterSwap', function (ev) {
        if (ev.detail && ev.detail.target && ev.detail.target.id === 'list-area') init();
    });
    document.addEventListener('DOMContentLoaded', init);
})();

// ---------- 邮件模板插入（B2） ----------
// 纯文本模式：在 textarea 光标处插入 body_text，光标落到末尾；
// 富文本模式：优先 dangerouslyInsertHtml(body_html)，空则把文本转义为段落；
// 插入后下拉复位，避免重复选择同一项不触发 change
(function () {
    var sel = document.getElementById('tpl-select');
    if (!sel) return;
    sel.addEventListener('change', function () {
        var opt = sel.options[sel.selectedIndex];
        if (!opt || !opt.value) return;
        var text = opt.dataset.text || '';
        var html = opt.dataset.html || '';
        var mode = document.getElementById('body-mode');
        var isHtml = mode && mode.value === 'html';
        if (isHtml && window.mbxEditor) {
            if (!html.trim()) {
                html = '<p>' + escapeHtml(text).replace(/\r?\n/g, '<br>') + '</p>';
            }
            // 先聚焦再插：wangEditor 未聚焦时 dangerouslyInsertHtml 会落到
            // 文档末尾（且内部选区不刷新）；focus() 恢复上次光标位置
            try { window.mbxEditor.focus(); } catch (e) { /* 忽略 */ }
            if (typeof window.mbxEditor.dangerouslyInsertHtml === 'function') {
                window.mbxEditor.dangerouslyInsertHtml(html);
            } else {
                window.mbxEditor.setHtml((window.mbxEditor.getHtml() || '') + html);
            }
        } else {
            var ta = document.getElementById('body-text');
            if (ta) {
                var start = ta.selectionStart || 0;
                var end = ta.selectionEnd || 0;
                ta.value = ta.value.slice(0, start) + text + ta.value.slice(end);
                ta.selectionStart = ta.selectionEnd = start + text.length;
                ta.focus();
            }
        }
        sel.selectedIndex = 0;
    });
})();

// ---------- 高级检索面板（B1） ----------
// 搜索框右侧 ⚙ 切换面板；清除筛选清空四项后提交（保留 folder/排序/每页）。
// 用事件委托：htmx 局部换入 topbar/adv-panel 后新元素仍生效（2026-11-05 修复点击失效）
document.addEventListener('click', function (ev) {
    var t = ev.target;
    if (!t || !t.closest) return;
    var panel = document.getElementById('adv-panel');
    var toggle = t.closest('#adv-toggle');
    if (toggle && panel) {
        panel.hidden = !panel.hidden;
        if (!panel.hidden) {
            var first = panel.querySelector('input[name="from"]');
            if (first) first.focus();
        }
        return;
    }
    var clear = t.closest('#adv-clear');
    if (clear && panel) {
        ['from', 'df', 'dt'].forEach(function (n) {
            var el = panel.querySelector('input[name="' + n + '"]');
            if (el) el.value = '';
        });
        var att = panel.querySelector('input[name="att"]');
        if (att) att.checked = false;
        panel.submit();
    }
});

// ---------- 键盘快捷键（A3） ----------
// 列表：j / ↓ 下移、k / ↑ 上移、Enter / o 打开选中、/ 聚焦搜索、n 新建邮件
// 详情：r 回复、f 转发、Esc 返回列表
// 输入框 / textarea / select / contenteditable 内的按键一律放行（Esc 仅失焦），
// 不打断打字；附件预览打开时 Esc 归预览模块，避免同时退出两层
(function () {
    var inField = function (el) {
        if (!el) return false;
        var tag = (el.tagName || '').toLowerCase();
        return tag === 'input' || tag === 'textarea' || tag === 'select' || el.isContentEditable;
    };
    var focusIdx = -1;
    var rows = function () {
        return Array.prototype.slice.call(document.querySelectorAll('#mail-list .mail-row'));
    };
    var paint = function () {
        var list = rows();
        if (focusIdx >= list.length) focusIdx = list.length - 1;
        list.forEach(function (r, i) { r.classList.toggle('kb-focus', i === focusIdx); });
        if (focusIdx >= 0 && list[focusIdx]) {
            list[focusIdx].scrollIntoView({ block: 'nearest' });
        }
    };
    document.addEventListener('keydown', function (ev) {
        if (ev.metaKey || ev.ctrlKey || ev.altKey) return;
        if (inField(ev.target)) {
            if (ev.key === 'Escape') ev.target.blur();
            return;
        }
        var modal = document.getElementById('preview-modal');
        var previewOpen = modal && !modal.hidden;
        if (ev.key === 'Escape') {
            if (previewOpen) return; // 预览层自己处理
            var back = document.querySelector('.topbar .back');
            if (back) { ev.preventDefault(); back.click(); }
            return;
        }
        var list = document.getElementById('mail-list');
        if (list) {
            var items = rows();
            if (!items.length) return;
            if (ev.key === 'j' || ev.key === 'ArrowDown') {
                ev.preventDefault();
                focusIdx = Math.min(items.length - 1, focusIdx + 1);
                paint();
            } else if (ev.key === 'k' || ev.key === 'ArrowUp') {
                ev.preventDefault();
                focusIdx = focusIdx < 0 ? 0 : Math.max(0, focusIdx - 1);
                paint();
            } else if (ev.key === 'Enter' || ev.key === 'o') {
                if (focusIdx < 0) { focusIdx = 0; paint(); return; }
                var link = items[focusIdx] && items[focusIdx].querySelector('.mr-link');
                if (link) { ev.preventDefault(); window.location.href = link.href; }
            } else if (ev.key === '/') {
                var box = document.querySelector('form.search input[name="q"]');
                if (box) { ev.preventDefault(); box.focus(); }
            } else if (ev.key === 'n') {
                ev.preventDefault();
                window.location.href = '/compose';
            }
            return;
        }
        var detail = document.querySelector('.mail-detail');
        if (detail) {
            if (ev.key === 'r') {
                var reply = detail.querySelector('a[href*="reply_to="]');
                if (reply) { ev.preventDefault(); window.location.href = reply.href; }
            } else if (ev.key === 'f') {
                var fwd = detail.querySelector('a[href*="forward_from="]');
                if (fwd) { ev.preventDefault(); window.location.href = fwd.href; }
            }
        }
    });
})();

// ---------- 撰写页：离开前未保存确认 ----------
// 编辑过但未保存/发送时，点链接离开弹页面内确认模态（Tauri 的 macOS WKWebView
// 不支持 window.confirm，故用 mbxConfirmLeave）。edited 标记由输入事件置位，
// 自动保存/存草稿成功后清零（已落库不再提示）。
(function () {
    function composeForm() {
        var el = document.getElementById('autosave-hint');
        return el ? el.closest('form') : null;
    }
    if (!composeForm()) return; // 非撰写页，不启用

    var edited = false;
    var leaving = false;
    var bypass = false;
    var ready = false;
    // 编辑器挂载后启用编辑标记（DOMContentLoaded 同步构建，稍延迟确保就绪）
    function arm() { ready = true; }
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function () { setTimeout(arm, 0); });
    } else {
        setTimeout(arm, 0);
    }
    function isDirty() { return edited; }

    function markEdited(ev) {
        var f = composeForm();
        if (!f || !f.contains(ev.target)) return;
        if (!ready) return;
        edited = true;
        var hint = document.getElementById('autosave-hint');
        if (hint) hint.innerHTML = MBX_IC.dot + ' 未保存';
    }
    document.body.addEventListener('input', markEdited, true);
    document.body.addEventListener('change', markEdited, true);

    // 保存/自动保存成功 → 已落库，清除未保存标记
    document.body.addEventListener('htmx:afterRequest', function (ev) {
        var d = ev.detail || {};
        var reqPath = String((d.pathInfo && d.pathInfo.requestPath) || d.path || '');
        var xhr = d.xhr;
        // 仅显式「存草稿」(/compose/draft) 成功才清除未保存标记；
        // 静默自动保存(/compose/autosave)不计入——有编辑时离开始终提醒，
        // 直到用户显式存草稿或发送（否则自动保存会提前把提醒"吃掉"）
        if (xhr && xhr.status >= 200 && xhr.status < 300 &&
            reqPath.indexOf('/compose/draft') >= 0) {
            edited = false;
        }
    });

    // 发送/存草稿按钮 → 已保存意图，放行后续导航
    document.addEventListener('click', function (ev) {
        var b = ev.target && ev.target.closest ? ev.target.closest('button[hx-post="/compose/send"], button[type="submit"]') : null;
        if (b) leaving = true;
    }, true);

    // 链接离开 → 自定义模态确认（确认后跳转，避免二次拦截）
    document.addEventListener('click', function (ev) {
        if (bypass) return;
        var a = ev.target && ev.target.closest ? ev.target.closest('a[href]') : null;
        if (!a) return;
        var href = a.getAttribute('href') || '';
        if (!href || href.charAt(0) === '#' || href.indexOf('javascript:') === 0 || a.target === '_blank') return;
        if (!isDirty()) return;
        ev.preventDefault();
        ev.stopPropagation();
        var dest = a.href;
        mbxConfirmLeave('邮件尚未保存或发送，离开将丢失未保存的内容。\n可先点「存草稿」保存。', function () {
            leaving = true;
            bypass = true;
            window.location.href = dest;
        });
    }, true);

    window.addEventListener('beforeunload', function (ev) {
        if (leaving) return;
        if (isDirty()) {
            ev.preventDefault();
            ev.returnValue = '';
            return '';
        }
    });
})();

// ---------- 设置页：离开前未保存确认 ----------
// 覆盖：切换 tab（hx-get 替换 #settings-pane）、点侧栏/其他链接离开、刷新/关闭。
// Tauri WKWebView 不支持 window.confirm，统一用页面内模态 mbxConfirmLeave；
// edited 标记由 pane 内输入事件置位，保存/切换后（afterSwap）清零。
(function () {
    function paneRoot() { return document.getElementById('settings-pane'); }
    if (!paneRoot()) return; // 非设置页

    var edited = false;
    var leaving = false;
    var bypass = false;

    function paneEdited(ev) {
        var p = paneRoot();
        if (!p || !p.contains(ev.target)) return;
        edited = true;
    }
    document.body.addEventListener('input', paneEdited, true);
    document.body.addEventListener('change', paneEdited, true);
    function isDirty() { return edited; }
    function clearEdited() { edited = false; }

    // 切换 tab（span.stab：hx-get 替换 pane）——有未保存修改先确认
    document.addEventListener('click', function (ev) {
        if (bypass) return;
        var tab = ev.target && ev.target.closest ? ev.target.closest('.settings-tabs .stab') : null;
        if (!tab) return;
        if (!isDirty()) return;
        ev.preventDefault();
        ev.stopPropagation();
        if (ev.stopImmediatePropagation) ev.stopImmediatePropagation();
        mbxConfirmLeave('设置尚未保存，切换后将丢失未保存的修改。', function () {
            leaving = true;
            bypass = true;
            tab.click();
            setTimeout(function () { bypass = false; }, 0);
        });
    }, true);

    // 离开设置页的链接导航（侧栏 / 返回等）
    document.addEventListener('click', function (ev) {
        if (bypass) return;
        var a = ev.target && ev.target.closest ? ev.target.closest('a[href]') : null;
        if (!a) return;
        var href = a.getAttribute('href') || '';
        if (!href || href.charAt(0) === '#' || href.indexOf('javascript:') === 0 || a.target === '_blank') return;
        if (!isDirty()) return;
        ev.preventDefault();
        ev.stopPropagation();
        var dest = a.href;
        mbxConfirmLeave('设置尚未保存，离开将丢失未保存的修改。', function () {
            leaving = true;
            bypass = true;
            window.location.href = dest;
        });
    }, true);

    // 保存按钮点击 → 已保存意图，放行
    document.addEventListener('click', function (ev) {
        var b = ev.target && ev.target.closest ? ev.target.closest('button[hx-post*="/save"], button[type="submit"]') : null;
        if (b) leaving = true;
    }, true);

    // 保存成功 / 切换 tab 后 pane 被替换 → 清除未保存标记
    document.body.addEventListener('htmx:afterSwap', function (ev) {
        var t = ev.detail && ev.detail.target;
        if (t && t.id === 'settings-pane') { clearEdited(); leaving = false; bypass = false; }
    });

    window.addEventListener('beforeunload', function (ev) {
        if (leaving) return;
        if (isDirty()) {
            ev.preventDefault();
            ev.returnValue = '';
            return '';
        }
    });
})();

// ---------- 源码页（A3）：行号 + 一键复制 ----------
// 源码页为整页导航（非 htmx 局部替换），DOMContentLoaded 一次构建即可。
function mbBuildSourceGutter(wrap) {
    var pre = wrap.querySelector('.src-pre');
    var gutter = wrap.querySelector('.src-gutter');
    if (!pre || !gutter) return;
    var text = pre.textContent || '';
    var n = text.split('\n').length;
    if (text.charCodeAt(text.length - 1) === 10) n -= 1; // 尾随换行不占空行
    var nums = [];
    for (var i = 1; i <= n; i++) nums.push(String(i));
    gutter.textContent = nums.join('\n');
}

function mbCopyTextFallback(text) {
    var ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); } catch (e) { /* 忽略 */ }
    document.body.removeChild(ta);
}

function mbSourceCopy(pre) {
    var btn = document.querySelector('.src-copy');
    if (!pre) return;
    var text = pre.textContent || '';
    var original = btn ? (btn.dataset.label || '复制') : '复制';
    function settle(label) {
        if (!btn) return;
        btn.dataset.label = original;
        btn.innerHTML = MBX_IC.check + ' ' + escapeHtml(label);
        setTimeout(function () { btn.textContent = original; }, 1500);
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(
            function () { settle('已复制'); },
            function () { mbCopyTextFallback(text); settle('已复制'); }
        );
    } else {
        mbCopyTextFallback(text);
        settle('已复制');
    }
}

document.addEventListener('DOMContentLoaded', function () {
    document.querySelectorAll('.src-wrap').forEach(mbBuildSourceGutter);
});

document.addEventListener('click', function (e) {
    var btn = e.target && e.target.closest ? e.target.closest('.src-copy') : null;
    if (!btn) return;
    var pre = document.getElementById('src-content');
    if (pre) mbSourceCopy(pre);
});

// ---------- 设置·模板富文本编辑器（批次五） ----------
// 与撰写页正文逻辑完全一致：单一「内容」字段，纯文本可单向切换为富文本
// （同款 wangEditor），切换后不可降级回纯文本；提交时 body_mode 由切换状态
// 决定，后端 html 模式从 body_html 派生纯文本版（预览/纯文本插入回退）。
// 编辑表单藏在 <details> 里，编辑器惰性挂载（html 模式首次可见时才建），
// 避免一页挂 N 个。
var MBX_TPL_SEQ = 0;

// HTML → 纯文本：块级元素与 <br> 转换行，去标签留文本
function mbxHtmlToText(html) {
    var d = document.createElement('div');
    d.innerHTML = html || '';
    d.querySelectorAll('br').forEach(function (el) { el.replaceWith('\n'); });
    d.querySelectorAll('p,div,li,tr,h1,h2,h3,h4,blockquote').forEach(function (el) {
        el.insertAdjacentText('afterend', '\n');
    });
    return (d.textContent || '')
        .replace(/[ \t]+\n/g, '\n')
        .replace(/\n{3,}/g, '\n\n')
        .trim();
}

// wangEditor 的 selector 只接受字符串：给每个字段生成唯一 id
function mbxTplBuild(st) {
    if (st.ed) return;
    if (!window.wangEditor) {
        // 无编辑器（脚本未加载/异常）：降级为源码 textarea 输入
        st.mode = 'src';
        st.bar.hidden = true;
        st.area.hidden = true;
        st.ta.hidden = false;
        if (st.srcBtn) st.srcBtn.disabled = true;
        return;
    }
    st.area.id = st.area.id || 'tpl-ed-area-' + (++MBX_TPL_SEQ);
    st.bar.id = st.bar.id || 'tpl-ed-tb-' + MBX_TPL_SEQ;
    try {
        st.ed = wangEditor.createEditor({
            selector: '#' + st.area.id,
            html: st.ta.value,
            config: {
                placeholder: '输入模板内容…（表格 / 列表 / 颜色 / 链接）',
                MENU_CONF: {
                    uploadImage: {
                        customUpload: function (file, insertFn) {
                            var reader = new FileReader();
                            reader.onload = function () { insertFn(reader.result, file.name, ''); };
                            reader.readAsDataURL(file);
                        }
                    }
                }
            },
            mode: 'default'
        });
        wangEditor.createToolbar({
            selector: '#' + st.bar.id,
            editor: st.ed,
            config: { excludeKeys: ['group-video', 'fullScreen'] },
            mode: 'default'
        });
    } catch (e) {
        console.error('模板编辑器初始化失败', e);
        st.mode = 'src';
        st.bar.hidden = true;
        st.area.hidden = true;
        st.ta.hidden = false;
        if (st.srcBtn) st.srcBtn.disabled = true;
        return;
    }
    st.ed.on('change', function () {
        if (st.mode !== 'rich') return;
        var h = st.ed.getHtml();
        // wangEditor 空内容输出 <p><br></p>：归一为空串，否则这段"空段落"
        // 会被当有效 HTML 入库，产生空壳模板
        if (!mbxHtmlToText(h)) h = '';
        st.ta.value = h;
    });
    st.mode = 'rich';
    st.ta.hidden = true;
    st.bar.hidden = false;
    st.area.hidden = false;
}

function mbxTplToggleSrc(st) {
    if (st.mode === 'rich') {
        // 富文本 → 源码：编辑器内容写回 textarea 展示
        st.ta.value = st.ed ? st.ed.getHtml() : '';
        st.ta.hidden = false;
        st.bar.hidden = true;
        st.area.hidden = true;
        st.mode = 'src';
        st.srcBtn.innerHTML = MBX_IC.pencil + ' 富文本';
    } else {
        mbxTplBuild(st);
        if (!st.ed) return;
        st.ed.setHtml(st.ta.value);
        st.ta.hidden = true;
        st.bar.hidden = false;
        st.area.hidden = false;
        st.mode = 'rich';
        st.srcBtn.innerHTML = MBX_IC.keyboard + ' HTML 源码';
    }
}

// 纯文本 → 富文本：单向升级（与撰写页 mbxUpgradeHtml 一致），内容转为段落，
// 升级后隐藏纯文本区与升级按钮，body_mode 置 html；HTML 不可再降级回纯文本
function mbxTplUpgrade(st) {
    var box = st.plainBox.querySelector('textarea[name="body_text"]');
    var initial = '';
    if (box && box.value) {
        var esc = box.value
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;');
        initial = '<p>' + esc.replace(/\r?\n/g, '<br>') + '</p>';
    }
    st.ta.value = initial;
    mbxTplBuild(st);
    if (st.plainHead) st.plainHead.hidden = true;
    st.plainBox.hidden = true;
    st.htmlField.hidden = false;
    if (st.modeInput) st.modeInput.value = 'html';
}

// html 模式才需要编辑器（plain 模式升级时按需建）——供惰性挂载判定
function mbxTplMaybeBuild(st) {
    if (st.ed) return;
    if (st.modeInput && st.modeInput.value === 'html') mbxTplBuild(st);
}

function mbxTplInitField(field) {
    if (field.__tplInit) return;
    field.__tplInit = true;
    var st = {
        mode: 'src',
        ed: null,
        modeInput: field.querySelector('[data-tpl-mode]'),
        plainHead: field.querySelector('[data-tpl-plain-head]'),
        plainBox: field.querySelector('[data-tpl-plain]'),
        htmlField: field.querySelector('[data-tpl-html-field]'),
        bar: field.querySelector('[data-tpl-tb]'),
        area: field.querySelector('[data-tpl-area]'),
        ta: field.querySelector('[data-tpl-html]'),
        srcBtn: field.querySelector('[data-tpl-src-toggle]'),
        upgradeBtn: field.querySelector('[data-tpl-upgrade]')
    };
    if (!st.plainBox || !st.htmlField || !st.ta || !st.bar || !st.area) return;
    if (st.srcBtn) st.srcBtn.addEventListener('click', function () { mbxTplToggleSrc(st); });
    if (st.upgradeBtn) st.upgradeBtn.addEventListener('click', function () { mbxTplUpgrade(st); });
    // 编辑表单在 <details> 内：html 模式展开才建编辑器；新增表单（初始 plain）
    // 升级时才建——不在页面进入时就挂 N 个编辑器实例
    var det = field.closest('details');
    if (det) {
        det.addEventListener('toggle', function () { if (det.open) mbxTplMaybeBuild(st); });
        if (det.open) mbxTplMaybeBuild(st);
    } else {
        mbxTplMaybeBuild(st);
    }
}

function mbxTplInitAll(root) {
    (root || document).querySelectorAll('[data-tpl-field]').forEach(mbxTplInitField);
}

// 片段重渲染（保存/删除/tab 切换）与整页进入都初始化。
// 注意：hx-swap=outerHTML 时 afterSwap 的 target 可能仍指向已被替换的旧节点，
// 初始化旧 DOM 会让按钮看似“无效”。因此统一扫描文档中尚未初始化的字段。
document.addEventListener('DOMContentLoaded', function () { mbxTplInitAll(document); });
document.body.addEventListener('htmx:afterSwap', function () { mbxTplInitAll(document); });

// ---------- 气泡说明（2026-11-10）----------
// 设置页原先把限制说明写成 label 后的 12px 小字，既挤占布局又容易和字段值
// 混淆；改为字段旁的 info 图标 + 悬停/聚焦气泡。浮层挂 body（position:fixed），
// 不会被设置页/向导的滚动容器裁掉；超出视口时自动翻到触发点上方。
(function () {
    var layer = null;
    var current = null;

    function ensureLayer() {
        if (!layer || !layer.isConnected) {
            layer = document.createElement('div');
            layer.className = 'tip-layer';
            layer.setAttribute('role', 'tooltip');
            layer.hidden = true;
            document.body.appendChild(layer);
        }
        return layer;
    }

    function place(btn) {
        var el = ensureLayer();
        var r = btn.getBoundingClientRect();
        var gap = 6;
        var pad = 8;
        // 先归零再量尺寸：气泡宽度随文本变化，上一轮 left/top 会造成误判
        el.style.left = '0px';
        el.style.top = '0px';
        var w = el.offsetWidth;
        var h = el.offsetHeight;
        var left = Math.max(pad, Math.min(r.left + r.width / 2 - w / 2, window.innerWidth - w - pad));
        var top = r.bottom + gap;
        if (top + h > window.innerHeight - pad) top = r.top - h - gap;
        el.style.left = Math.round(left) + 'px';
        el.style.top = Math.round(Math.max(pad, top)) + 'px';
    }

    function show(btn) {
        var text = btn.getAttribute('data-tip');
        if (!text) return;
        current = btn;
        var el = ensureLayer();
        el.textContent = text;
        el.hidden = false;
        place(btn);
    }

    function hide() {
        current = null;
        if (layer) layer.hidden = true;
    }

    function tipOf(node) {
        return node && node.closest ? node.closest('[data-tip]') : null;
    }

    document.addEventListener('mouseover', function (ev) {
        var b = tipOf(ev.target);
        if (!b || b === current) return;
        if (ev.relatedTarget && b.contains(ev.relatedTarget)) return;
        show(b);
    });
    document.addEventListener('mouseout', function (ev) {
        var b = tipOf(ev.target);
        if (!b) return;
        if (ev.relatedTarget && b.contains(ev.relatedTarget)) return;
        hide();
    });
    document.addEventListener('focusin', function (ev) {
        var b = tipOf(ev.target);
        if (b) show(b);
    });
    document.addEventListener('focusout', function (ev) {
        var b = tipOf(ev.target);
        if (b && b === current) hide();
    });
    document.addEventListener('click', function (ev) {
        var b = tipOf(ev.target);
        if (b) show(b);
        else if (current) hide();
    });
    document.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape') { hide(); return; }
        // 键盘激活（role=button）：Enter/Space 与点击等价
        if ((ev.key === 'Enter' || ev.key === ' ') && ev.target && ev.target.matches
            && ev.target.matches('[data-tip]')) {
            ev.preventDefault();
            show(ev.target);
        }
    });
    window.addEventListener('scroll', hide, true);
    window.addEventListener('resize', hide);
})();

// ---------- 向导：集团邮箱 OA 指引按域名实时显隐（2026-11-10）----------
// 服务端按「页面加载时的域名」（默认 @chinatelecom.cn）渲染 .ct-guide；
// 域名是向导里唯一可改的账号要素——客户改掉域名后 OA 指引不再适用，
// 应随输入即时隐藏，改回则恢复。判断口径与服务端 is_ct_mail_domain
// 一致：trim + 去前导 @ + 大小写不敏感全等（非前缀匹配）。
(function () {
    var CT = 'chinatelecom.cn';

    function isCtDomain(v) {
        var s = (v || '').trim();
        while (s.charAt(0) === '@') s = s.slice(1);
        return s.toLowerCase() === CT;
    }

    function apply(form) {
        if (!form || !form.classList.contains('setup-card')) return;
        var input = form.querySelector('input[name="account_domain"]');
        if (!input) return;
        var on = isCtDomain(input.value);
        form.querySelectorAll('.ct-guide').forEach(function (el) {
            el.hidden = !on;
        });
        // 两条黄条全隐藏后容器仍占 gap+margin（12px 空白）。容器本身一并
        // 隐藏，否则改成非集团域名时标题上方留一道空档。
        var box = form.querySelector('.setup-alerts');
        if (box) box.hidden = !on;
    }

    // input 事件覆盖键入/粘贴/撤销/浏览器自动填充；事件委托挂 body
    document.body.addEventListener('input', function (ev) {
        var t = ev.target;
        if (t && t.name === 'account_domain') apply(t.closest('.setup-card'));
    });

    // 页面进入校准一次（不依赖服务端初始渲染，双保险）；向导被拒绝
    // 重提只换 #test-results 片段，表单 DOM 与 hidden 状态原样保留
    function initAll(root) {
        (root || document).querySelectorAll('.setup-card').forEach(apply);
    }
    document.addEventListener('DOMContentLoaded', function () { initAll(document); });
    document.body.addEventListener('htmx:afterSwap', function () { initAll(document); });
})();
