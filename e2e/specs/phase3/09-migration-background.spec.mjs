import { browser, expect } from '@wdio/globals'
import { goto, shot, runDir, waitBoot } from '../../support/helpers.mjs'
import fs from 'node:fs'
import path from 'node:path'

// 09：A3 生产后台迁移链的端到端证据（run.sh 第三阶段，独立 wdio 实例）。
//
// 只在第三阶段跑，由 run.sh 前缀传入两个开关：
//   CORPMAIL_E2E_BACKGROUND=1  e2e 构建也注入后台 executor（与生产同构）
//   CORPMAIL_E2E_NO_EXIT=1     成功分支不调 app.exit(0)，留进程供断言
// 主链路 95/90 不设这两个变量，仍走同步分支，一字未改。
//
// 前置状态：CORPMAIL_E2E_HOME 复用阶段二 W2 跑完向导后的 HOME（已配置，
// 数据目录 = reuse-data）；产物与截图另放 <RUN_ID>-bg。全新 HOME 只会回到
// 欢迎页，进不了设置页，所以这里必须复用而不是隔离。
const DEST = path.join(runDir(), 'bg-moved-data')
const SOURCE_DIR = process.env.CORPMAIL_E2E_SOURCE_DATA_DIR || ''

describe('09 生产后台迁移链（后台线程 + 事件 + 落盘）', () => {
  it('前置：阶段二终态已配置，存储页可发起迁移', async () => {
    expect(SOURCE_DIR).not.toBe('')
    expect(fs.existsSync(path.join(SOURCE_DIR, 'corpmail.db'))).toBe(true)
    await waitBoot()
    await goto('/settings/storage')
    // 不能断 URL：ui:// 自定义协议下 WKWebView 把导航 URL 归位成
    // ui://localhost/（主链路 95 也只断 DOM）。且第三阶段的 app 是冷启动，
    // goto 返回时首帧可能还没渲染——这里等 pane 出现再断言，避免拿到空 DOM。
    await browser.waitUntil(
      async () => browser.execute(() => !!document.getElementById('settings-pane')),
      { timeout: 20000, timeoutMsg: '设置存储页 20s 内未渲染出 #settings-pane' },
    )
    const st = await browser.execute(() => {
      const pane = document.getElementById('settings-pane')
      const move = Array.from(pane?.querySelectorAll('button') || []).find((b) =>
        (b.textContent || '').includes('更换位置'),
      )
      return {
        hasPane: !!pane,
        moveDisabled: move ? move.disabled : null,
        frozen: (pane?.textContent || '').includes('重启应用后继续使用'),
      }
    })
    expect(st.hasPane).toBe(true)
    expect(st.moveDisabled).toBe(false)
    expect(st.frozen).toBe(false)
  })

  it('A3：background 响应头 + starting/后续 progress + done 终态 + 落盘 + 冻结 409', async () => {
    fs.mkdirSync(DEST, { recursive: true })

    // 监听必须在 POST 之前注册；用原生 fetch 才能读自定义响应头
    // （helpers.htmxAjax 只回 HX-Redirect，读不到 X-CorpMail-Migration）。
    const res = await browser.execute(async (dest) => {
      window.__BG_EVENTS__ = []
      const names = ['migration-progress', 'migration-done', 'migration-error']
      await Promise.all(
        names.map((name) =>
          window.__TAURI__.event.listen(name, (e) => {
            window.__BG_EVENTS__.push({ name, payload: e.payload })
          }),
        ),
      )
      const r = await fetch('/settings/storage/move', {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body: `dest=${encodeURIComponent(dest)}`,
      })
      return {
        status: r.status,
        header: r.headers.get('X-CorpMail-Migration'),
        text: (await r.text()).slice(0, 200),
      }
    }, DEST)

    expect(res.status).toBe(200)
    expect(res.header).toBe('background')
    expect(res.text).toContain('数据迁移正在进行中')

    // 后台线程在另一线程跑：等终态事件，最长 90s（小库实际几秒）。
    // 超时后把已收到的事件序列带进错误信息，否则只看到 waitUntil 超时。
    try {
      await browser.waitUntil(
        async () =>
          browser.execute(() =>
            (window.__BG_EVENTS__ || []).some((e) => e.name !== 'migration-progress'),
          ),
        { timeout: 90000, interval: 200, timeoutMsg: '后台迁移终态事件 90s 内未到达' },
      )
    } catch (e) {
      const seen = await browser.execute(() =>
        (window.__BG_EVENTS__ || [])
          .map((x) => `${x.name}:${JSON.stringify(x.payload)}`)
          .join(' | '),
      )
      throw new Error(`${e.message}；已收到：${seen || '(空)'}`)
    }

    const events = await browser.execute(() => window.__BG_EVENTS__)
    expect(events.some((e) => e.name === 'migration-error')).toBe(false)
    expect(events.some((e) => e.name === 'migration-done')).toBe(true)
    const progress = events.filter((e) => e.name === 'migration-progress')
    expect(progress[0]?.payload?.phase).toBe('starting')
    expect(progress.length).toBeGreaterThan(1)
    expect(progress.slice(1).some((e) => e.payload?.phase && e.payload.phase !== 'starting')).toBe(
      true,
    )

    // 快照落在新目录，源库保留作备份（迁移不删旧数据）。
    expect(fs.existsSync(path.join(DEST, 'corpmail.db'))).toBe(true)
    expect(fs.existsSync(path.join(SOURCE_DIR, 'corpmail.db'))).toBe(true)

    // 冻结位已置：写端点 409，读页面仍 200。
    const probe = await browser.execute(async () => {
      const w = await fetch('/settings/sync/save', {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body: 'sync_interval_min=9',
      })
      const r = await fetch('/settings/storage')
      return { write: w.status, read: r.status }
    })
    expect(probe.write).toBe(409)
    expect(probe.read).toBe(200)
    await shot('后台迁移-完成与冻结')
  })
})
