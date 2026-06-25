/**
 * ═══════════════════════════════════════════════════════════════
 * Wholesale pages — baseline screenshot capture
 *
 * Walks every wholesale admin page, captures screenshots for human
 * review before the Form→Json migration. Only runs when wholesale
 * addon is enabled (ENABLED_ADDONS includes "wholesale").
 * ═══════════════════════════════════════════════════════════════
 */
const { test } = require('@playwright/test')
const path = require('path')
const fs = require('fs')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const { snap, writeReviewIndex, SCREENSHOT_DIR, ensureDir } = require('./helpers/screenshot')

const ENABLED = (process.env.ENABLED_ADDONS || '').split(',').map(s => s.trim()).includes('wholesale')

// Wholesale page inventory (list/form pages, no detail pages needing IDs).
const WHOLESALE_PAGES = [
  { name: 'ws-01-dashboard', path: '/wholesale' },
  { name: 'ws-02-settings', path: '/wholesale/settings' },
  { name: 'ws-03-tenants', path: '/wholesale/tenants' },
  { name: 'ws-04-tenants-new', path: '/wholesale/tenants/new' },
  { name: 'ws-05-profiles', path: '/wholesale/profiles' },
  { name: 'ws-06-profiles-new', path: '/wholesale/profiles/new' },
  { name: 'ws-07-rate-decks', path: '/wholesale/rate-decks' },
  { name: 'ws-08-rate-decks-new', path: '/wholesale/rate-decks/new' },
  { name: 'ws-09-trunks', path: '/wholesale/trunks' },
  { name: 'ws-10-cdrs', path: '/wholesale/cdrs' },
  { name: 'ws-11-cdrs-exports', path: '/wholesale/cdrs/exports' },
  { name: 'ws-12-limiters', path: '/wholesale/limiters' },
  { name: 'ws-13-cluster', path: '/wholesale/cluster' },
]

async function capturePage(page, entry) {
  const url = `${BASE_URL}${entry.path}`
  const result = { ...entry, url, status: 0, ok: false, file: `${entry.name}.png`, redirect: null, error: null }
  try {
    const resp = await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 })
    result.status = resp ? resp.status() : 0
    await page.waitForLoadState('networkidle', { timeout: 5000 }).catch(() => {})
    await page.waitForTimeout(800)
    await snap(page, entry.name, { fullPage: true })
    result.ok = result.status > 0 && result.status < 400
  } catch (e) {
    result.error = e.message.split('\n')[0]
    await snap(page, `${entry.name}-error`, { fullPage: true }).catch(() => {})
    result.file = `${entry.name}-error.png`
  }
  const tag = result.ok ? '✓' : '✗'
  console.log(`  ${tag} ${String(result.status).padEnd(3)} ${entry.name.padEnd(22)} ${entry.path}`)
  return result
}

// Only run when wholesale is enabled.
const describeFn = ENABLED ? test.describe : test.describe.skip
describeFn('Wholesale baseline — screenshot capture', () => {
  test('login then capture all wholesale pages', async ({ page }) => {
    await consoleLogin(page)
    const results = []
    for (const entry of WHOLESALE_PAGES) {
      results.push(await capturePage(page, entry))
    }
    const indexPath = writeReviewIndex(results, { dir: path.join(SCREENSHOT_DIR) })
    const failed = results.filter(r => !r.ok)
    console.log(`\n  📸 ${results.length} wholesale pages → ${path.relative(process.cwd(), indexPath)}`)
    if (failed.length) console.log(`  ⚠ ${failed.length} page(s) had issues: ${failed.map(f => f.name).join(', ')}`)
  })
})
