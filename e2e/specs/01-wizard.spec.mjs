import { browser, $, $$, expect } from '@wdio/globals'
import { shot, waitBoot, waitText } from '../support/helpers.mjs'

const IMAP_PORT = process.env.E2E_IMAP_PORT
const SMTP_PORT = process.env.E2E_SMTP_PORT
const MAIL_COUNT = Number(process.env.E2E_MAIL_COUNT || 3)

// 01：首启向导 —— 双通道真实连接测试（指向本机 mock）→ 保存 → 立即首拉。
// 这是核心链路用例：它同时覆盖 conncheck、配置落盘、凭据存储、
// 收件线程、模板渲染与列表刷新。
describe('01 首启向导与首次收件', () => {
  it('向导表单预填公司邮箱默认值（465 / 993 / 自动）', async () => {
    await waitBoot()
    await $('a.btn.primary.big').click()
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('/welcome/setup'),
      { timeout: 15000, timeoutMsg: '未进入向导页' },
    )
    await waitBoot()

    await expect($('input[name="smtp_port"]')).toHaveValue('465')
    await expect($('input[name="recv_port"]')).toHaveValue('993')
    await expect($('select[name="recv_security"]')).toHaveValue('auto')
    await shot('向导-默认配置')
  })

  it('必填校验：账号/密码为空时提交不落库', async () => {
    await $('button[type="submit"]').click()
    // 浏览器的 required 校验直接拦下，页面仍停在向导
    expect(await browser.getUrl()).toContain('/welcome/setup')
    await shot('向导-必填校验')
  })

  it('指向本机 mock 服务器后「测试并保存」通过', async () => {
    await $('input[name="account_prefix"]').setValue('e2e')
    await $('input[name="account_domain"]').setValue('corpmail.test')
    await $('input[name="password"]').setValue('e2e-password')
    await $('input[name="display_name"]').setValue('自动化测试')

    await $('input[name="smtp_host"]').setValue('127.0.0.1')
    await $('input[name="smtp_port"]').setValue(SMTP_PORT)
    await $('select[name="smtp_security"]').selectByAttribute('value', 'plain')
    await $('input[name="recv_host"]').setValue('127.0.0.1')
    await $('input[name="recv_port"]').setValue(IMAP_PORT)
    await $('select[name="recv_security"]').selectByAttribute('value', 'plain')
    await shot('向导-已填写本地服务器')

    await $('button[type="submit"]').click()
    // 提交后要么整页跳转 done，要么表单区出现错误片段——出现错误立即快速失败，
    // 把错误文本带进超时信息（否则只能干等 60s 后对着 getUrl 轮询日志猜）
    await browser.waitUntil(
      async () => {
        if ((await browser.getUrl()).includes('/welcome/done')) return true
        const text = await $('body').getText()
        if (/未保存|设置失败|保存失败|密码保存失败/.test(text)) {
          throw new Error(`向导提交失败：${text.match(/[^。]{0,120}(失败[^。]{0,80})/)?.[0] ?? text.slice(0, 120)}`)
        }
        return false
      },
      { timeout: 60000, timeoutMsg: '双通道测试未通过或未跳转完成页' },
    )
    await waitBoot()
    await shot('向导-配置完成')
  })

  it('完成页显示账号、服务器与凭据保管说明', async () => {
    const text = await $('body').getText()
    expect(text).toContain('e2e@corpmail.test')
    expect(text).toContain('127.0.0.1')
    expect(text).toContain('密码已存入本机凭据保管')
  })

  it('进入主界面后首拉把 mock 邮件写入列表', async () => {
    await $('a.btn.primary.big').click()
    await browser.waitUntil(
      async () => (await browser.getUrl()).match(/^ui:\/\/localhost\/?$|^http:\/\/ui\.localhost\/?$/) !== null,
      { timeout: 20000, timeoutMsg: '未回到主界面' },
    )
    await waitBoot()

    await browser.waitUntil(
      async () => (await $$('.mail-row')).length >= MAIL_COUNT,
      { timeout: 90000, timeoutMsg: '首拉未在 90s 内写入邮件（看收件状态条错误）' },
    )
    await waitText('.mail-list', '会议纪要')
    await shot('收件箱-首拉完成')
  })

  it('未读标记与附件标记按服务器 FLAGS/结构渲染', async () => {
    expect((await $$('.mail-row.unread')).length).toBeGreaterThanOrEqual(1)
    expect((await $$('.mr-clip')).length).toBeGreaterThanOrEqual(1)
    await shot('收件箱-未读与附件标记')
  })
})
