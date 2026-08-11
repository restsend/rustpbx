/**
 * WebRTC ↔ RTP audio relay e2e — browser + sipbot bidirectional audio.
 *
 * Topology:
 *   Playwright Chromium (caller, WebRTC)  →  RustPBX  →  sipbot (callee, plain RTP)
 *   browser registers via jssip WS         →  leg A    →  leg B SIP registers
 *   browser calls 1002 (sipbot)            →  bridge   →  sipbot answers (echo)
 *
 * Verified:
 *   1. Browser inbound RTP stats: packetsReceived > 0, bytesReceived > N
 *   2. sipbot stdout shows RX/TX RTP packet counts > 0
 *   3. The rewrite bridge stamps MID on relayed packets (proven by browser
 *      being able to attribute them — without MID, stats would stay zero).
 *
 * Prerequisites:
 *   - Running RustPBX on 127.0.0.1:8082 with config.toml.dev (users bob/alice,
 *     password 123456, WebSocket SIP on :8082/ws)
 *   - Extensions bob (1000), alice (1001), and 1002 configured or auto-create
 *   - sipbot binary in PATH or set SIPBOT_BIN env var
 *   - Node dependencies: @playwright/test, child_process
 */

const { test, expect } = require('@playwright/test')
const { spawn } = require('child_process')
const path = require('path')
const fs = require('fs')
const os = require('os')

const PBX_URL = process.env.PBX_BASE_URL || 'http://127.0.0.1:8082'
const PASSWORD = '123456'
const SIPBOT_BIN = process.env.SIPBOT_BIN || 'sipbot'
const PBX_SIP = process.env.PBX_SIP_ADDR || '127.0.0.1:5060'

// ── helpers ──────────────────────────────────────────────────────────────

/** Generate a short tone WAV (8 kHz mono 16-bit PCM). */
function genToneWav(file, freq = 800, secs = 3) {
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
    b.writeInt16LE(
      Math.round(12000 * Math.sin((2 * Math.PI * freq * i) / sampleRate)),
      44 + i * 2
    )
  }
  fs.writeFileSync(file, b)
}

/**
 * Spawn a sipbot subprocess; returns { proc, output(), kill() }.
 * Accumulates stdout/stderr for assertions.
 */
