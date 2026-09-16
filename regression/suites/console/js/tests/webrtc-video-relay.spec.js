// WebRTC video-relay e2e verification through the MediaBridge fast-path relay.
//
// Drives two phone_jssip.html tabs (bob = caller, alice = callee) against a
// running dev server, makes a video call, and asserts that BOTH sides receive
// and render REAL remote video. Two layers of assertion:
//
//   1. `remoteVideo.videoWidth > 0` — the browser decoded & rendered at least
//      one remote frame. A one-way relay bug (e.g. the caller's video payload
//      type not matching the relay rule, or a lost initial keyframe that PLI
//      can't recover) shows up as `videoWidth === 0` on exactly one side.
//   2. `remoteVideoHasLiveContent` — the rendered remote frame is non-black
//      AND changes over time, proving live video is actually flowing through
//      the relay rather than a black/static frame.
//
// By default the browsers send VP8 (software encoder). When VIDEO_CODEC=h264
// is set, `createOffer`/`createAnswer` are monkey-patched so the browser
// prefers H264 for its send direction — exercising the path a hardware
// camera uses.
//
// Requires the dev server on 127.0.0.1:8082 (config.toml.dev) with users
// bob/alice (password 123456). Uses the headless fake camera (a moving test
// pattern) so no real camera is needed.
const { test, expect } = require('@playwright/test')

const SERVER = process.env.PBX_BASE_URL || 'http://127.0.0.1:8082'
const PASSWORD = '123456'
const FORCE_H264 = (process.env.VIDEO_CODEC || '').toLowerCase() === 'h264'

// Reorder the video m-line so H264 payload types come before VP8, making the
// browser send H264 instead of VP8.
function h264First(sdp) {
  const lines = sdp.split('\r\n')
  const rtpmap = {}
  for (const l of lines) {
    const m = l.match(/^a=rtpmap:(\d+) (\w+)/)
    if (m) rtpmap[m[1]] = m[2]
  }
  const out = []
  for (const l of lines) {
    if (l.startsWith('m=video')) {
      const formats = l.split(' ').slice(3)
      const h264 = formats.filter((f) => (rtpmap[f] || '').toUpperCase() === 'H264')
      const rest = formats.filter((f) => !h264.includes(f))
      const reordered = [...h264, ...rest]
      out.push(`m=video ${l.split(' ')[1]} ${l.split(' ')[2]} ${reordered.join(' ')}`)
      continue
    }
    out.push(l)
  }
  return out.join('\r\n')
}

test.beforeEach(async ({ context }) => {
  if (FORCE_H264) {
    await context.addInitScript((src) => {
      const origOffer = RTCPeerConnection.prototype.createOffer
      const origAnswer = RTCPeerConnection.prototype.createAnswer
      const reorder = new Function('sdp', src) // eslint-disable-line no-new-func
      RTCPeerConnection.prototype.createOffer = function (...args) {
        return origOffer.apply(this, args).then((o) => {
          if (o && o.sdp) o.sdp = reorder(o.sdp)
          return o
        })
      }
      RTCPeerConnection.prototype.createAnswer = function (...args) {
        return origAnswer.apply(this, args).then((o) => {
          if (o && o.sdp) o.sdp = reorder(o.sdp)
          return o
        })
      }
    }, `(${h264First.toString()})(sdp)`)
  }
})

async function openPhone(context, user) {
  const page = await context.newPage()
  await page.goto(`${SERVER}/static/phone_jssip.html?caller=${user}&callee=alice`)
  await page.fill('#password', PASSWORD)
  // Enable video using the default CAMERA source. In headless the fake device
  // (`--use-fake-device-for-media-stream`, see playwright.config.js) emits a
  // constantly-moving test pattern — guaranteed live content. (The in-page
  // "Clock" canvas source is throttled in background tabs, which made the
  // live-content assertion flaky.)
  await page.check('#enableVideo')
  return page
}

