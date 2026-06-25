/**
 * ═══════════════════════════════════════════════════════════════
 * Console Baseline Screenshot Capture
 *
 * Walks every admin console page, captures a full-page screenshot for
 * human review, and emits manifest.json + index.html (review grid).
 *
 * This is deliberately lenient: a single page returning non-2xx does NOT
 * abort the run — the goal is to capture ALL pages so a human can spot
 * layout regressions before/after the JSON+Alpine refactor.
 * ═══════════════════════════════════════════════════════════════
 */
const { test } = require('@playwright/test')
const path = require('path')
const fs = require('fs')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const { snap, writeReviewIndex, SCREENSHOT_DIR, ensureDir } = require('./helpers/screenshot')

ensureDir(SCREENSHOT_DIR)

// ── Page inventory (pre-refactor, default features) ──────────
// Paths are relative to base_path (/console). Auth pages don't need login.
const AUTH_PAGES = [
  { name: 'auth-01-login', path: '/login', auth: false },
  { name: 'auth-02-forgot', path: '/forgot', auth: false },
]

const CONSOLE_PAGES = [
  { name: '01-dashboard', path: '/', auth: true },
  { name: '02-extensions-list', path: '/extensions', auth: true },
  { name: '03-extension-new', path: '/extensions/new', auth: true },
  { name: '04-sip-trunk-list', path: '/sip-trunk', auth: true },
  { name: '05-sip-trunk-new', path: '/sip-trunk/new', auth: true },
  { name: '06-routing-list', path: '/routing', auth: true },
  { name: '07-routing-new', path: '/routing/new', auth: true },
  { name: '08-settings', path: '/settings', auth: true },
  { name: '09-call-records', path: '/call-records', auth: true },
  { name: '10-diagnostics', path: '/diagnostics', auth: true },
  { name: '11-addons', path: '/addons', auth: true },
  { name: '12-notifications', path: '/notifications', auth: true },
  { name: '13-metrics-runtime', path: '/metrics/runtime', auth: true },
  { name: '14-licenses', path: '/licenses', auth: true },
]

/**
 * Visit a page, wait for it to settle, capture a full-page screenshot.
 * Returns a result record for the manifest.
 */
async function capturePage(page, entry) {
  const url = `${BASE_URL}${entry.path}`
  const result = { ...entry, url, status: 0, ok: false, file: `${entry.name}.png`, redirect: null, error: null }
  try {
    const resp = await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 })
    result.status = resp ? resp.status() : 0
    // Final URL after any redirect chain.
    const finalUrl = page.url()
    if (!finalUrl.endsWith(entry.path) && !finalUrl.replace(/\/$/, '').endsWith(entry.path.replace(/\/$/, ''))) {
      const m = finalUrl.match(/\/console(\/[^\?]*)/)
      result.redirect = m ? m[1] : finalUrl
    }
    // Let Alpine-driven async data settle (SSR pages also fetch lists via fetch).
    await page.waitForLoadState('networkidle', { timeout: 6000 }).catch(() => {})
    await page.waitForTimeout(900)
    await snap(page, entry.name, { fullPage: true })
    result.ok = result.status > 0 && result.status < 400
  } catch (e) {
    result.error = e.message.split('\n')[0]
    // Best-effort capture even on failure.
    await snap(page, `${entry.name}-error`, { fullPage: true }).catch(() => {})
    result.file = `${entry.name}-error.png`
  }
  const tag = result.ok ? '✓' : '✗'
  const redir = result.redirect ? ` → ${result.redirect}` : ''
  console.log(`  ${tag} ${String(result.status).padEnd(3)} ${entry.name.padEnd(24)} ${entry.path}${redir}`)
  return result
}

test.describe.configure({ mode: 'serial' })

test.describe('Console baseline — auth pages (no login)', () => {
  test('capture auth pages', async ({ page }) => {
    const results = []
    for (const entry of AUTH_PAGES) {
      results.push(await capturePage(page, entry))
    }
    // Persist partial manifest; final one is written by the authenticated test.
    fs.writeFileSync(path.join(SCREENSHOT_DIR, 'auth-manifest.json'), JSON.stringify(results, null, 2))
  })
})

test.describe('Console baseline — authenticated pages', () => {
  test('login then capture all console pages', async ({ page }) => {
    // Login first (PRG flow). Screenshot the post-login dashboard too.
    await consoleLogin(page)
    await snap(page, '00-after-login', { fullPage: true })

    const results = [{ name: '00-after-login', path: '/', auth: true, url: page.url(), status: 200, ok: true, file: '00-after-login.png', redirect: null, error: null }]
    for (const entry of CONSOLE_PAGES) {
      results.push(await capturePage(page, entry))
    }

    // Merge auth manifest if present.
    const authPath = path.join(SCREENSHOT_DIR, 'auth-manifest.json')
    let all = results
    if (fs.existsSync(authPath)) {
      all = JSON.parse(fs.readFileSync(authPath, 'utf8')).concat(results)
    }

    const indexPath = writeReviewIndex(all)
    const failed = all.filter((r) => !r.ok)
    console.log(`\n  📸 ${all.length} pages captured → ${path.relative(process.cwd(), indexPath)}`)
    if (failed.length) {
      console.log(`  ⚠ ${failed.length} page(s) had issues: ${failed.map((f) => f.name).join(', ')}`)
    }
  })
})
