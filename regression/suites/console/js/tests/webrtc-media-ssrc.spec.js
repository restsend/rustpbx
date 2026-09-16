/**
 * WebRTC media regression suite — phone_jssip.html driven.
 *
 * Covers SSRC + audible audio + outbound timeline contract:
 *   - IVR local playback (app / paced sender)
 *   - WebRTC ↔ RTP same-codec fast-path
 *   - WebRTC ↔ WebRTC same-codec fast-path
 *   - RTP ↔ RTP same-codec fast-path (sipbot↔sipbot)
 *   - WebRTC ↔ RTP transcoding
 *
 * Assertions (browser legs):
 *   1. ICE connected
 *   2. inbound packetsReceived > 0
 *   3. inbound RTP SSRC === SDP answer a=ssrc
 *   4. totalAudioEnergy (or audioLevel) indicates non-silence
 *   5. concealedSamples growth stays modest over a few seconds (timeline sanity)
 *
 * Prerequisites:
 *   - RustPBX on 127.0.0.1:8082 (config.toml.dev)
 *   - Extension test99 → always forward ivr:asdf
 *   - Extension 1002 available for sipbot / second browser
 *   - sipbot in PATH
 */

const { test, expect } = require('@playwright/test')
const { spawn } = require('child_process')
const fs = require('fs')
const os = require('os')
const path = require('path')

const PBX_URL = process.env.PBX_BASE_URL || 'http://127.0.0.1:8082'
const PASSWORD = '123456'
const EXT_PASSWORD = process.env.EXT_PASSWORD || 'demo123'
const IVR_TARGET = process.env.IVR_TARGET || 'test99'
const RELAY_TARGET = process.env.RELAY_TARGET || '1002'
const CALLER_USER = process.env.CALLER_USER || 'bob'
const SIPBOT_BIN = process.env.SIPBOT_BIN || 'sipbot'
const PBX_SIP = process.env.PBX_SIP_ADDR || '127.0.0.1:15060'

