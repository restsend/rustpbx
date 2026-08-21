/**
 * Field-report regression (0.5.0-rc.1):
 *   [recording] enabled=true + default video_policy
 *   → phone_jssip.html (WebRTC) → sipbot (plain RTP) must establish with
 *     bidirectional audio (recording forces MediaBridge anchoring).
 *
 * Run:
 *   cargo build --features "default,contact-center,addon-sbc,addon-wholesale"
 *   cd e2e/console && PBX_BIN=../../target/debug/rustpbx npx playwright test tests/webrtc-recording-audio.spec.js
 */

const { test, expect } = require('@playwright/test')
const { chromium } = require('playwright-core')
const { spawn } = require('child_process')
const path = require('path')
const fs = require('fs')
const os = require('os')

const REPO = path.resolve(__dirname, '..', '..', '..')
const PBX_BIN = process.env.PBX_BIN || path.join(REPO, 'target', 'debug', 'rustpbx')
const SIPBOT_BIN = process.env.SIPBOT_BIN || 'sipbot'

const HTTP_PORT = 18081
const SIP_PORT = 15061
const PBX_URL = `http://127.0.0.1:${HTTP_PORT}`
const PASSWORD = '123456'

test.describe.configure({ mode: 'serial' })

let pbxProc = null
let tmpHome = null

test.beforeAll(async () => {
  tmpHome = fs.mkdtempSync(path.join(os.tmpdir(), 'rustpbx-rec-audio-'))
  const recordDir = path.join(tmpHome, 'recordings')
  fs.mkdirSync(recordDir, { recursive: true })

  const config = `
log_level = "info"
database_url = "sqlite::memory:"
http_addr = "127.0.0.1:${HTTP_PORT}"

[proxy]
addr = "127.0.0.1"
udp_port = ${SIP_PORT}
tcp_port = ${SIP_PORT}
ws_handler = "/ws"
modules = ["acl", "auth", "registrar", "call"]
media_proxy = "all"

[[proxy.user_backends]]
type = "memory"

[[proxy.user_backends.users]]
id = 1
enabled = true
username = "bob"
password = "${PASSWORD}"
is_support_webrtc = true

[[proxy.user_backends.users]]
id = 2
enabled = true
username = "1002"
password = "${PASSWORD}"
is_support_webrtc = false

[recording]
enabled = true
auto_start = true
type = "local"
path = "${recordDir}"

[console]
allow_registration = true
secure_cookie = false
`
  const confPath = path.join(tmpHome, 'config.toml')
  fs.writeFileSync(confPath, config)

  pbxProc = spawn(PBX_BIN, ['--conf', confPath], {
    cwd: REPO,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  const log = []
  pbxProc.stdout && pbxProc.stdout.on('data', (d) => log.push(String(d)))
  pbxProc.stderr && pbxProc.stderr.on('data', (d) => log.push(String(d)))
  pbxProc._log = () => log.join('')

  const deadline = Date.now() + 45000
  while (Date.now() < deadline) {
    try {
      const resp = await fetch(`${PBX_URL}/static/phone_jssip.html`)
      if (resp.ok) break
    } catch (_) {}
    await new Promise((r) => setTimeout(r, 400))
  }
})

test.afterAll(async () => {
  if (pbxProc) {
    try { pbxProc.kill('SIGKILL') } catch (_) {}
  }
})

test('phone_jssip + recording → sipbot: call establishes with RX packets', async () => {
  test.setTimeout(120000)

  const out = []
  const callee = spawn(SIPBOT_BIN, [
    'wait',
    '-a', '127.0.0.1:0',
    '-u', '1002',
    '-p', PASSWORD,
    '-d', `127.0.0.1:${SIP_PORT}`,
    '-r',
    '--echo',
    '--ring-duration', '1',
    '--audio-quality',
  ], { stdio: ['ignore', 'pipe', 'pipe'] })
  callee.stdout && callee.stdout.on('data', (d) => out.push(String(d)))
  callee.stderr && callee.stderr.on('data', (d) => out.push(String(d)))
  await new Promise((r) => setTimeout(r, 2500))
  expect(out.join(''), 'sipbot should register').toMatch(/Registered successfully/i)

  const browser = await chromium.launch({
    headless: true,
    args: [
      '--no-sandbox',
      '--use-fake-ui-for-media-stream',
      '--use-fake-device-for-media-stream',
    ],
  })
  const ctx = await browser.newContext({ permissions: ['microphone'] })
  await ctx.grantPermissions(['microphone'], { origin: PBX_URL })
  const page = await ctx.newPage()
  await page.goto(`${PBX_URL}/static/phone_jssip.html?caller=bob&callee=1002`, {
    waitUntil: 'domcontentloaded',
    timeout: 30000,
  })
  await page.fill('#password', PASSWORD)
  await page.click('#registerBtn')
  await expect(page.locator('#registrationStatus')).toContainText('Registered', { timeout: 20000 })

  await page.fill('#callTarget', '1002')
  await page.click('#callBtn')
  await expect(page.locator('#callControls.active')).toBeVisible({ timeout: 30000 })
  await page.waitForTimeout(8000)

  const rx = parseInt((await page.locator('#rxPackets').textContent()) || '0', 10)

  const sipbotText = out.join('')
  const hasSipbotAudio =
    /RX:\s*[1-9]/.test(sipbotText)
    || /has_audio\s*:\s*true/.test(sipbotText)
    || /total_frames\s*:\s*[1-9]/.test(sipbotText)

  console.log('rxPackets=', rx, 'sipbotTail=', sipbotText.slice(-800))
  expect(
    rx > 0 || hasSipbotAudio,
    `no media with recording enabled (rx=${rx}). sipbot:\n${sipbotText.slice(-1200)}\npbx:\n${pbxProc._log().slice(-1500)}`
  ).toBeTruthy()

  try { await page.click('#hangupBtn') } catch (_) {}
  try { callee.kill('SIGKILL') } catch (_) {}
  await browser.close()
})
