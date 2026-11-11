import { browser, $, $$, expect } from '@wdio/globals'
import { shot, waitBoot, waitText, selectValue, htmxAjax } from '../support/helpers.mjs'

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
    // 字段说明已改气泡触发点（2026-11-10）：登录账号/数据保存目录两处
    const tips = await browser.execute(() => document.querySelectorAll('.tip-i').length)
    expect(tips).toBeGreaterThanOrEqual(2)
    // 默认集团域名：两条 OA 指引在场（服务端按默认域名渲染）
    const guides0 = await browser.execute(() =>
      [...document.querySelectorAll('.setup-card .ct-guide')].map((g) => !g.hidden),
    )
    expect(guides0.filter(Boolean).length).toBe(2)
    // 两条黄条置顶（表单第一个元素，标题之前），仅首设向导展示
    const alertsTop = await browser.execute(() => {
      const form = document.querySelector('form.setup-card')
      return form?.firstElementChild?.classList.contains('setup-alerts') === true
    })
    expect(alertsTop).toBe(true)
    // 首设：数据保存目录默认为空，页面提示用户自行选择
    await expect($('input[name="data_dir"]')).toHaveValue('')
    const required = await browser.execute(
      () => document.querySelector('input[name="data_dir"]')?.required === true,
    )
    expect(required).toBe(true)
    const hint = await browser.execute(() =>
      document.querySelector('.dir-hint')?.textContent?.includes('请先选择数据保存目录'),
    )
    expect(hint).toBe(true)
    // 页面不再展示默认目录路径（默认位置只保留在隐藏属性 data-default-dir）
    const hintText = await browser.execute(() =>
      document.querySelector('.dir-hint')?.textContent || '',
    )
    expect(hintText).not.toContain('默认位置')
    await shot('向导-默认配置')
  })

  it('必填校验：账号/密码为空时提交不落库', async () => {
    await $('button[type="submit"]').click()
    // 浏览器的 required 校验直接拦下，页面仍停在向导
    expect(await browser.getUrl()).toContain('/welcome/setup')
    await shot('向导-必填校验')
  })

  it('数据目录未选择：后端兜底拦截，不进入完成页', async () => {
    await $('input[name="account_prefix"]').setValue('e2e')
    await $('input[name="password"]').setValue('e2e-password')
    // 绕过 required 直接 POST（模拟脚本提交），后端必须拒
    const values = await browser.execute(() =>
      Object.fromEntries(new FormData(document.querySelector('.setup-card')).entries()),
    )
    values.data_dir = ''
    // 直发但不 swap（htmxAjax 的 outerHTML 会替换整个 #test-results 容器，
    // 残留文案会污染下一条用例的 body 全文快速失败检测）
    const res = await htmxAjax('POST', '/welcome/setup/save', values, '#no-swap-probe')
    expect(res.text).toContain('请先选择数据保存目录')
    expect(res.redirect).toBeNull()
    expect(await browser.getUrl()).toContain('/welcome/setup')
    await shot('向导-未选目录被拦截')
  })

  it('指向本机 mock 服务器后「测试并保存」通过', async () => {
    await $('input[name="account_prefix"]').setValue('e2e')
    await $('input[name="account_domain"]').setValue('corpmail.test')
    // 改掉集团域名后 OA 指引应实时隐藏（app.js 监听输入，非集团域名不适用 OA 路径）
    await browser.waitUntil(
      async () => {
        const hidden = await browser.execute(
          () => ![...document.querySelectorAll('.setup-card .ct-guide')].some((g) => !g.hidden),
        )
        return hidden
      },
      { timeout: 5000, timeoutMsg: '改自定义域名后 OA 指引未隐藏' },
    )
    await $('input[name="password"]').setValue('e2e-password')
    await $('input[name="display_name"]').setValue('自动化测试')

    await $('input[name="smtp_host"]').setValue('127.0.0.1')
    await $('input[name="smtp_port"]').setValue(SMTP_PORT)
    await selectValue('select[name="smtp_security"]', 'plain')
    await $('input[name="recv_host"]').setValue('127.0.0.1')
    await $('input[name="recv_port"]').setValue(IMAP_PORT)
    await selectValue('select[name="recv_security"]', 'plain')
    // 首设必须显式选择数据保存目录：模拟用户点「选择…」选了默认位置
    //（data-default-dir 是隐藏属性，仅自动化使用；页面不再展示默认目录，
    //  填入后与旧版预填行为一致）
    const defaultDir = await $('input[name="data_dir"]').getAttribute('data-default-dir')
    expect(defaultDir).toBeTruthy()
    await $('input[name="data_dir"]').setValue(defaultDir)
    await shot('向导-已填写本地服务器')

    await $('button[type="submit"]').click()
    // 提交后要么整页跳转 done，要么表单区出现错误片段——出现错误立即快速失败，
    // 把错误文本带进超时信息（否则只能干等 60s 后对着 getUrl 轮询日志猜）
    await browser.waitUntil(
      async () => {
        if ((await browser.getUrl()).includes('/welcome/done')) return true
        const text = await $('body').getText()
        if (/未保存|设置失败|保存失败|密码保存失败|请先选择数据保存目录/.test(text)) {
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
