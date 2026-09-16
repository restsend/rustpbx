/**
 * REPRO: WebRTC (real Chrome/JsSIP over WS) → RTP (sipbot) fastpath call with
 * stereo auto-recording. Field issue (./fastpath capture): the recorded WAV's
 * caller leg (left channel = audio FROM the browser) is a constant 0xFF
 * silence byte for the whole call while live audio flowed fine both ways.
 *
 * This spec boots a dedicated rustpbx with recording auto_start, drives the
 * real phone_jssip.html page with Chromium's fake-mic tone device, calls a
 * sipbot echo callee, then parses the produced WAV and reports the per-leg
 * silence ratio + RMS.
 *
 * Run: cd e2e/console && npx playwright test tests/webrtc-recording-repro.spec.js
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

const HTTP_PORT = 18080
const SIP_PORT = 15060
const PBX_URL = `http://127.0.0.1:${HTTP_PORT}`
const PASSWORD = '123456'

// ── WAV helpers (G.711 μ-law stereo as written by the proxy recorder) ──────

function parseG711StereoWav(file) {
  const b = fs.readFileSync(file)
  if (b.subarray(0, 4).toString() !== 'RIFF') throw new Error('not RIFF')
  let pos = 12
  let fmtTag = 0
  let channels = 0
  let sampleRate = 0
  let payload = null
  while (pos + 8 <= b.length) {
    const id = b.subarray(pos, pos + 4).toString()
    const size = b.readUInt32LE(pos + 4)
    const body = pos + 8
    if (body + size > b.length) break
    if (id === 'fmt ') {
      fmtTag = b.readUInt16LE(body)
      channels = b.readUInt16LE(body + 2)
      sampleRate = b.readUInt32LE(body + 4)
    } else if (id === 'data') {
      payload = b.subarray(body, body + size)
    }
    pos = body + size + (size & 1)
  }
  if (!payload) throw new Error('no data chunk')
  return { fmtTag, channels, sampleRate, payload }
}

function ulaw2pcm(u) {
  u = ~u & 0xff
  const sign = u & 0x80
  const exp = (u >> 4) & 0x07
  const man = u & 0x0f
  let pcm = ((man << 1) | 1) << exp
  pcm -= 33 << 5
  return sign ? -pcm : pcm
}

function analyzeLeg(bytes) {
  let silence = 0
  let sumSq = 0
  const pcm = []
  for (const byte of bytes) {
    if (byte === 0xff) silence++
    const s = ulaw2pcm(byte)
    sumSq += s * s
    pcm.push(s)
  }
  const rms = Math.sqrt(sumSq / Math.max(1, bytes.length))
  return { silenceRatio: silence / Math.max(1, bytes.length), rms }
}

// ── the spec ───────────────────────────────────────────────────────────────

/** Generate a short tone WAV (8 kHz mono 16-bit PCM). */
function genToneWav(file, freq = 440, secs = 3) {
  const sampleRate = 8000
  const n = Math.floor(sampleRate * secs)
  const dataSize = n * 2
  const b = Buffer.alloc(44 + dataSize)
  b.write('RIFF', 0)
  b.writeUInt32LE(36 + dataSize, 4)
  b.write('WAVE', 8)
  b.write('fmt ', 12)
  b.writeUInt32LE(16, 16)
  b.writeUInt16LE(1, 20)
  b.writeUInt16LE(1, 22)
  b.writeUInt32LE(sampleRate, 24)
  b.writeUInt32LE(sampleRate * 2, 28)
  b.writeUInt16LE(2, 32)
  b.writeUInt16LE(16, 34)
  b.write('data', 36)
  b.writeUInt32LE(dataSize, 40)
  for (let i = 0; i < n; i++) {
    b.writeInt16LE(Math.round(12000 * Math.sin((2 * Math.PI * freq * i) / sampleRate)), 44 + i * 2)
  }
  fs.writeFileSync(file, b)
}

test.describe.configure({ mode: 'serial' })

let pbxProc = null
let recordDir = null
let tmpHome = null

