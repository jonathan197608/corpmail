import path from 'node:path'
import fs from 'node:fs'

// ---- 运行目录 ------------------------------------------------------------
// 每次运行独立产物目录：截图 / 日志 / 报告集中存放，便于按 run 归档
const RUN_ID = process.env.E2E_RUN_ID || new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19)
export const RUN_DIR = path.resolve('artifacts', RUN_ID)
fs.mkdirSync(RUN_DIR, { recursive: true })
// 回传给后续 spawn 的 worker 进程：主进程与 worker 共用同一 RUN_DIR
// （否则 worker 会新建第二个时间戳目录，截图与 home/mock 产物分家）
process.env.E2E_RUN_ID = RUN_ID
process.env.E2E_RUN_DIR = RUN_DIR

// ---- 被测二进制 ----------------------------------------------------------
// 必须是 `cargo build --features e2e` 的产物：只有该构建注册了内嵌 WebDriver
// 服务端。macOS 上没有 WKWebView 的外部 driver，embedded provider 是唯一路径。
const APP = path.resolve('../src-tauri/target/debug/corpmail')
if (!fs.existsSync(APP)) {
  throw new Error(
    `找不到被测二进制 ${APP}\n先构建：cd src-tauri && cargo build --features e2e`,
  )
}

// ---- 沙箱数据目录 --------------------------------------------------------
// 每个 run 一套独立 HOME：app_data_dir / corpmail.toml / keyfile / 隔离 keychain
// 全部落在 run 目录内，互不干扰，也绝不触碰真实用户数据。
// A3 第三阶段例外：CORPMAIL_E2E_HOME 让第三阶段复用阶段二跑完向导后的
// 「已配置 + 有库」HOME，产物目录仍是独立的 <RUN_ID>-bg。若换新 HOME，app
// 只会回到欢迎页，根本进不了设置页，后台迁移无从发起。
const HOME = process.env.CORPMAIL_E2E_HOME
  ? path.resolve(process.env.CORPMAIL_E2E_HOME)
  : path.join(RUN_DIR, 'home')
fs.mkdirSync(path.join(HOME, 'Library', 'Application Support'), { recursive: true })
process.env.HOME = HOME

export const config = {
  runner: 'local',
  // 单层 glob：specs/*.spec.mjs = 主链路（00-07 + 90 + 95）；
  // specs/phase2/ 与 specs/phase3/ 分别由 run.sh 第二、三阶段显式 --spec
  // 调起（各自需要独立前置状态 / 独立 app 进程与运行时开关）
  specs: ['./specs/*.spec.mjs'],
  maxInstances: 1,
  logLevel: 'info',
  waitforTimeout: 15000,
  connectionRetryTimeout: 120000,
  connectionRetryCount: 2,

  services: [
    [
      '@wdio/tauri-service',
      {
        appBinaryPath: APP,
        driverProvider: 'embedded',
        embeddedPort: Number(process.env.E2E_WD_PORT || 4445),
        startTimeout: 90000,
        // 后端 stderr 进报告：收件/发送失败原因一眼可见（GUI 应用无控制台）
        captureBackendLogs: true,
        backendLogLevel: 'debug',
      },
    ],
  ],

  capabilities: [
    {
      browserName: 'tauri',
      'tauri:options': { application: APP },
    },
  ],

  framework: 'mocha',
  reporters: ['spec'],
  // 首败即停：bail=1 第一个 spec 失败就中止整套；mochaOpts.bail 同一文件内
  // 第一个 it 失败即停。后续 spec 依赖前序（配置→首拉→列表→撰写），级联
  // 失败只会刷一屏噪音 + 烧时间（曾出现向导失败后 5 个 spec 全在欢迎页空轮询）
  bail: 1,
  mochaOpts: { ui: 'bdd', timeout: 120000, bail: true },

  // 每个 worker 独立会话：进入后静默焦点管理（否则每条 $/查询白等 5s 超时）
  before: async function () {
    const { suppressFocusManager } = await import('./support/helpers.mjs')
    await suppressFocusManager()
  },

  // 每个用例失败时自动补一张整页截图（不替代用例内主动截图，只作失败现场）
  afterTest: async function (test, _context, { error }) {
    if (!error) return
    const safe = test.title.replace(/[^\w\u4e00-\u9fa5-]+/g, '_').slice(0, 60)
    try {
      await browser.saveScreenshot(path.join(RUN_DIR, `FAIL-${safe}.png`))
    } catch (shotErr) {
      // 截图失败不能掩盖原失败（审计 R85）：旧实现 `catch {}` 静默吞掉，
      // 报告里既没有截图也没有「为什么没有截图」。这里打印原因，并尽力把
      // worker 进程退出码置 1（最终退出码由 wdio 汇总，主要靠日志留痕）
      console.error(`[E2E] 失败截图保存失败（${test.title}）：${shotErr?.message || shotErr}`)
      process.exitCode = 1
    }
  },

  // worker 进程内：每个 spec 结束后把截图清单落成 index.md（跨 spec 合并）
  afterSession: async function () {
    const { writeShotIndex } = await import('./support/helpers.mjs')
    writeShotIndex()
  },

  // 主进程：起 mock 邮件服务器（端口通过 env 传给 worker），结束时收尾
  onPrepare: async function () {
    const { startMock } = await import('./support/mock.mjs')
    const info = await startMock(RUN_DIR)
    console.log(
      `[E2E] mock 邮件服务器 imap=${info.imap} smtp=${info.smtp}，初始邮件 ${info.mails} 封`,
    )
  },

  onComplete: function () {
    import('./support/mock.mjs').then(({ stopMock }) => stopMock())
    console.log(`\n[E2E] 截图与日志：${RUN_DIR}\n`)
  },
}
