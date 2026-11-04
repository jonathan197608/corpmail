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

/** 等到某段文本出现在页面上（htmx 局部刷新后的断言基础） */
export async function waitText(selector, text, timeout = 20000) {
  await browser.waitUntil(
    async () => (await browser.$(selector).getText()).includes(text),
    { timeout, timeoutMsg: `等待「${text}」超时（selector=${selector}）` },
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
