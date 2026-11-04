import { browser, $, $$, expect } from '@wdio/globals'
import { shot, waitBoot } from '../support/helpers.mjs'

// 00：未配置状态。run 目录是全新的 HOME，所以这里必然落在欢迎页。
describe('00 启动与欢迎页', () => {
  it('首帧渲染欢迎页，启动遮罩已移除', async () => {
    await waitBoot()
    await expect(browser).toHaveTitle('CorpMail')
    await expect($('h1')).toHaveText(expect.stringContaining('欢迎使用 CorpMail'))
    await shot('欢迎页')
  })

  it('三条产品说明与「开始配置」入口可见', async () => {
    await expect($$('.feature-row')).toBeElementsArrayOfSize(3)
    await expect($('a.btn.primary.big')).toHaveText(expect.stringContaining('开始配置'))
    await shot('欢迎页-功能说明')
  })

  it('侧栏在未配置时不显示收件状态（无假错误）', async () => {
    // 回归：启动 2s 首收曾在向导之前以空账号跑一轮，把「凭据缺失」写进状态条
    const status = await $('#fetch-status')
    const text = (await status.isExisting()) ? await status.getText() : ''
    expect(text).not.toContain('凭据缺失')
  })
})
