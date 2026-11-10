import fs from 'node:fs'
import path from 'node:path'
import { browser, $, expect } from '@wdio/globals'
import { shot, waitBoot, waitText, selectValue, goto, runDir, htmxAjax } from '../../support/helpers.mjs'

const IMAP_PORT = process.env.E2E_IMAP_PORT
const SMTP_PORT = process.env.E2E_SMTP_PORT

function reuseMarker() {
  const p = path.join(runDir(), 'reuse-ready.json')
  const m = JSON.parse(fs.readFileSync(p, 'utf8'))
  if (!m.dataDir || !fs.existsSync(path.join(m.dataDir, 'corpmail.db'))) {
    throw new Error(`reuse-data 不完整：${p}`)
  }
  return m
}

/** 填向导表单（W1/W2 共用；仅账号前缀不同） */
async function fillWizard(accountPrefix, dataDir) {
  await $('a.btn.primary.big').click()
  await browser.waitUntil(
    async () => (await browser.getUrl()).includes('/welcome/setup'),
    { timeout: 15000, timeoutMsg: '未进入向导页' },
  )
  await waitBoot()
  await $('input[name="account_prefix"]').setValue(accountPrefix)
  await $('input[name="account_domain"]').setValue('corpmail.test')
  await $('input[name="password"]').setValue('e2e-password')
  await $('input[name="display_name"]').setValue('重装复用测试')
  await $('input[name="smtp_host"]').setValue('127.0.0.1')
  await $('input[name="smtp_port"]').setValue(SMTP_PORT)
  await selectValue('select[name="smtp_security"]', 'plain')
  await $('input[name="recv_host"]').setValue('127.0.0.1')
  await $('input[name="recv_port"]').setValue(IMAP_PORT)
  await selectValue('select[name="recv_security"]', 'plain')
  // 指向既有数据目录（预填的是锚点默认目录，setValue 整体替换）
  await $('input[name="data_dir"]').setValue(dataDir)
}

/** 提交向导（W2 用）：W1 被拒后表单还留在页面上，此时 WebKit 内嵌驱动对
 *  同一表单的二次提交全部不可靠：elementClick 静默无效；原生 el.click() 实测
 *  click/submit 事件都派发但 htmx 不接手；requestSubmit 也不触发 hx-post。
 *  （真实用户点按钮不受影响——该问题属合成事件/驱动范围，非应用 bug。）
 *  改走 htmxAjax（fetch 直发 + 读 HX-Redirect 头），与 90 失败链路 F2/F4
 *  同一模式：服务器侧走完全相同的 handler，成功以 HX-Redirect 判定 */
async function submitSetupFormDirect() {
  const values = await browser.execute(() =>
    Object.fromEntries(new FormData(document.querySelector('.setup-card')).entries()),
  )
  const res = await htmxAjax('POST', '/welcome/setup/save', values, '#test-results')
  if (/不一致|拒用|未保存|设置失败|密码保存失败/.test(String(res.text))) {
    throw new Error(`向导提交被拒：${String(res.text).slice(0, 160)}`)
  }
  return res
}

describe('08 重装后向导沿用既有数据目录', () => {
  it('启动回到欢迎页（配置丢失），既有数据目录完好', async () => {
    await waitBoot()
    await expect($('h1')).toHaveText(expect.stringContaining('欢迎使用 CorpMail'))
    const marker = reuseMarker()
    expect(fs.existsSync(path.join(marker.dataDir, 'corpmail.db'))).toBe(true)
    await shot('复用-配置丢失回到欢迎页')
  })

  it('W1 异账号拒绝：登录账号 ≠ 库归属 → 不保存、不跳转', async () => {
    const marker = reuseMarker()
    await fillWizard('e2e-other', marker.dataDir)
    await shot('复用-向导指向既有目录（异账号）')

    // 首次提交走真实按钮（新页面上 elementClick 可靠），拒绝片段回到
    // #test-results：双通道真连 mock 都通过后，归属判定拒绝
    await $('button[type="submit"]').click()
    await browser.waitUntil(
      async () => (await $('#test-results').getText()).includes('不一致'),
      { timeout: 60000, timeoutMsg: '异账号未在 60s 内返回拒绝提示' },
    )
    const err = await $('#test-results').getText()
    expect(err).toContain('已拒绝使用')
    expect(err).toContain('e2e@corpmail.test') // 拒绝文案指明库内归属账号
    // 未跳转：仍停在向导（无 HX-Redirect）
    expect(await browser.getUrl()).toContain('/welcome/setup')
    await shot('复用-异账号被拒绝')
  })

  it('W2 同账号采纳：历史数据直接沿用 + 完成页提示增量续收', async () => {
    // W1 失败停在向导页，只改账号前缀；提交走 fetch 直发（见
    // submitSetupFormDirect 注释——换片段后的二次提交是驱动不可靠区）
    await $('input[name="account_prefix"]').setValue('e2e')
    const res = await submitSetupFormDirect()
    expect(res.redirect).toBe('/welcome/done')
    await goto('/welcome/done')
    const done = await $('body').getText()
    expect(done).toContain('已直接沿用')
    expect(done).toMatch(/已有 \d+ 封历史邮件/)
    expect(done).toContain('增量拉取') // 收件从既有进度续拉，不重新全量收取
    expect(done).toContain('数据目录已设置为')
    expect(done).toContain('立即生效') // 采纳是会话内换库，无需重启
    await shot('复用-完成页沿用提示')

    // 主界面：历史邮件恢复。「欢迎使用」在主链路 04 已删除，本阶段不出现
    // 说明数据是重装前的最终状态快照——删除操作/草稿/发件等全部连续
    await $('a.btn.primary.big').click()
    await browser.waitUntil(
      async () =>
        (await browser
          .getUrl())
          .match(/^ui:\/\/localhost\/?$|^http:\/\/ui\.localhost\/?$/) !== null,
      { timeout: 20000, timeoutMsg: '未进入主界面' },
    )
    await waitBoot()
    await waitText('.mail-list', '会议纪要')
    const listText = await $('.mail-list').getText()
    expect(listText).toContain('季度报表')
    expect(listText).not.toContain('欢迎使用') // 04 的删除在重装后仍然生效
    await shot('复用-历史邮件恢复')

    // 草稿/发件（05/90 主链路产物）：同样来自历史库
    await goto('/?folder=draft')
    await waitText('.mail-list', 'E2E 自动保存草稿')
    await goto('/?folder=sent')
    await waitText('.mail-list', 'E2E 发送链路验证')
    await shot('复用-草稿与发件保留')

    // 模板：下拉反映主链路「最终状态」——05 D1 新增的还在；06 F5c 删除的
    // 没有复活（采纳的是最终状态而非历史快照，与「欢迎使用」缺席同理）
    await goto('/compose')
    const opts = await browser.execute(() =>
      [...document.querySelectorAll('#tpl-select option')].map((o) => o.textContent.trim()),
    )
    expect(opts).toContain('E2E 撰写页预置模板')
    expect(opts).not.toContain('E2E 模板') // 06 F5c 的删除在重装后仍然生效
    await shot('复用-模板最终状态')

    // 联系人（01 首拉自动采集）：重装后仍然在列
    await goto('/contacts')
    await waitText('h1', '联系人')
    const ctText = await $('body').getText()
    expect(ctText).toContain('zhangwei@corpmail.test')
    expect(ctText).toContain('wangqiang@corpmail.test')
    await shot('复用-联系人保留')
  })
})
