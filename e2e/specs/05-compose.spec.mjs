import { browser, $, $$, expect } from '@wdio/globals'
import { goto, shot, waitBoot, waitText, runDir } from '../support/helpers.mjs'
import { sentMails } from '../support/mock.mjs'
import fs from 'node:fs'
import path from 'node:path'

// 05：撰写页、草稿自动保存/存草稿、立即发送（SMTP 落盘证据）、
// 撤销发送窗口、回复/转发/附加 .eml 的预填上下文（D1–D9）。
const COMPOSE = { to: 'input[name="to_addrs"]', subject: 'input[name="subject"]', body: '#body-text' }

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
      return row
    }
  }
  throw new Error(`列表中找不到「${text}」`)
}

describe('05 撰写与发送', () => {
  it('D1 撰写页初始态：标题、模板下拉、空表单', async () => {
    // 前置造一条模板：compose 的 #tpl-select 只在用户有模板时渲染
    // （compose.html `{% if templates %}`），独立 HOME 初始为空表。
    // 走 settings·模板 pane 的真实表单路径，顺带覆盖 B2 的新增入口。
    await goto('/settings/templates')
    await waitText('#settings-pane', '邮件模板')
    await $('.tpl-form input[name="name"]').setValue('E2E 模板')
    await $('.tpl-form textarea[name="body_text"]').setValue('收到，我这边核对后回复您。')
    await $('.tpl-form button[type="submit"]').click()
    await waitText('#settings-pane', '✓ 已保存')
    await waitText('#settings-pane', 'E2E 模板')
    await shot('设置-新增模板成功')

    await goto('/compose')
    await waitText('h1', '撰写新邮件')
    await expect($('input[name="to_addrs"]')).toHaveValue('')
    await expect($('#tpl-select')).toBeExisting()
    // 下拉里应有刚建的模板（DOM 状态用 execute 读， WKWebView 下 select
    // 文本抽取不可靠）
    const opts = await browser.execute(() =>
      [...document.querySelectorAll('#tpl-select option')].map((o) => o.textContent),
    )
    expect(opts.some((t) => t.includes('E2E 模板'))).toBe(true)
    await shot('撰写-初始态含模板下拉')
  })

  it('D2 草稿自动保存：停止输入 5s 后 hint 显示「已自动保存」', async () => {
    await goto('/compose')
    await $(COMPOSE.subject).setValue('E2E 自动保存草稿')
    await $(COMPOSE.body).setValue('自动保存验证正文。')
    await browser.waitUntil(
      async () => (await $('#autosave-hint').getText()).includes('已自动保存'),
      { timeout: 15000, timeoutMsg: '5s 防抖后未出现自动保存提示' },
    )
    await shot('撰写-自动保存提示')
  })

  it('D3 存草稿：跳转带 saved=1，草稿箱出现该行', async () => {
    await $(COMPOSE.to).setValue('draft@corpmail.test')
    await $('button[type="submit"]').click()
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('saved=1'),
      { timeoutMsg: '存草稿未跳转确认页' },
    )
    await waitBoot()
    await shot('撰写-存草稿成功')

    await goto('/?folder=draft')
    await waitBoot()
    await waitText('.mail-list', 'E2E 自动保存草稿')
    await shot('草稿箱-新草稿')
  })

  it('D4 立即发送：outbox 落盘（To/主题正确）+ 发件箱出现该邮件', async () => {
    const before = sentMails().length
    await goto('/compose')
    await $(COMPOSE.to).setValue('dest@corpmail.test')
    await $(COMPOSE.subject).setValue('E2E 发送链路验证')
    await $(COMPOSE.body).setValue('这封信经 mock SMTP 发出，outbox 里应能找到。')
    await shot('撰写-待发送')
    await $('button.btn.primary.big').click() // 📨 发送

    // 跳回列表后 app.js 首帧立即 replaceState 清掉 just_sent（防刷新重复提示），
    // URL 断言是竞态必挂——撤销窗口 toast 出现才是「已排入队列」的可靠证据
    await browser.waitUntil(
      async () => (await $('.toast.busy').isExisting()),
      { timeoutMsg: '发送后未出现撤销窗口 toast（可能校验失败停在撰写页）' },
    )
    await waitText('.toast', '发送队列')
    await shot('发送-撤销窗口toast')

    await browser.waitUntil(
      () => sentMails().length > before,
      { timeout: 45000, timeoutMsg: 'mock outbox 未收到信（SMTP 派发超时）' },
    )
    const mail = sentMails().at(-1)
    expect(mail.raw).toContain('To: dest@corpmail.test')
    // 中文主题走 RFC 2047 编码，解开比对
    const m = mail.raw.match(/=\?utf-8\?B\?([^?]+)\?=/)
    expect(m && Buffer.from(m[1], 'base64').toString('utf8')).toContain('E2E 发送链路验证')
    fs.writeFileSync(path.join(runDir(), 'outbox-last.eml'), mail.raw)

    await goto('/?folder=sent')
    await waitBoot()
    await waitText('.mail-list', 'E2E 发送链路验证')
    await shot('发送-发件箱落位')
  })

  it('D5 撤销发送：toast 撤销按钮 8s 窗口内点击 → 回撰写页', async () => {
    await goto('/compose')
    await $(COMPOSE.to).setValue('undo@corpmail.test')
    await $(COMPOSE.subject).setValue('E2E 撤销窗口验证')
    await $(COMPOSE.body).setValue('发出后立即撤销。')
    await $('button.btn.primary.big').click()
    // 等 toast（URL just_sent 已被 app.js replaceState 清掉，见 D4 注释）
    await browser.waitUntil(
      async () => (await $('.toast.busy').isExisting()),
      { timeoutMsg: '未回列表/撤销 toast 未出现' },
    )
    await waitBoot()

    // toast 撤销按钮只在 8s 窗口内有效，出现即刻点
    await $('.toast-undo').click()
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('undone=1'),
      { timeout: 15000, timeoutMsg: '撤销未跳回撰写页（可能错过 8s 窗口）' },
    )
    await waitBoot()
    await waitText('.compose-note', '已撤销发送')
    await shot('发送-已撤销')
  })

  it('D6 回复预填：收件人=原发件人，正文含引用', async () => {
    await goto('/')
    await waitBoot()
    const rows = await $$('.mail-row')
    let id = null
    for (const row of rows) {
      if ((await row.getText()).includes('会议纪要')) {
        id = await row.getAttribute('data-mail-id')
        break
      }
    }
    expect(id).not.toBeNull()
    await goto(`/compose?reply_to=${id}`)
    await waitBoot()
    await waitText('h1', '回复邮件')
    const to = await $(COMPOSE.to).getValue()
    expect(to).toContain('wangqiang@corpmail.test')
    const body = await $(COMPOSE.body).getValue()
    expect(body).toContain('参会人')
    await shot('撰写-回复预填')
  })

  it('D7 回复全部：标题为「回复全部」', async () => {
    await goto('/')
    await waitBoot()
    const id = await (await $$('.mail-row'))[0].getAttribute('data-mail-id')
    await goto(`/compose?reply_all_to=${id}`)
    await waitBoot()
    await waitText('h1', '回复全部')
    await shot('撰写-回复全部')
  })

  it('D8 转发：主题 Fwd: 前缀 +「将携带 1 个附件」提示', async () => {
    await goto('/')
    await waitBoot()
    const rows = await $$('.mail-row')
    for (const row of rows) {
      if ((await row.getText()).includes('季度报表')) {
        const id = await row.getAttribute('data-mail-id')
        await goto(`/compose?forward_from=${id}`)
        await waitBoot()
        await waitText('h1', '转发邮件')
        expect(await $(COMPOSE.subject).getValue()).toContain('Fwd: ')
        await waitText('.compose-note', '1 个附件')
        await shot('撰写-转发带附件')
        return
      }
    }
    throw new Error('找不到季度报表邮件')
  })

  it('D9 附加 .eml 转发（B6）：提示以 xxx.eml 附件加入', async () => {
    await goto('/')
    await waitBoot()
    const rows = await $$('.mail-row')
    const id = await rows[0].getAttribute('data-mail-id')
    await goto(`/compose?attach_eml=${id}`)
    await waitBoot()
    await waitText('h1', '作为附件转发')
    const note = await $('.compose-note').getText()
    expect(note).toMatch(/\.eml/)
    await shot('撰写-附加eml转发')
  })
})
