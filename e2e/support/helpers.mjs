import fs from 'node:fs'
import path from 'node:path'
import { browser } from '@wdio/globals'

const RUN_DIR = process.env.E2E_RUN_DIR || path.resolve('artifacts', 'adhoc')
fs.mkdirSync(RUN_DIR, { recursive: true })

let seq = 0
export const shots = []

/**
 * 静默服务的焦点管理（tauri-service beforeCommand 每 $/$$/getTitle/点击前
 * 都会调 plugin:wdio|get_window_states，本环境没有 tauri-plugin-wdio IPC
 * 插件，每次白等 5s 超时）。官方设计：显式 switchToWindow 成功一次后
 * auto-focus 永久跳过（源码 afterCommand → suppressActiveWindowFocus）。
 * 本应用单窗口，焦点恢复无意义，安全。
 */
export async function suppressFocusManager() {
  if (suppressFocusManager._done) return
  const handles = await browser.getWindowHandles()
  await browser.switchToWindow(handles[0])
  suppressFocusManager._done = true
}

/**
 * 全页截图并登记到本次运行的截图清单。
 * 命名：NNN-<语义名>.png（NNN 全局递增，两次用例之间顺序即操作顺序）
 */
export async function shot(name) {
  seq += 1
  const file = path.join(RUN_DIR, `${String(seq).padStart(3, '0')}-${name}.png`)
  await browser.saveScreenshot(file)
  shots.push({ seq, name, file: path.relative(RUN_DIR, file) })
  return file
}

/** 等启动遮罩消失（base.html 的 #boot 由 app.js 首帧移除） */
export async function waitBoot() {
  await browser.waitUntil(
    async () => !(await browser.$('#boot').isExisting()),
    { timeout: 30000, timeoutMsg: '启动遮罩未消失（app.js 未执行或页面未加载）' },
  )
}

/** 读附件预览模态的开合状态（app.js 只切 hidden 属性）。
 *  不用 isDisplayed：内嵌驱动下其 checkVisibility/W3C 算法不可靠
 *  （实测模态已关仍返回 true），execute 通道已被 htmx.ajax/selectValue 实测可用。 */
export function modalOpen() {
  return browser.execute(() => {
    const m = document.getElementById('preview-modal')
    return !!m && !m.hidden
  })
}

/** 等到某段文本出现在页面上（htmx 局部刷新后的断言基础） */
export async function waitText(selector, text, timeout = 20000) {
  await browser.waitUntil(
    async () => (await browser.$(selector).getText()).includes(text),
    { timeout, timeoutMsg: `等待「${text}」超时（selector=${selector}）` },
  )
}

/** 本次运行的产物目录（mock outbox、导出落盘都在这里） */
export const runDir = () => process.env.E2E_RUN_DIR || path.resolve('artifacts', 'adhoc')

/**
 * 保存类接口的自动化驱动（导出 .eml / 附件另存 / 数据目录迁移等
 * 本来由前端先弹 Tauri 对话框选 dest 再 POST 的接口）。
 *
 * 不用 htmx.ajax + config.handler：htmx 把自定义 handler 当作
 * 「替代默认 swap handler」调用（源码 const M = i.handler||jn，onload
 * 里 M(elt,info) 即 swap 本身）——handler 只捕获响应则 DOM 永不更新，
 * swap 后的断言必然超时。改为 fetch + 手动替换目标节点：
 * 返回 {status, text, redirect, swapped}，语义与真实前端一致。
 */
export async function htmxAjax(method, url, values, target = '#toast') {
  return browser.execute(
    async (m, u, v, t) => {
      const body = Object.entries(v || {})
        .map(([k, val]) => `${encodeURIComponent(k)}=${encodeURIComponent(String(val))}`)
        .join('&')
      const resp = await fetch(u, {
        method: m,
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body,
      })
      const text = await resp.text()
      const elt = document.querySelector(t)
      let swapped = false
      if (elt) {
        const tmp = document.createElement('div')
        tmp.innerHTML = text
        const frag = tmp.firstElementChild
        if (frag && frag.id && frag.id === elt.id) {
          elt.replaceWith(frag)
        } else {
          elt.outerHTML = text
        }
        swapped = true
      }
      return {
        status: resp.status,
        text,
        // 失败链路要断言「没有跳转」：HX-Redirect 在响应头里，不在正文
        redirect: resp.headers.get('HX-Redirect'),
        swapped,
      }
    },
    method,
    url,
    values,
    target,
  )
}

/** 把 window.confirm 一律放行（hx-confirm / 批量删除确认框） */
export async function autoConfirm() {
  await browser.execute(() => {
    window.confirm = () => true
  })
}

/**
 * 设置 <select> 的值。selectByAttribute 在 WKWebView 内嵌驱动下对
 * <option> 的模拟点击不生效（值不变），改用 JS 直改 + 派发 change：
 * htmx 表单序列化读的是 DOM 值，change 事件供监听方刷新联动。
 */
export async function selectValue(selector, value) {
  await browser.execute(
    (s, v) => {
      const sel = document.querySelector(s)
      if (!sel) throw new Error(`selectValue: 找不到 ${s}`)
      sel.value = v
      sel.dispatchEvent(new Event('change', { bubbles: true }))
    },
    selector,
    value,
  )
}

/**
 * 跳转到应用内路径。
 * 页面跑在自定义协议上（macOS: ui://localhost/，Windows: http://ui.localhost/），
 * 不能用相对路径直接 browser.url()，需保留 origin。
 */
export async function goto(path) {
  const cur = await browser.getUrl()
  const m = cur.match(/^([a-z]+:\/\/[^/]+)/i)
  if (!m) throw new Error(`无法解析当前页面 origin：${cur}`)
  await browser.url(m[1] + path)
  await waitBoot()
}


/** 把本次运行登记的截图写成可点击的清单（Markdown），供人工复核。
 *  wdio 每个 spec 一个 worker 进程，清单按 shots.json 累积合并。 */
export function writeShotIndex(title = 'E2E 截图清单') {
  if (!shots.length) return null
  const jsonFile = path.join(RUN_DIR, 'shots.json')
  let all = []
  try {
    if (fs.existsSync(jsonFile)) all = JSON.parse(fs.readFileSync(jsonFile, 'utf8'))
  } catch {
    all = []
  }
  all.push(...shots)
  fs.writeFileSync(jsonFile, JSON.stringify(all, null, 2))

  const lines = [`# ${title}`, '', `运行时间：${new Date().toISOString()}`, '']
  for (const s of all) lines.push(`${s.seq}. [${s.name}](${s.file})　\`${s.file}\``)
  const file = path.join(RUN_DIR, 'index.md')
  fs.writeFileSync(file, lines.join('\n') + '\n')
  return file
}