function runSipbot(args, timeoutMs = 60000) {
  const out = []
  const proc = spawn(SIPBOT_BIN, args, {
    env: { ...process.env, RUST_LOG: 'info' },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  proc.stdout && proc.stdout.on('data', (d) => out.push(String(d)))
  proc.stderr && proc.stderr.on('data', (d) => out.push(String(d)))

  const timer = setTimeout(() => {
    try { proc.kill('SIGTERM') } catch (_) {}
  }, timeoutMs)

  return {
    proc,
    output: () => out.join(''),
    kill: () => {
      clearTimeout(timer)
      try { proc.kill('SIGTERM') } catch (_) {}
    },
  }
}

/** Parse sipbot's "RX: Np/Nb TX: Np/Nb" line from output. */
function parseSipbotRtpStats(output) {
  const rxMatch = output.match(/RX:\s*(\d+)p\/(\d+)b/)
  const txMatch = output.match(/TX:\s*(\d+)p\/(\d+)b/)
  return {
    rxPackets: rxMatch ? parseInt(rxMatch[1], 10) : 0,
    rxBytes: rxMatch ? parseInt(rxMatch[2], 10) : 0,
    txPackets: txMatch ? parseInt(txMatch[1], 10) : 0,
    txBytes: txMatch ? parseInt(txMatch[2], 10) : 0,
  }
}

// ── test ─────────────────────────────────────────────────────────────────

test.describe.configure({ mode: 'serial' })

test('WebRTC ↔ RTP audio relay: bidirectional audio verified via browser stats + sipbot RTP', async ({
  browser,
}) => {
  test.setTimeout(120000)

  const toneFile = path.join(os.tmpdir(), `sipbot-tone-${Date.now()}.wav`)
  const recordOut = path.join(os.tmpdir(), `sipbot-out-${Date.now()}.wav`)
  genToneWav(toneFile, 800, 3)

  // ── 1. Start sipbot callee (extension 1002, auto-answer echo mode) ──
  const callee = runSipbot([
    'wait',
    '1002',
    '--uri',
    PBX_SIP,
    '--mode',
    'echo',
    '--record-out',
    recordOut,
    '--opus',
  ])

  // Wait for sipbot to register (it emits "Registered" or similar).
  // Give it a moment to start.
  await new Promise((r) => setTimeout(r, 2000))
  let calleeOutput = callee.output()
  if (!calleeOutput.includes('register')) {
    // may still be starting; give more time
    await new Promise((r) => setTimeout(r, 2000))
    calleeOutput = callee.output()
  }
  console.log('sipbot callee initial output:', calleeOutput.slice(0, 500))

  // ── 2. Open browser, register WebRTC caller (bob) ──
  const ctx = await browser.newContext({
    permissions: ['microphone'],
  })
  await ctx.grantPermissions(['microphone'], { origin: PBX_URL })

  const page = await ctx.newPage()
  // phone_jssip.html params: caller/callee pre-fill the UI fields
  await page.goto(`${PBX_URL}/static/phone_jssip.html?caller=bob&callee=1002`, {
    waitUntil: 'domcontentloaded',
    timeout: 15000,
  })

  // Fill password and register
  await page.fill('#password', PASSWORD)
  await page.click('#registerBtn')
  await expect(page.locator('#registrationStatus')).toContainText('Registered', {
    timeout: 15000,
  })

  // ── 3. Make the call from browser → sipbot ──
  await page.fill('#callTarget', '1002')
  await page.click('#callBtn')

  // Wait for the call to be active (callControls visible)
  await expect(page.locator('#callControls.active')).toBeVisible({ timeout: 30000 })

  console.log('Call established — waiting for audio stats to accumulate...')

  // ── 4. Poll browser inbound RTP stats ──
  // phone_jssip.html has #rxPackets, #rxBytes that are updated by
  // its setInterval(stats, 2000) loop. Wait for > 0.
  await expect
    .poll(
      async () => {
        const text = await page.locator('#rxPackets').textContent()
        return parseInt(text, 10) || 0
      },
      { timeout: 30000, intervals: [1500] }
    )
    .toBeGreaterThan(10)

  const rxBytes = await page.locator('#rxBytes').textContent()
  console.log(`Browser inbound audio: ${rxBytes}`)

  // ── 5. Let audio flow for a few more seconds, then verify sipbot stats ──
  await new Promise((r) => setTimeout(r, 3000))

  calleeOutput = callee.output()
  console.log('sipbot final output:', calleeOutput.slice(-500))

  const stats = parseSipbotRtpStats(calleeOutput)
  console.log(
    `sipbot RTP stats: RX=${stats.rxPackets}p/${stats.rxBytes}b TX=${stats.txPackets}p/${stats.txBytes}b`
  )

  // sipbot must have received some packets (browser → sipbot direction)
  expect(stats.rxPackets).toBeGreaterThan(
    0,
    `sipbot should receive RTP from browser (browser → agent direction); output: ${calleeOutput.slice(-800)}`
  )
  // sipbot must have sent some packets (sipbot → browser direction)
  expect(stats.txPackets).toBeGreaterThan(
    0,
    `sipbot should send RTP to browser (agent → browser direction); output: ${calleeOutput.slice(-800)}`
  )

  // ── 6. Verify recorded output WAV from sipbot has content ──
  if (fs.existsSync(recordOut)) {
    const wavSize = fs.statSync(recordOut).size
    console.log(`sipbot recorded WAV: ${wavSize} bytes`)
    // WAV header is 44 bytes + audio data. Even a few packets should produce
    // measurable audio data.
    expect(wavSize).toBeGreaterThan(
      44,
      'sipbot recorded output WAV must contain audio data beyond the header'
    )
  }

  // Cleanup
  callee.kill()
  try { fs.unlinkSync(toneFile) } catch (_) {}
  try { fs.unlinkSync(recordOut) } catch (_) {}
  await page.close()
  await ctx.close()
})