async function registerPhone(page) {
  await page.click('#registerBtn')
  await expect(page.locator('#registrationStatus')).toContainText('Registered', {
    timeout: 15000,
  })
}

async function remoteVideoWidth(page) {
  return page.evaluate(() => {
    const v = document.getElementById('remoteVideo')
    return v && v.srcObject ? v.videoWidth : 0
  })
}

// Draw the remote video into an offscreen canvas and check it has bright,
// *changing* pixels. `videoWidth > 0` alone only proves a track was bound —
// a black or frozen frame still has dimensions. The headless fake camera
// emits a constantly-moving test pattern, so changing non-black pixels prove
// the relay is actually carrying the peer's live video.
async function remoteVideoHasLiveContent(page) {
  return page.evaluate(async () => {
    const v = document.getElementById('remoteVideo')
    if (!v || !v.srcObject || v.videoWidth === 0) return false
    const w = Math.min(v.videoWidth, 320)
    const h = Math.min(v.videoHeight, 240)
    if (w === 0 || h === 0) return false
    const c = document.createElement('canvas')
    c.width = w
    c.height = h
    const ctx = c.getContext('2d', { willReadFrequently: true })
    const snap = () => {
      ctx.drawImage(v, 0, 0, w, h)
      const d = ctx.getImageData(0, 0, w, h).data
      let lit = 0
      let sum = 0
      let n = 0
      for (let i = 0; i < d.length; i += 16) {
        const luma = (d[i] + d[i + 1] + d[i + 2]) / 3
        if (luma > 24) lit++
        sum += luma
        n++
      }
      return { lit, avg: sum / n }
    }
    const a = snap()
    await new Promise((r) => setTimeout(r, 1000))
    const b = snap()
    // Non-black frames in both samples AND a measurable change between them.
    return a.lit > 2 && b.lit > 2 && Math.abs(a.avg - b.avg) > 0.4
  })
}

test(`WebRTC video relay: video flows in BOTH directions (codec=${FORCE_H264 ? 'H264' : 'VP8'})`, async ({
  browser,
}) => {
  test.setTimeout(120000)

  const bobCtx = await browser.newContext({ permissions: ['microphone', 'camera'] })
  const aliceCtx = await browser.newContext({ permissions: ['microphone', 'camera'] })
  await Promise.all([
    bobCtx.grantPermissions(['microphone', 'camera'], { origin: SERVER }),
    aliceCtx.grantPermissions(['microphone', 'camera'], { origin: SERVER }),
  ])

  const bob = await openPhone(bobCtx, 'bob')
  const alice = await openPhone(aliceCtx, 'alice')
  await Promise.all([registerPhone(bob), registerPhone(alice)])

  await bob.fill('#callTarget', 'alice')
  await bob.click('#callBtn')

  const incoming = alice.locator('#incomingCall.active')
  await expect(incoming).toBeVisible({ timeout: 20000 })
  await alice.click('#answerBtn')

  await expect(bob.locator('#callControls.active')).toBeVisible({ timeout: 20000 })
  await expect(alice.locator('#callControls.active')).toBeVisible({ timeout: 20000 })

  // Layer 1: both directions must have decoded at least one remote frame.
  await expect
    .poll(async () => remoteVideoWidth(bob), { timeout: 30000, intervals: [1000] })
    .toBeGreaterThan(0)
  await expect
    .poll(async () => remoteVideoWidth(alice), { timeout: 30000, intervals: [1000] })
    .toBeGreaterThan(0)

  // Layer 2: the rendered remote frames must be live (non-black, changing) —
  // this is the assertion that actually catches a "relay forwards RTP but the
  // peer still shows black/static" regression.
  await expect
    .poll(() => remoteVideoHasLiveContent(bob), { timeout: 30000, intervals: [1500] })
    .toBe(true)
  await expect
    .poll(() => remoteVideoHasLiveContent(alice), { timeout: 30000, intervals: [1500] })
    .toBe(true)

  await bob.close()
  await alice.close()
  await bobCtx.close()
  await aliceCtx.close()
})