test.beforeAll(async () => {
  tmpHome = fs.mkdtempSync(path.join(os.tmpdir(), 'rustpbx-repro-'))
  recordDir = path.join(tmpHome, 'recordings')
  fs.mkdirSync(recordDir, { recursive: true })

  const config = `
log_level = "debug"
database_url = "sqlite::memory:"
http_addr = "127.0.0.1:${HTTP_PORT}"

[proxy]
addr = "127.0.0.1"
udp_port = ${SIP_PORT}
tcp_port = ${SIP_PORT}
ws_handler = "/ws"
modules = ["acl", "auth", "registrar", "call"]

[[proxy.user_backends]]
type = "memory"

[[proxy.user_backends.users]]
id = 1
enabled = true
username = "bob"
password = "${PASSWORD}"
allow_guest_calls = false
is_support_webrtc = true

[[proxy.user_backends.users]]
id = 2
enabled = true
username = "1002"
password = "${PASSWORD}"
allow_guest_calls = false

[recording]
enabled = true
auto_start = true
type = "local"
format = "wav"

[sipflow]
type = "local"
root = "${path.join(tmpHome, 'sipflow')}"
subdirs = "daily"
flush_count = 1000
flush_interval_secs = 5

[console]
allow_registration = true
secure_cookie = false

[[console.api_tokens]]
token = "repro-api-token-123"
scopes = ["call", "session", "media", "record"]
`
  const confPath = path.join(tmpHome, 'config.toml')
  fs.writeFileSync(confPath, config)

  pbxProc = spawn(PBX_BIN, ['--conf', confPath], {
    cwd: REPO, // static/ + config/ are resolved from the working directory
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  const log = []
  pbxProc.stdout && pbxProc.stdout.on('data', (d) => log.push(String(d)))
  pbxProc.stderr && pbxProc.stderr.on('data', (d) => log.push(String(d)))
  pbxProc._log = () => log.join('')

  // Wait for HTTP to come up.
  const deadline = Date.now() + 30000
  while (Date.now() < deadline) {
    try {
      const resp = await fetch(`${PBX_URL}/api/notifications/unread-count`)
      if (resp.ok || resp.status === 401) break
    } catch (_) {}
    await new Promise((r) => setTimeout(r, 500))
  }
  console.log('rustpbx up on', PBX_URL)
})

test.afterAll(async () => {
  if (pbxProc) {
    try { pbxProc.kill('SIGKILL') } catch (_) {}
  }
  // keep tmpHome for inspection; print the path
  console.log('repro artifacts at:', tmpHome)
})

test('browser(WebRTC) → sipbot(RTP) recording: caller leg must have real audio', async () => {
  test.setTimeout(180000)

  // sipbot callee: answer after 1s, echo mode.
  const out = []
  // 440 Hz ringback file: sipbot answers with 183 early media (SDP + tone)
  // and only answers for real after --ring-duration, mirroring the field
  // baresip flow (180 → 183 w/ SDP + ausine → 200 OK).
  const ringback = path.join(tmpHome, 'ringback440.wav')
  genToneWav(ringback, 440, 5)
  const callee = spawn(SIPBOT_BIN, [
    'wait',
    '-a', `127.0.0.1:0`,
    '-u', '1002',
    '-p', PASSWORD,
    '-d', `127.0.0.1:${SIP_PORT}`,
    '-r',
    '--ringback', ringback,
    '--ring-duration', '3',
    '--echo',
  ], { stdio: ['ignore', 'pipe', 'pipe'] })
  callee.stdout && callee.stdout.on('data', (d) => out.push(String(d)))
  callee.stderr && callee.stderr.on('data', (d) => out.push(String(d)))
  await new Promise((r) => setTimeout(r, 2500))
  console.log('sipbot startup:', out.join('').slice(0, 300))

  const exeCandidates = [
    path.join(os.homedir(), '.cache', 'ms-playwright', 'chromium-1223', 'chrome-linux64', 'chrome'),
    path.join(os.homedir(), '.cache', 'ms-playwright', 'chromium-1234', 'chrome-linux64', 'chrome'),
  ].find((c) => fs.existsSync(c))
  const ownBrowser = await chromium.launch({
    ...(exeCandidates ? { executablePath: exeCandidates } : {}),
    headless: true,
    args: [
      '--no-sandbox',
      '--use-fake-ui-for-media-stream',
      '--use-fake-device-for-media-stream',
    ],
  })
  const ctx = await ownBrowser.newContext({ permissions: ['microphone'] })
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
  console.log('call established, letting fake-mic tone + echo flow...')

  // Let audio flow ~12s (fake mic emits a tone by default).
  await new Promise((r) => setTimeout(r, 12000))

  // Verify live bidirectional audio from the browser's own stats.
  const rxPackets = parseInt((await page.locator('#rxPackets').textContent()) || '0', 10)
  console.log('browser inbound packets (live):', rxPackets)

  await page.click('#hangupBtn')
  await new Promise((r) => setTimeout(r, 3000))
  try { callee.kill('SIGTERM') } catch (_) {}

  console.log('sipbot tail:', out.join('').slice(-400))

  // Field flow: register console superuser, login, download the synthesized
  // WAV from the sipflow export path (/api/call-records/{id}/recording?stream=mixed).
  const apiHeaders = { Authorization: 'Bearer repro-api-token-123' }
  const req = ctx.request

  await new Promise((r) => setTimeout(r, 3000))
  let records = null
  let rec = null
  for (let attempt = 0; attempt < 10; attempt++) {
    const r = await req.post(`${PBX_URL}/api/call-records`, {
      headers: apiHeaders,
      data: { page: 1, per_page: 10, filters: null, sort: 'started_at_desc' },
    })
    if (r.ok()) {
      records = await r.json()
      const list = records.items || []
      if (list.length > 0) {
        rec = list[0]
        break
      }
    }
    await new Promise((r2) => setTimeout(r2, 1500))
  }
  console.log('records body:', JSON.stringify(records).slice(0, 400))
  expect(rec, 'a call record must exist').toBeTruthy()
  console.log('record id:', rec.id, 'call_id:', rec.call_id)

  const dl = await req.get(
    `${PBX_URL}/api/call-records/${rec.id}/recording?stream=mixed`,
    { headers: apiHeaders }
  )
  console.log('recording download status:', dl.status())
  expect(dl.ok(), 'recording download must succeed').toBeTruthy()
  const wavFile = path.join(tmpHome, 'downloaded.wav')
  fs.writeFileSync(wavFile, await dl.body())
  console.log('downloaded wav size:', fs.statSync(wavFile).size)

  const wav = parseG711StereoWav(wavFile)
  console.log(`wav fmt=${wav.fmtTag} channels=${wav.channels} rate=${wav.sampleRate} bytes=${wav.payload.length}`)

  // Support both the FileRecorder output (fmt 7 G.711 μ-law) and the sipflow
  // export output (fmt 1 PCM16).
  const frameBytes = wav.fmtTag === 7 ? 1 : 2
  const interleave = wav.channels === 2
  const stride = frameBytes * (interleave ? 2 : 1)
  const legAFrames = []
  const legBFrames = []
  for (let off = 0; off + stride <= wav.payload.length; off += stride) {
    legAFrames.push(wav.payload.subarray(off, off + frameBytes))
    if (interleave) legBFrames.push(wav.payload.subarray(off + frameBytes, off + stride))
  }
  const decodeFrame = (f) =>
    wav.fmtTag === 7 ? ulaw2pcm(f[0]) : f.readInt16LE(0)
  const legStats = (frames) => {
    let silence = 0
    let sumSq = 0
    for (const f of frames) {
      const v = decodeFrame(f)
      if (v === 0) silence++
      sumSq += v * v
    }
    return {
      silenceRatio: silence / Math.max(1, frames.length),
      rms: Math.sqrt(sumSq / Math.max(1, frames.length)),
    }
  }
  const a = legStats(legAFrames)
  const b = interleave ? legStats(legBFrames) : null
  console.log(
    `legA(caller ingress, browser mic): silenceRatio=${(a.silenceRatio * 100).toFixed(1)}% rms=${a.rms.toFixed(0)}`
  )
  if (b) {
    console.log(
      `legB(caller egress, sipbot echo):  silenceRatio=${(b.silenceRatio * 100).toFixed(1)}% rms=${b.rms.toFixed(0)}`
    )
    expect(b.silenceRatio, 'callee leg must not be constant silence').toBeLessThan(0.95)
  }
  // REPRO assertion: the field bug shows the caller leg 100% silence.
  expect(a.silenceRatio, 'caller leg must not be constant silence (field bug repro)').toBeLessThan(0.95)

  // Dump the pbx log tail to help diagnose if the repro fires.
  fs.writeFileSync(path.join(tmpHome, 'pbx.log'), pbxProc._log())
  await page.close()
  await ctx.close()
  await ownBrowser.close()
})
