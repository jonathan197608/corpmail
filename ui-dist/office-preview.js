// Office 附件预览（B8）：本地解析 DOCX/XLSX/PPTX，产物净化后注入 sandbox iframe。
// 安全边界：
// - 附件字节来自服务端已清洗副本（宏/OLE/外部关系已移除）；
// - 渲染产物走 DOM 白名单净化，去掉脚本/事件/外部资源；
// - iframe sandbox=""（无脚本、无同源），CSP 仅允许 data: 图片与内联样式；
// - 解析失败时 DOCX/XLSX 可降级纯文本；PPTX 按产品要求提示下载。

(function () {
    'use strict';

    const KINDS = ['docx', 'xlsx', 'pptx', 'xls'];
    const SCRIPT_CACHE = new Map();
    const MAX_SHEETS = 20;
    const MAX_ROWS = 2000;
    const MAX_COLS = 100;
    const MAX_INLINE_IMAGE = 8 * 1024 * 1024;
    const FETCH_TIMEOUT = 20000;

    const state = {
        kind: '',
        url: '',
        emailId: 0,
        attachId: 0,
        pv: null,
        wb: null,
        sheets: [],
        sheetIndex: 0,
        slideIndex: 0,
        objectUrl: ''
    };

    function $(id) { return document.getElementById(id); }
    function modal() { return $('preview-modal'); }
    function panel() { return modal() ? modal().querySelector('.pm-panel') : null; }
    function bodyEl() { return $('pm-body'); }
    function toolbarEl() { return $('pm-toolbar'); }

    // ---------- 懒加载 vendor ----------
    function loadScript(src) {
        if (SCRIPT_CACHE.has(src)) return SCRIPT_CACHE.get(src);
        const p = new Promise((resolve, reject) => {
            const s = document.createElement('script');
            s.src = src;
            s.onload = () => resolve();
            s.onerror = () => reject(new Error('预览组件加载失败：' + src));
            document.head.appendChild(s);
        }).catch(err => {
            SCRIPT_CACHE.delete(src);
            throw err;
        });
        SCRIPT_CACHE.set(src, p);
        return p;
    }

    async function loadFor(kind) {
        if (kind === 'docx') {
            await loadScript('/vendor/office/jszip.min.js');
            await loadScript('/vendor/office/docx-preview.min.js');
            if (!window.docx) throw new Error('DOCX 预览组件不可用');
        } else if (kind === 'xlsx' || kind === 'xls') {
            await loadScript('/vendor/office/xlsx.full.min.js');
            if (!window.XLSX) throw new Error('XLSX 预览组件不可用');
        } else if (kind === 'pptx') {
            await loadScript('/vendor/office/pptx-preview.umd.js');
            if (!window.pptxPreview) throw new Error('PPTX 预览组件不可用');
        }
    }

    // ---------- 状态复位 ----------
    function reset() {
        if (state.objectUrl) {
            try { URL.revokeObjectURL(state.objectUrl); } catch (e) { /* 忽略 */ }
            state.objectUrl = '';
        }
        if (state.pv) {
            try { state.pv.destroy(); } catch (e) { /* 忽略销毁异常 */ }
        }
        state.kind = '';
        state.url = '';
        state.emailId = 0;
        state.attachId = 0;
        state.pv = null;
        state.wb = null;
        state.sheets = [];
        state.sheetIndex = 0;
        state.slideIndex = 0;
        const p = panel();
        if (p) p.classList.remove('office');
        const b = bodyEl();
        if (b) {
            b.classList.remove('office');
            b.innerHTML = '';
        }
        const t = toolbarEl();
        if (t) {
            t.hidden = true;
            t.innerHTML = '';
        }
    }

    window.mbxOfficeReset = reset;

    // ---------- URL / 数据 ----------
    function idsFromUrl(url) {
        const m = /\/emails\/(\d+)\/attachments\/(\d+)\/view(?:\?.*)?$/.exec(url || '');
        return m ? { emailId: Number(m[1]), attachId: Number(m[2]) } : { emailId: 0, attachId: 0 };
    }

    function endpoint(url, suffix) {
        return String(url || '').replace(/\/view(?:\?.*)?$/, '/' + suffix);
    }

    async function fetchBytes(url) {
        const ctrl = new AbortController();
        const timer = setTimeout(() => ctrl.abort(), FETCH_TIMEOUT);
        try {
            const r = await fetch(url, { signal: ctrl.signal });
            if (!r.ok) {
                let msg = 'HTTP ' + r.status;
                try { msg = (await r.text()) || msg; } catch (e) { /* 忽略 */ }
                throw new Error(msg);
            }
            return await r.arrayBuffer();
        } catch (e) {
            if (e && e.name === 'AbortError') throw new Error('加载超时，请下载后用系统应用打开。');
            throw e;
        } finally {
            clearTimeout(timer);
        }
    }

    // ---------- 净化 ----------
    const BLOCK_TAGS = new Set([
        'script', 'iframe', 'object', 'embed', 'link', 'meta', 'base',
        'form', 'input', 'button', 'textarea', 'select', 'style'
    ]);

    function safeUrl(raw, forSrc) {
        const v = String(raw || '').trim().replace(/[\u0000-\u0020]+/g, '');
        const lower = v.toLowerCase();
        if (lower.startsWith('data:image/')) return true;
        if (forSrc) return lower.startsWith('blob:');
        if (v.startsWith('#')) return true;
        return lower.startsWith('http://') || lower.startsWith('https://')
            || lower.startsWith('mailto:') || lower.startsWith('tel:');
    }

    function sanitizeCss(css) {
        return String(css || '')
            .replace(/@import[\s\S]*?;/gi, '')
            .replace(/url\s*\(\s*['"]?\s*(?:https?:|javascript:|data:text\/html)[^)]*\)/gi, 'none')
            .replace(/expression\s*\(/gi, '(')
            .replace(/<\/style/gi, '<\\/style');
    }

    function sanitizeElement(root) {
        const all = Array.from(root.querySelectorAll('*'));
        for (const el of all) {
            const tag = el.tagName.toLowerCase();
            if (BLOCK_TAGS.has(tag)) {
                el.remove();
                continue;
            }
            for (const attr of Array.from(el.attributes)) {
                const name = attr.name.toLowerCase();
                if (name.startsWith('on') || name === 'srcdoc' || name === 'formaction'
                    || name === 'action' || name === 'http-equiv' || name === 'srcset'
                    || name === 'poster' || name === 'background') {
                    el.removeAttribute(attr.name);
                    continue;
                }
                if (name === 'style') {
                    el.setAttribute('style', sanitizeCss(attr.value));
                    continue;
                }
                if (name === 'src' || name === 'href' || name === 'xlink:href') {
                    // SVG <image> 的 href/xlink:href 等价于图片 src，允许 blob/data
                    const imageRef = tag === 'image' || name === 'src';
                    if (!safeUrl(attr.value, imageRef)) el.removeAttribute(attr.name);
                }
            }
            // 预览内不导航：锚点只保留文字
            if (tag === 'a') el.removeAttribute('href');
        }
    }

    async function inlineBlobImages(root) {
        const nodes = Array.from(root.querySelectorAll('img,image'));
        for (const el of nodes) {
            const attr = el.tagName.toLowerCase() === 'image' ? 'href' : 'src';
            const src = el.getAttribute(attr) || el.getAttribute('xlink:href') || '';
            if (!src.toLowerCase().startsWith('blob:')) continue;
            try {
                const r = await fetch(src);
                const blob = await r.blob();
                if (blob.size > MAX_INLINE_IMAGE) continue;
                const dataUrl = await new Promise((resolve, reject) => {
                    const fr = new FileReader();
                    fr.onload = () => resolve(String(fr.result || ''));
                    fr.onerror = () => reject(new Error('图片读取失败'));
                    fr.readAsDataURL(blob);
                });
                el.setAttribute(attr, dataUrl);
                el.removeAttribute('xlink:href');
            } catch (e) {
                el.removeAttribute(attr);
                el.removeAttribute('xlink:href');
            }
        }
    }

    async function prepareHtml(html) {
        const tpl = document.createElement('template');
        tpl.innerHTML = String(html || '');
        sanitizeElement(tpl.content);
        await inlineBlobImages(tpl.content);
        return tpl.innerHTML;
    }

    function buildSrcdoc(html, css) {
        const csp = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; "
            + "font-src data:; base-uri 'none'; form-action 'none'";
        return '<!doctype html><html><head><meta charset="utf-8">'
            + '<meta http-equiv="Content-Security-Policy" content="' + csp + '">'
            + '<style>' + sanitizeCss(css) + '</style></head><body>' + html + '</body></html>';
    }

    async function showIframe(html, css) {
        const clean = await prepareHtml(html);
        // 用 Blob URL 承载（Tauri CSP 已允许 frame-src blob:），比 srcdoc 在各
        // WebView 引擎下更一致；sandbox="" 保证无脚本、无同源权限
        if (state.objectUrl) {
            try { URL.revokeObjectURL(state.objectUrl); } catch (e) { /* 忽略 */ }
        }
        state.objectUrl = URL.createObjectURL(new Blob([buildSrcdoc(clean, css)], { type: 'text/html' }));
        const frame = document.createElement('iframe');
        frame.setAttribute('sandbox', '');
        frame.setAttribute('referrerpolicy', 'no-referrer');
        frame.title = $('pm-name') ? $('pm-name').textContent : '附件预览';
        frame.src = state.objectUrl;
        const b = bodyEl();
        b.innerHTML = '';
        b.classList.add('office');
        b.appendChild(frame);
    }

    function showLoading(text) {
        const b = bodyEl();
        if (!b) return;
        b.innerHTML = '';
        b.classList.add('office');
        const wrap = document.createElement('div');
        wrap.className = 'pm-loading';
        const spin = document.createElement('i');
        const label = document.createElement('span');
        label.textContent = text || '正在解析…';
        wrap.appendChild(spin);
        wrap.appendChild(label);
        b.appendChild(wrap);
    }

    // ---------- 工具栏 ----------
    function button(label, title, onClick) {
        const b = document.createElement('button');
        b.type = 'button';
        b.className = 'btn small';
        b.textContent = label;
        if (title) b.title = title;
        b.addEventListener('click', onClick);
        return b;
    }

    function toolbarLayout() {
        const t = toolbarEl();
        t.innerHTML = '';
        t.hidden = false;
        const left = document.createElement('div');
        left.className = 'pm-tools-left';
        const right = document.createElement('div');
        right.className = 'pm-tools-right';
        t.appendChild(left);
        t.appendChild(right);
        return { left, right };
    }

    function addDownload(right) {
        if (!state.emailId || !state.attachId) return;
        const name = $('pm-name') ? $('pm-name').textContent : '附件';
        right.appendChild(button('另存为', '另存到本机', () => {
            if (window.mbxSaveAttachment) window.mbxSaveAttachment(state.emailId, state.attachId, name);
        }));
    }

    // ---------- DOCX ----------
    async function renderDocx(buf) {
        await loadFor('docx');
        const bodyHost = document.createElement('div');
        const styleHost = document.createElement('div');
        await window.docx.renderAsync(buf, bodyHost, styleHost, {
            inWrapper: true,
            breakPages: true,
            renderHeaders: true,
            renderFooters: true,
            useBase64URL: true,
            ignoreWidth: false,
            ignoreHeight: false
        });
        const css = styleHost.innerHTML + DOCX_CSS;
        await showIframe(bodyHost.innerHTML, css);
        const { right } = toolbarLayout();
        const left = toolbarEl().querySelector('.pm-tools-left');
        const label = document.createElement('span');
        label.className = 'pm-tools-label';
        label.textContent = 'Word 文档';
        left.appendChild(label);
        right.appendChild(button('文本模式', '渲染异常时可切换纯文本预览', () => showTextFallback()));
        addDownload(right);
    }

    // ---------- XLSX / XLS ----------
    function limitSheet(ws, maxRows, maxCols) {
        if (!ws || !ws['!ref']) return ws;
        const range = window.XLSX.utils.decode_range(ws['!ref']);
        const end = {
            r: Math.min(range.e.r, maxRows - 1),
            c: Math.min(range.e.c, maxCols - 1)
        };
        if (end.r === range.e.r && end.c === range.e.c) return ws;
        const out = { '!ref': window.XLSX.utils.encode_range({ s: { r: 0, c: 0 }, e: end }) };
        const merges = (ws['!merges'] || []).filter(m => m.e.r <= end.r && m.e.c <= end.c);
        if (merges.length) out['!merges'] = merges;
        for (let r = 0; r <= end.r; r++) {
            for (let c = 0; c <= end.c; c++) {
                const addr = window.XLSX.utils.encode_cell({ r, c });
                if (ws[addr]) out[addr] = ws[addr];
            }
        }
        return out;
    }

    function sheetTruncated(ws) {
        if (!ws || !ws['!ref']) return false;
        const r = window.XLSX.utils.decode_range(ws['!ref']);
        return r.e.r + 1 > MAX_ROWS || r.e.c + 1 > MAX_COLS;
    }

    async function renderSheet(index) {
        const name = state.sheets[index];
        if (!name) return;
        state.sheetIndex = index;
        const ws = state.wb.Sheets[name];
        const html = window.XLSX.utils.sheet_to_html(limitSheet(ws, MAX_ROWS, MAX_COLS), { editable: false });
        await showIframe(html, XLSX_CSS);
        const { left, right } = toolbarLayout();
        state.sheets.forEach((sheetName, i) => {
            const tab = document.createElement('button');
            tab.type = 'button';
            tab.className = 'pm-sheet-tab' + (i === index ? ' on' : '');
            tab.textContent = sheetName;
            tab.title = sheetName;
            tab.addEventListener('click', () => renderSheet(i));
            left.appendChild(tab);
        });
        if (sheetTruncated(ws)) {
            const tip = document.createElement('span');
            tip.className = 'pm-tools-note';
            tip.textContent = '仅预览前 ' + MAX_ROWS + ' 行 / ' + MAX_COLS + ' 列';
            left.appendChild(tip);
        }
        right.appendChild(button('文本模式', '渲染异常时可切换纯文本预览', () => showTextFallback()));
        addDownload(right);
    }

    async function renderXlsx(buf) {
        await loadFor(state.kind);
        const wb = window.XLSX.read(new Uint8Array(buf), { type: 'array' });
        state.wb = wb;
        state.sheets = (wb.SheetNames || []).slice(0, MAX_SHEETS);
        if (!state.sheets.length) throw new Error('工作簿中没有可预览的工作表。');
        await renderSheet(0);
    }

    // ---------- PPTX ----------
    async function renderPptx(buf) {
        await loadFor('pptx');
        // 先探测幻灯片尺寸（4:3 / 16:9），再按真实宽高比初始化，避免黑边/裁切
        const probeHost = document.createElement('div');
        const probe = window.pptxPreview.init(probeHost, { width: 960, height: 540 });
        await probe.load(buf);
        const w = Number(probe.pptx && probe.pptx.width) || 0;
        const h = Number(probe.pptx && probe.pptx.height) || 0;
        try { probe.destroy(); } catch (e) { /* 忽略 */ }
        const height = w > 0 && h > 0 ? Math.round(960 * h / w) : 540;

        const host = document.createElement('div');
        const pv = window.pptxPreview.init(host, { width: 960, height: height, mode: 'slide' });
        await pv.preview(buf);
        if (!pv.slideCount) throw new Error('PPTX 中没有可预览的幻灯片。');
        state.pv = pv;
        state.slideIndex = 0;
        await renderSlide(0);
    }

    async function renderSlide(index) {
        const pv = state.pv;
        if (!pv) return;
        if (index < 0 || index >= pv.slideCount) return;
        state.slideIndex = index;
        pv.renderSingleSlide(index);
        await new Promise(r => setTimeout(r, 50));
        // 渲染宿主不在文档树中，需从 pv.dom 查找（renderSingleSlide 把节点挂到 pv.dom）
        const host = pv.dom || document;
        const el = host.querySelector('.pptx-preview-slide-wrapper-' + index)
            || host.querySelector('.pptx-preview-slide-wrapper');
        const html = el ? el.outerHTML : '';
        if (!html) throw new Error('幻灯片渲染结果为空。');
        await showIframe(html, PPTX_CSS);
        const { left, right } = toolbarLayout();
        left.appendChild(button('‹', '上一页', () => renderSlide(state.slideIndex - 1)));
        const page = document.createElement('span');
        page.className = 'pm-tools-label';
        page.textContent = (index + 1) + ' / ' + pv.slideCount;
        left.appendChild(page);
        left.appendChild(button('›', '下一页', () => renderSlide(state.slideIndex + 1)));
        right.appendChild(button('下载', '用 PowerPoint 打开以查看完整效果', () => {
            const name = $('pm-name') ? $('pm-name').textContent : '附件';
            if (window.mbxSaveAttachment) window.mbxSaveAttachment(state.emailId, state.attachId, name);
        }));
        // 页码按钮在首/尾禁用
        const btns = left.querySelectorAll('button');
        if (btns[0]) btns[0].disabled = index <= 0;
        if (btns[1]) btns[1].disabled = index >= pv.slideCount - 1;
    }

    // ---------- 文本兜底 / 错误 ----------
    async function showTextFallback() {
        const b = bodyEl();
        if (!b) return;
        showLoading('正在提取文本…');
        try {
            const r = await fetch(endpoint(state.url, 'office-text'));
            const text = await r.text();
            if (!r.ok) throw new Error(text || '文本提取失败');
            b.innerHTML = '';
            b.classList.add('office');
            const note = document.createElement('div');
            note.className = 'pm-fallback-note';
            note.textContent = '已降级为纯文本预览（版式可能丢失）；如需完整排版请下载后用 Office 打开。';
            const pre = document.createElement('pre');
            pre.className = 'pm-text';
            pre.textContent = text;
            b.appendChild(note);
            b.appendChild(pre);
            const { right } = toolbarLayout();
            const left = toolbarEl().querySelector('.pm-tools-left');
            const label = document.createElement('span');
            label.className = 'pm-tools-label';
            label.textContent = '纯文本';
            left.appendChild(label);
            right.appendChild(button('重新渲染', '重新尝试图形预览', () => open(state.url, state.kind, $('pm-name').textContent)));
            addDownload(right);
        } catch (e) {
            showError((e && e.message) || '文本提取失败', false);
        }
    }

    function showError(message, allowText) {
        const b = bodyEl();
        if (!b) return;
        b.innerHTML = '';
        b.classList.add('office');
        const wrap = document.createElement('div');
        wrap.className = 'pm-error';
        const title = document.createElement('div');
        title.className = 'pm-error-title';
        title.textContent = '无法预览';
        const msg = document.createElement('p');
        msg.className = 'pm-error-msg';
        msg.textContent = message || '预览失败';
        const actions = document.createElement('div');
        actions.className = 'pm-error-actions';
        if (allowText) {
            actions.appendChild(button('纯文本预览', '提取文字内容（版式丢失）', () => showTextFallback()));
        }
        addDownload(actions);
        actions.appendChild(button('关闭', '关闭预览', () => window.mbxClosePreview && window.mbxClosePreview()));
        wrap.appendChild(title);
        wrap.appendChild(msg);
        wrap.appendChild(actions);
        b.appendChild(wrap);
        toolbarEl().hidden = true;
    }

    // ---------- 入口 ----------
    async function open(url, kind, name) {
        reset();
        state.kind = kind;
        state.url = url;
        const ids = idsFromUrl(url);
        state.emailId = ids.emailId;
        state.attachId = ids.attachId;
        const m = modal();
        const p = panel();
        if (!m || !p) return;
        m.hidden = false;
        p.classList.add('office');
        const nameEl = $('pm-name');
        if (nameEl && name) nameEl.textContent = name;
        showLoading('正在加载…');
        try {
            const buf = await fetchBytes(endpoint(url, 'office'));
            showLoading('正在解析…');
            // 让浏览器先画一帧加载态，再做同步解析
            await new Promise(r => setTimeout(r, 20));
            if (kind === 'docx') await renderDocx(buf);
            else if (kind === 'xlsx' || kind === 'xls') await renderXlsx(buf);
            else if (kind === 'pptx') await renderPptx(buf);
            else throw new Error('不支持的预览类型');
        } catch (e) {
            let msg = (e && e.message) ? e.message : '预览失败';
            // PPTX 不做文本降级：解析失败统一提示下载（加密/超限等明确原因保留）
            if (kind === 'pptx' && !/加密|上限|不支持|损坏/.test(msg)) {
                msg = '该 PPTX 暂无法预览，请下载后用 Microsoft PowerPoint 打开。';
            }
            showError(msg, kind === 'docx' || kind === 'xlsx' || kind === 'xls');
        }
    }

    window.mbxPreviewOffice = function (url, kind, name) {
        if (KINDS.indexOf(kind) < 0) {
            showError('不支持的预览类型', false);
            return;
        }
        open(url, kind, name);
    };

    // 预览样式：iframe 内部基础排版（不含任何外部资源）
    const DOCX_CSS = `
        html, body { height: 100%; }
        body { margin: 0; background: #eef2f7; font-family: "Microsoft YaHei", "PingFang SC", system-ui, sans-serif; }
        .docx-wrapper { background: #eef2f7; padding: 16px 0; }
        .docx-wrapper > section.docx { box-shadow: 0 1px 4px rgba(15,23,42,.12); margin-bottom: 16px; }
        img { max-width: 100%; }
    `;
    const XLSX_CSS = `
        html, body { height: 100%; }
        body { margin: 0; padding: 16px; background: #fff; color: #1f2937;
               font-family: "Microsoft YaHei", "PingFang SC", system-ui, sans-serif; font-size: 13px; }
        table { border-collapse: collapse; table-layout: auto; }
        td, th { border: 1px solid #cbd5e1; padding: 4px 8px; white-space: nowrap; vertical-align: top; }
        tr:first-child td { background: #f8fafc; }
    `;
    const PPTX_CSS = `
        html, body { height: 100%; }
        body { margin: 0; padding: 12px; background: #fff; box-sizing: border-box; overflow: auto; }
        .pptx-preview-slide-wrapper { position: relative !important; top: 0 !important;
            margin: 0 auto !important; box-shadow: 0 1px 4px rgba(15,23,42,.15); }
    `;
})();