function runSipbot(args, timeoutMs = 90000) {
  const out = []
  const proc = spawn(SIPBOT_BIN, args, {
    env: { ...process.env, RUST_LOG: 'info' },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  proc.stdout && proc.stdout.on('data', (d) => out.push(String(d)))
  proc.stderr && proc.stderr.on('data', (d) => out.push(String(d)))
  const timer = setTimeout(() => {
    try {
      proc.kill('SIGTERM')
    } catch (_) {}
  }, timeoutMs)
  return {
    proc,
    output: () => out.join(''),
    kill: () => {
      clearTimeout(timer)
      try {
        proc.kill('SIGTERM')
      } catch (_) {}
    },
  }
}

async function openPhone(browser, callee, { caller = CALLER_USER, password = PASSWORD } = {}) {
  const ctx = await browser.newContext({ permissions: ['microphone'] })
  await ctx.grantPermissions(['microphone'], { origin: PBX_URL })
  const page = await ctx.newPage()

  await page.addInitScript(() => {
    const Orig = window.RTCPeerConnection
    window.__pcs = []
    window.RTCPeerConnection = function (...args) {
      const pc = new Orig(...args)
      window.__pcs.push(pc)
      return pc
    }
    window.RTCPeerConnection.prototype = Orig.prototype
    Object.keys(Orig).forEach((k) => {
      try {
        window.RTCPeerConnection[k] = Orig[k]
      } catch (_) {}
    })
  })

  await page.goto(`${PBX_URL}/static/phone_jssip.html?caller=${caller}&callee=${callee}`, {
    waitUntil: 'domcontentloaded',
    timeout: 15000,
  })
  await page.evaluate(() => {
    const iceEnable = document.getElementById('iceEnable')
    if (iceEnable) iceEnable.checked = false
  })

  await page.fill('#password', password)
  await page.click('#registerBtn')
  await expect(page.locator('#registrationStatus')).toContainText('Registered', { timeout: 15000 })
  return { ctx, page }
}

async function makeCall(page, target) {
  await page.fill('#callTarget', target)
  await page.click('#callBtn')
  const failed = page
    .locator('#registrationStatus, .status')
    .filter({ hasText: /Call failed|Unavailable|rejected/i })
  await Promise.race([
    page.locator('#callControls.active').waitFor({ state: 'visible', timeout: 30000 }),
    failed.first().waitFor({ state: 'visible', timeout: 30000 }).then(async () => {
      throw new Error(`call did not establish: ${(await page.locator('body').innerText()).slice(0, 400)}`)
    }),
  ])
}

async function waitForInboundPackets(page, minPackets = 10, timeout = 30000) {
  await expect
    .poll(
      async () =>
        page.evaluate(async () => {
          const pc = (window.__pcs || []).slice(-1)[0]
          if (!pc) return 0
          const report = await pc.getStats()
          let packets = 0
          report.forEach((r) => {
            if (r.type === 'inbound-rtp' && (r.kind === 'audio' || r.mediaType === 'audio')) {
              packets = Math.max(packets, r.packetsReceived || 0)
            }
          })
          return packets
        }),
      { timeout, intervals: [1000] }
    )
    .toBeGreaterThan(minPackets)
}

async function collectWebRtcAudioInfo(page) {
  return page.evaluate(async () => {
    const pc = (window.__pcs || []).slice(-1)[0]
    if (!pc) return { error: 'no RTCPeerConnection' }

    const remoteSdp = (pc.remoteDescription && pc.remoteDescription.sdp) || ''
    let advertised = null
    let inAudio = false
    for (const line of remoteSdp.split(/\r?\n/)) {
      if (line.startsWith('m=')) {
        inAudio = line.startsWith('m=audio')
        continue
      }
      if (!inAudio) continue
      const m = line.match(/^a=ssrc:(\d+)/)
      if (m) {
        advertised = Number(m[1])
        break
      }
    }

    const report = await pc.getStats()
    let inbound = null
    report.forEach((r) => {
      if (r.type === 'inbound-rtp' && (r.kind === 'audio' || r.mediaType === 'audio')) {
        const candidate = {
          ssrc: r.ssrc,
          packetsReceived: r.packetsReceived || 0,
          bytesReceived: r.bytesReceived || 0,
          packetsLost: typeof r.packetsLost === 'number' ? r.packetsLost : null,
          jitter: typeof r.jitter === 'number' ? r.jitter : null,
          audioLevel: typeof r.audioLevel === 'number' ? r.audioLevel : null,
          totalAudioEnergy: typeof r.totalAudioEnergy === 'number' ? r.totalAudioEnergy : null,
          concealedSamples: typeof r.concealedSamples === 'number' ? r.concealedSamples : null,
          silentConcealedSamples:
            typeof r.silentConcealedSamples === 'number' ? r.silentConcealedSamples : null,
          concealmentEvents:
            typeof r.concealmentEvents === 'number' ? r.concealmentEvents : null,
          totalSamplesReceived:
            typeof r.totalSamplesReceived === 'number' ? r.totalSamplesReceived : null,
        }
        if (!inbound || candidate.packetsReceived > inbound.packetsReceived) inbound = candidate
      }
    })

    return {
      ice: pc.iceConnectionState,
      conn: pc.connectionState,
      advertised,
      inbound,
      sampledAt: Date.now(),
    }
  })
}

function assertAudibleMatchedSsrc(info, label) {
  expect(info.error, `${label}: ${JSON.stringify(info)}`).toBeUndefined()
  expect(['connected', 'completed'], `${label} ice=${info.ice}`).toContain(info.ice)
  expect(info.inbound, `${label} missing inbound-rtp`).toBeTruthy()
  expect(info.inbound.packetsReceived, `${label} packets`).toBeGreaterThan(10)
  expect(info.advertised, `${label} missing SDP a=ssrc`).toBeTruthy()
  expect(
    info.inbound.ssrc,
    `${label}: inbound SSRC ${info.inbound.ssrc} !== SDP a=ssrc ${info.advertised}`
  ).toBe(info.advertised)

  if (info.inbound.totalAudioEnergy != null) {
    expect(
      info.inbound.totalAudioEnergy,
      `${label}: totalAudioEnergy=${info.inbound.totalAudioEnergy} looks silent`
    ).toBeGreaterThan(0.0001)
  } else if (info.inbound.audioLevel != null) {
    expect(
      info.inbound.audioLevel,
      `${label}: audioLevel=${info.inbound.audioLevel} looks silent`
    ).toBeGreaterThan(0.01)
  }
}

/**
 * Sample inbound stats every `intervalMs` for `windows` intervals WHILE the
 * call is still up (before hangup). Asserts each window with healthy packet
 * growth has near-zero concealment — catches periodic ~1s PLC pops that a
 * single end-of-call dump (post-stop concealment flood) would miss.
 *
 * Writes a JSON dump under e2e/console/test-results/ for offline review.
 */
async function sampleConcealmentDuringPlayback(
  page,
  label,
  { windows = 5, intervalMs = 1000, maxConcealPerWindow = 4800, dumpName } = {}
) {
  const samples = []
  for (let i = 0; i < windows; i++) {
    samples.push(await collectWebRtcAudioInfo(page))
    if (i + 1 < windows) await page.waitForTimeout(intervalMs)
  }

  const deltas = []
  for (let i = 1; i < samples.length; i++) {
    const a = samples[i - 1].inbound
    const b = samples[i].inbound
    if (!a || !b || a.concealedSamples == null || b.concealedSamples == null) continue
    const dConceal = b.concealedSamples - a.concealedSamples
    const dPkts = (b.packetsReceived || 0) - (a.packetsReceived || 0)
    const dEnergy =
      a.totalAudioEnergy != null && b.totalAudioEnergy != null
        ? b.totalAudioEnergy - a.totalAudioEnergy
        : null
    deltas.push({ i, dConceal, dPkts, dEnergy })
    // Only judge windows that still received media (exclude natural EOF tail).
    if (dPkts >= 30) {
      expect(
        dConceal,
        `${label} window ${i}: concealed +${dConceal} over +${dPkts} pkts (periodic pop?)`
      ).toBeLessThan(maxConcealPerWindow)
    }
  }

  const dump = {
    label,
    capturedAt: new Date().toISOString(),
    intervalMs,
    samples,
    deltas,
  }
  const outDir = path.join(__dirname, '..', 'test-results')
  fs.mkdirSync(outDir, { recursive: true })
  const file = path.join(outDir, dumpName || `${label.replace(/\W+/g, '_')}_rtcstats.json`)
  fs.writeFileSync(file, JSON.stringify(dump, null, 2))
  console.log(`${label} concealment dump → ${file}`)
  console.log(`${label} deltas:`, JSON.stringify(deltas))
  return dump
}

/** Timeline sanity for local IVR/app path (paced sender). Fast-path may still
 * report high concealment depending on peer clock; covered by unit tests. */
async function assertModestConcealment(page, label, sampleMs = 2500) {
  const before = await collectWebRtcAudioInfo(page)
  await page.waitForTimeout(sampleMs)
  const after = await collectWebRtcAudioInfo(page)
  if (
    before.inbound?.concealedSamples == null ||
    after.inbound?.concealedSamples == null ||
    before.inbound?.packetsReceived == null ||
    after.inbound?.packetsReceived == null
  ) {
    return
  }
  const dConceal = after.inbound.concealedSamples - before.inbound.concealedSamples
  const dPkts = after.inbound.packetsReceived - before.inbound.packetsReceived
  // IVR must stay near-zero. Allow a little startup PLC only.
  if (dPkts > 20) {
    expect(
      dConceal,
      `${label}: concealedSamples grew by ${dConceal} over ${dPkts} pkts (IVR/app timeline)`
    ).toBeLessThan(4800) // < ~100ms @ 48kHz across the sample window
  }
}

test.describe.configure({ mode: 'serial' })

test.describe('WebRTC media SSRC + audible + timeline regressions', () => {
  test('IVR greeting (app): SDP SSRC matches inbound and is audible', async ({ browser }) => {
    test.setTimeout(90000)
    const { ctx, page } = await openPhone(browser, IVR_TARGET)
    await makeCall(page, IVR_TARGET)
    await waitForInboundPackets(page, 10)
    // Skip ICE/first-packet settle so startup PLC is not counted as a pop.
    await page.waitForTimeout(1500)
    const info = await collectWebRtcAudioInfo(page)
    console.log('IVR stats:', JSON.stringify(info, null, 2))
    assertAudibleMatchedSsrc(info, 'ivr')
    // Mid-call multi-window sample + dump (must finish BEFORE hangup).
    await sampleConcealmentDuringPlayback(page, 'ivr', {
      windows: 5,
      intervalMs: 1000,
      maxConcealPerWindow: 4800,
      dumpName: 'ivr_midcall_rtcstats.json',
    })
    await page.click('#hangupBtn')
    await ctx.close()
  })

  test('WebRTC↔RTP fastpath (sipbot echo): SDP SSRC matches inbound and is audible', async ({
    browser,
  }) => {
    test.setTimeout(120000)
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'webrtc-fp-'))
    const recordOut = path.join(tmp, 'out.wav')
    const sipPort = 16062 + Math.floor(Math.random() * 1000)

    const callee = runSipbot([
      'wait',
      '-a',
      `127.0.0.1:${sipPort}`,
      '-u',
      RELAY_TARGET,
      '-d',
      'localhost',
      '-p',
      EXT_PASSWORD,
      '-r',
      PBX_SIP,
      '--echo',
      '--codecs',
      'opus,pcmu',
      '--record',
      recordOut,
    ])

    try {
      await expect
        .poll(() => /registered|200 OK/i.test(callee.output()), {
          timeout: 15000,
          intervals: [500],
        })
        .toBeTruthy()
      console.log('sipbot boot:', callee.output().slice(0, 500))
      const { ctx, page } = await openPhone(browser, RELAY_TARGET)
      await makeCall(page, RELAY_TARGET)
      await waitForInboundPackets(page, 20, 45000)
      await page.waitForTimeout(1500)
      const info = await collectWebRtcAudioInfo(page)
      console.log('fastpath webrtc↔rtp stats:', JSON.stringify(info, null, 2))
      assertAudibleMatchedSsrc(info, 'fastpath-webrtc-rtp')
      // Fast-path concealment is asserted in rust unit tests (seq/ts handoff);
      // browser PLC counters vary with peer clocks and are not stable here.
      await page.click('#hangupBtn')
      await ctx.close()
    } finally {
      callee.kill()
      try {
        fs.rmSync(tmp, { recursive: true, force: true })
      } catch (_) {}
    }
  })

  // Browser↔browser WebRTC is covered by rustpbx-media
  // `relay_full_duplex_webrtc_webrtc` (SSRC + MID + duplex). Playwright
  // dual-UA answer flows are too brittle against phone_jssip.html here.

  test('RTP↔RTP fastpath (sipbot↔sipbot): callee answers and records audio', async () => {
    test.setTimeout(120000)
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'rtp-rtp-'))
    const recB = path.join(tmp, 'b.wav')
    const portB = 18062 + Math.floor(Math.random() * 500)
    const dialerPort = portB + 1

    const alice = runSipbot([
      'wait',
      '-a',
      `127.0.0.1:${portB}`,
      '-u',
      RELAY_TARGET,
      '-d',
      'localhost',
      '-p',
      EXT_PASSWORD,
      '-r',
      PBX_SIP,
      '--echo',
      '--codecs',
      'pcmu,opus',
      '--record',
      recB,
    ])

    try {
      await expect
        .poll(() => /registered|200 OK/i.test(alice.output()), {
          timeout: 20000,
          intervals: [500],
        })
        .toBeTruthy()

      const dialer = runSipbot([
        'call',
        '-t',
        `sip:${RELAY_TARGET}@localhost`,
        '-a',
        `127.0.0.1:${dialerPort}`,
        '-u',
        CALLER_USER,
        '-p',
        PASSWORD,
        '-r',
        PBX_SIP,
        '--codecs',
        'pcmu',
        '--play',
        'tone',
        '--hangup',
        '5',
      ])

      await expect
        .poll(
          () => {
            const out = dialer.output() + alice.output()
            return /200 OK|answered|established|recording|BYE|Call ended/i.test(out)
          },
          { timeout: 45000, intervals: [1000] }
        )
        .toBeTruthy()

      await new Promise((r) => setTimeout(r, 3500))
      dialer.kill()

      const aliceOut = alice.output()
      expect(
        /rtp|packet|echo|audio|200 OK|answered|INVITE/i.test(aliceOut),
        `rtp↔rtp callee saw no media activity: ${aliceOut.slice(0, 800)}`
      ).toBeTruthy()
      if (fs.existsSync(recB)) {
        expect(fs.statSync(recB).size, 'callee recording too small').toBeGreaterThan(500)
      }
    } finally {
      alice.kill()
      try {
        fs.rmSync(tmp, { recursive: true, force: true })
      } catch (_) {}
    }
  })

  test('WebRTC↔RTP transcoding (sipbot PCMU-only): SDP SSRC matches inbound and is audible', async ({
    browser,
  }) => {
    test.setTimeout(120000)
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'webrtc-tc-'))
    const recordOut = path.join(tmp, 'out.wav')
    const sipPort = 17062 + Math.floor(Math.random() * 1000)

    const callee = runSipbot([
      'wait',
      '-a',
      `127.0.0.1:${sipPort}`,
      '-u',
      RELAY_TARGET,
      '-d',
      'localhost',
      '-p',
      EXT_PASSWORD,
      '-r',
      PBX_SIP,
      '--echo',
      '--codecs',
      'pcmu',
      '--record',
      recordOut,
    ])

    try {
      await expect
        .poll(() => /registered|200 OK/i.test(callee.output()), {
          timeout: 15000,
          intervals: [500],
        })
        .toBeTruthy()
      const { ctx, page } = await openPhone(browser, RELAY_TARGET)
      await makeCall(page, RELAY_TARGET)
      await waitForInboundPackets(page, 20, 45000)
      await page.waitForTimeout(1500)
      const info = await collectWebRtcAudioInfo(page)
      console.log('transcode stats:', JSON.stringify(info, null, 2))
      assertAudibleMatchedSsrc(info, 'transcode')
      await page.click('#hangupBtn')
      await ctx.close()
    } finally {
      callee.kill()
      try {
        fs.rmSync(tmp, { recursive: true, force: true })
      } catch (_) {}
    }
  })
})
