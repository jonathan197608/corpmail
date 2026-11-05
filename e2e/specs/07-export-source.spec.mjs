import { browser, $, $$, expect } from '@wdio/globals'
import { goto, shot, waitBoot, waitText, htmxAjax, runDir } from '../support/helpers.mjs'
import fs from 'node:fs'
import path from 'node:path'

// 07：原文视图、HTML 净化、.eml 导出（B4）、附件另存（A4）。
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

describe('07 原文视图与导出', () => {
  it('C4 原文视图：source 页含 Message-ID 与收发头', async () => {
    await goto('/')
    await waitBoot()
    await openRowBySubject('会议纪要')
    // href 含 list_query 后缀（/emails/{id}/source?folder=...），不能用 $= 锚定结尾
    await $('a[href*="/source"]').click()
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
