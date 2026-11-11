import { browser, expect } from '@wdio/globals'
import { goto, shot, waitText, htmxAjax, runDir } from '../support/helpers.mjs'
import path from 'node:path'
import fs from 'node:fs'

// 95：数据目录迁移 + 迁移冻结（审计 H2 / R2）
//
// 为什么必须排在主链路最末（文件名 95 > 90）：
// 迁移成功后数据快照已定格，本进程仍连着旧库——R2 起所有写入都被路由层
// 409 拒绝（否则用户迁移后的修改会静默写进即将被抛弃的旧库）。放在中间
// 会让 04-07 的发送/草稿/模板全部失败。
describe('95 数据目录迁移与冻结', () => {
  it('迁移前：更换位置可用，无冻结横幅', async () => {
    await goto('/settings/storage')
    const st = await browser.execute(() => {
      const pane = document.getElementById('settings-pane')
      const move = Array.from(pane.querySelectorAll('button')).find((b) =>
        (b.textContent || '').includes('更换位置'),
      )
      return {
        frozen: (pane.textContent || '').includes('重启应用后继续使用'),
        moveDisabled: move ? move.disabled : null,
      }
    })
    expect(st.frozen).toBe(false)
    expect(st.moveDisabled).toBe(false)
  })

  it('A6 迁移：待生效目录 + 重启徽标 + 冻结横幅与禁用写按钮', async () => {
    const dest = path.join(runDir(), 'moved-data')
    fs.mkdirSync(dest, { recursive: true })
    const res = await htmxAjax('POST', '/settings/storage/move', { dest }, '#settings-pane')
    expect(res.status).toBe(200)
    // 单测钉过的文案：徽标「重启后生效」+ 旧目录「当前使用」注记
    expect(res.text).toContain('重启后生效')
    await waitText('#settings-pane', '当前使用')
    expect(res.text).toContain('重启应用后继续使用')
    await shot('设置-数据目录待生效（已冻结）')

    const st = await browser.execute(() => {
      const pane = document.getElementById('settings-pane')
      const save = pane.querySelector('button[type="submit"]')
      const move = Array.from(pane.querySelectorAll('button')).find((b) =>
        (b.textContent || '').includes('更换位置'),
      )
      return { save: save ? save.disabled : null, move: move ? move.disabled : null }
    })
    expect(st.save).toBe(true)
    expect(st.move).toBe(true)
  })

  it('R2 冻结：写请求 409 + 明确提示，读页面照常', async () => {
    const probe = await browser.execute(async () => {
      const r = await fetch('/settings/sync/save', {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body: 'sync_interval_min=9',
      })
      const text = await r.text()
      const read = await fetch('/settings/storage')
      return { status: r.status, text: text.slice(0, 120), readStatus: read.status }
    })
    expect(probe.status).toBe(409)
    expect(probe.text).toContain('已迁移')
    expect(probe.readStatus).toBe(200)
    await shot('设置-冻结后写入被拒')
  })

  it('冻结期收件入口也被拒（不写旧库）', async () => {
    const probe = await browser.execute(async () => {
      const r = await fetch('/emails/fetch')
      return { status: r.status, text: (await r.text()).slice(0, 120) }
    })
    expect(probe.status).toBe(409)
    expect(probe.text).toContain('已迁移')
  })
})
