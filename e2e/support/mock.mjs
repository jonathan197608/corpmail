import fs from 'node:fs'
import path from 'node:path'
import readline from 'node:readline'
import { spawn } from 'node:child_process'

let proc = null

/**
 * 启动 mock 邮件服务器（IMAP + SMTP，明文，随机端口）。
 * 端口写回 process.env —— wdio worker 继承主进程环境，spec 里直接读。
 */
export async function startMock(runDir) {
  const script = path.resolve('support/mockmail.py')
  const outbox = path.join(runDir, 'outbox')
  fs.mkdirSync(outbox, { recursive: true })

  proc = spawn(process.env.PYTHON || 'python3', [script, '--outbox', outbox], {
    stdio: ['ignore', 'pipe', 'inherit'],
  })

  const info = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('mock 邮件服务器启动超时（15s）')), 15000)
    const rl = readline.createInterface({ input: proc.stdout })
    rl.on('line', (line) => {
      if (line.trim().startsWith('{')) {
        clearTimeout(timer)
        resolve(JSON.parse(line))
      }
    })
    proc.on('exit', (code) => {
      clearTimeout(timer)
      reject(new Error(`mock 邮件服务器退出（code=${code}）`))
    })
  })

  process.env.E2E_IMAP_PORT = String(info.imap)
  process.env.E2E_SMTP_PORT = String(info.smtp)
  process.env.E2E_MAIL_COUNT = String(info.mails)
  process.env.E2E_OUTBOX = outbox
  return info
}

export function stopMock() {
  if (!proc) return
  proc.kill('SIGTERM')
  proc = null
}

/** 读发出邮件（SMTP 落盘）用于断言「真的发出去了」 */
export function sentMails() {
  const dir = process.env.E2E_OUTBOX
  if (!dir || !fs.existsSync(dir)) return []
  return fs
    .readdirSync(dir)
    .filter((f) => f.endsWith('.eml'))
    .sort()
    .map((f) => ({ name: f, raw: fs.readFileSync(path.join(dir, f), 'utf8') }))
}
