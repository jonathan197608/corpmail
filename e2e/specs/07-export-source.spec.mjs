import { browser, $, $$, expect } from '@wdio/globals'
import {
  goto,
  shot,
  waitBoot,
  waitText,
  htmxAjax,
  runDir,
  openSplit,
  clickByText,
} from '../support/helpers.mjs'
import fs from 'node:fs'
import path from 'node:path'

// 07：原文视图、HTML 净化、.eml 导出（B4）、导出 MD（新增）、附件另存（A4）。
// 保存类接口走 htmx.ajax 直接 POST dest（前端正常路径是先弹 Tauri 目录对话框）。
function exportDir() {
  const d = path.join(runDir(), 'export')
  fs.mkdirSync(d, { recursive: true })
  return d
}

async function openRowBySubject(text) {
  const rows = await $$('.mail-row')
  for (const row of rows) {
    if ((await row.getText()).includes(text)) {
      await row.$('.mr-link').click()
      await browser.waitUntil(
        async () => (await browser.getUrl()).includes('/emails/'),
        { timeoutMsg: `未进入「${text}」详情页` },
      )
      await waitBoot()
      return
    }
  }
  throw new Error(`列表中找不到「${text}」`)
}

async function detailId() {
  const url = await browser.getUrl()
  const m = url.match(/\/emails\/(\d+)/)
  if (!m) throw new Error(`当前 URL 不含邮件 id：${url}`)
  return m[1]
}

/** 详情页顶栏的 ⋯ 菜单（收件箱邮件有「回复▾」与「⋯」两个 split，取最后一个） */
async function openDetailMore() {
  const splits = await $$('.tb-actions .split')
  if (!splits.length) throw new Error('详情页顶栏没有 split 菜单')
  await openSplit(splits[splits.length - 1])
}

describe('07 原文视图与导出', () => {
  it('C4 原文视图：⋯ 菜单入口 → 原始信头含 Message-ID 与收发头', async () => {
    await goto('/')
    await waitBoot()
    await openRowBySubject('会议纪要')
    // 2026-11 改版：源码入口收进详情页顶栏 ⋯ 菜单（不再常驻按钮）
    await openDetailMore()
    await clickByText('.tb-actions .split-menu a', '源码')
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('/source'),
      { timeoutMsg: '未进入原文页' },
    )
    await waitBoot()
    // 默认 tab 是「HTML 源码」，纯文本邮件（会议纪要）显示空态——Message-ID
    // 与收发头在「原始信头」tab（?part=headers）
    await $('a[href*="part=headers"]').click()
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('part=headers'),
      { timeoutMsg: '未切到原始信头 tab' },
    )
    await waitBoot()
    const text = await $('body').getText()
    expect(text).toMatch(/e2e-\d+@corpmail\.test/) // fixture Message-ID
    expect(text).toContain('wangqiang@corpmail.test')
    await shot('原文视图-信头')
  })

  it('C9 原文页增强：行号 gutter + 复制按钮', async () => {
    const state = await browser.execute(() => {
      const gutter = document.querySelector('.src-gutter')
      const pre = document.getElementById('src-content')
      return {
        gutterLines: gutter ? gutter.textContent.trim().split('\n').length : 0,
        content: pre ? pre.textContent.length : 0,
        copyBtn: !!document.querySelector('.src-copy'),
      }
    })
    expect(state.content).toBeGreaterThan(0)
    expect(state.gutterLines).toBeGreaterThan(1) // 行号与正文行数对应
    expect(state.copyBtn).toBe(true)
    // 点一下不报错即可（剪贴板 API 在 WebDriver 里可能被拒，app.js 有降级路径）
    await $('.src-copy').click()
    await shot('原文视图-行号与复制')
  })

  it('C7 HTML 正文净化：渲染为 md-html 且无 script', async () => {
    await goto('/')
    await waitBoot()
    await openRowBySubject('季度报表')
    await expect($('.md-body.md-html')).toBeExisting()
    const html = await browser.execute(
      () => document.querySelector('.md-body.md-html').innerHTML,
    )
    expect(html).toContain('<h2>')
    expect(html).not.toContain('<script')
    await shot('读信-HTML正文')
  })

  it('C5 导出 .eml（B4）：POST dest → toast + 文件落盘内容含头', async () => {
    const id = await detailId()
    const dest = path.join(exportDir(), 'quarterly.eml')
    fs.rmSync(dest, { force: true })
    const res = await htmxAjax('POST', `/emails/${id}/export-eml`, { dest })
    expect(res.text).toContain('已导出到')
    await browser.waitUntil(() => fs.existsSync(dest), {
      timeoutMsg: '.eml 文件未落盘',
    })
    const raw = fs.readFileSync(dest, 'utf8')
    expect(raw).toContain('Message-ID:')
    expect(raw).toContain('lina@corpmail.test')
    expect(raw).toContain('report.csv')
    await shot('导出-eml成功toast')
  })

  it('C8 导出 Markdown：front matter + 正文 + 附件清单', async () => {
    const id = await detailId()
    const dest = path.join(exportDir(), 'quarterly.md')
    fs.rmSync(dest, { force: true })
    const res = await htmxAjax('POST', `/emails/${id}/export-md`, { dest })
    expect(res.text).toContain('已导出到')
    await browser.waitUntil(() => fs.existsSync(dest), {
      timeoutMsg: '.md 文件未落盘',
    })
    const md = fs.readFileSync(dest, 'utf8')
    // YAML front matter：subject/from/date/folder 均在此
    expect(md.startsWith('---\n')).toBe(true)
    expect(md).toContain('subject:')
    expect(md).toContain('from:')
    expect(md).toContain('folder:')
    expect(md).toContain('## 正文')
    expect(md).toContain('## 附件')
    expect(md).toContain('report.csv')
    // 内容与源邮件一致（fixture 正文含「研发,120000」的表格数据）
    expect(md).toContain('季度')
    await shot('导出-md成功toast')
  })

  it('C6 全部另存（A4）：toast 报告数量 + CSV 内容一致', async () => {
    await goto('/')
    await waitBoot()
    await openRowBySubject('季度报表')
    const id = await detailId()
    const dir = path.join(exportDir(), 'attachments')
    fs.rmSync(dir, { recursive: true, force: true })
    fs.mkdirSync(dir, { recursive: true })
    const res = await htmxAjax('POST', `/emails/${id}/attachments/all`, {
      dest_dir: dir,
    })
    expect(res.text).toContain('已保存 1 个附件')
    const csv = path.join(dir, 'report.csv')
    await browser.waitUntil(() => fs.existsSync(csv), {
      timeoutMsg: 'report.csv 未落盘',
    })
    expect(fs.readFileSync(csv, 'utf8')).toContain('研发,120000')
    await shot('导出-附件全部另存')
  })
})

