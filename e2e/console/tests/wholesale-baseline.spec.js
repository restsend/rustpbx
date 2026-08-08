/**
 * ═══════════════════════════════════════════════════════════════
 * Wholesale pages — baseline screenshot capture + visual gate
 *
 * Walks every wholesale admin page (list, form, detail and edit pages)
 * and captures screenshots. Only runs when the wholesale addon is
 * enabled (ENABLED_ADDONS includes "wholesale").
 *
 * Two modes:
 *   default         → lenient capture for human review (snap + index.html).
 *                     A page returning 4xx/5xx is reported but does not fail.
 *   VISUAL_GATE=1   → additionally asserts `toHaveScreenshot()` per page, so
 *                     unintended visual regressions fail the run automatically.
 *                     Dynamic pages (dashboard, CDRs) are excluded from the
 *                     strict pixel gate (live metrics / timestamps) but are
 *                     still captured for human review.
 *
 * Detail/edit pages need real entity ids, so the test first seeds one
 * rate-deck, carrier trunk, routing profile and tenant over the JSON API and
 * deletes them again at the end.
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const path = require('path')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const { snap, writeReviewIndex, SCREENSHOT_DIR } = require('./helpers/screenshot')
const {
  getJsonHeaders,
  seedWholesaleEntities,
  cleanupWholesaleEntities,
} = require('./helpers/wholesale-seed')

const ENABLED = (process.env.ENABLED_ADDONS || '')
  .split(',')
  .map((s) => s.trim())
  .includes('wholesale')

// Strict pixel-diff gate (opt-in). Default stays lenient human-review capture.
const VISUAL_GATE = process.env.VISUAL_GATE === '1'

/**
 * Page inventory. `dynamic: true` marks pages with live/timestamped content
 * that are excluded from the strict pixel gate (still captured for review).
 * `path` may contain a `#hash` to select an in-page tab.
 */
function wholesalePages(seed) {
  return [
    // ── Static pages (no ids needed) ────────────────────────────
    { name: 'ws-01-dashboard', path: '/wholesale', dynamic: true },
    { name: 'ws-02-settings', path: '/wholesale/settings' },
    { name: 'ws-03-tenants', path: '/wholesale/tenants' },
    { name: 'ws-04-tenants-new', path: '/wholesale/tenants/new' },
    { name: 'ws-05-profiles', path: '/wholesale/profiles' },
    { name: 'ws-06-profiles-new', path: '/wholesale/profiles/new' },
    { name: 'ws-07-rate-decks', path: '/wholesale/rate-decks' },
    { name: 'ws-08-rate-decks-new', path: '/wholesale/rate-decks/new' },
    { name: 'ws-09-trunks', path: '/wholesale/trunks' },
    { name: 'ws-10-cdrs', path: '/wholesale/cdrs', dynamic: true },
    { name: 'ws-11-cdrs-exports', path: '/wholesale/cdrs/exports' },
    { name: 'ws-12-cluster', path: '/wholesale/cluster' },
    { name: 'ws-13-diagnostics', path: '/wholesale/diagnostics' },

    // ── Tenant detail (5 hash tabs) + edit + trunk form ─────────
    { name: 'ws-20-tenant-detail', path: `/wholesale/tenants/${seed.tenantId}` },
    { name: 'ws-21-tenant-detail-rates', path: `/wholesale/tenants/${seed.tenantId}#rates` },
    { name: 'ws-22-tenant-detail-recharges', path: `/wholesale/tenants/${seed.tenantId}#recharges` },
    { name: 'ws-23-tenant-detail-billing', path: `/wholesale/tenants/${seed.tenantId}#billing` },
    { name: 'ws-24-tenant-detail-diagnostics', path: `/wholesale/tenants/${seed.tenantId}#diagnostics` },
    { name: 'ws-25-tenant-edit', path: `/wholesale/tenants/${seed.tenantId}/edit` },
    { name: 'ws-26-tenant-trunk-new', path: `/wholesale/tenants/${seed.tenantId}/trunks/new` },

    // ── Profile detail + edit + rule form ───────────────────────
    { name: 'ws-30-profile-detail', path: `/wholesale/profiles/${seed.profileId}` },
    { name: 'ws-31-profile-edit', path: `/wholesale/profiles/${seed.profileId}/edit` },
    { name: 'ws-32-profile-item-new', path: `/wholesale/profiles/${seed.profileId}/items/new` },

    // ── Rate deck detail + edit + import ────────────────────────
    { name: 'ws-40-rate-deck-detail', path: `/wholesale/rate-decks/${seed.deckId}` },
    { name: 'ws-41-rate-deck-edit', path: `/wholesale/rate-decks/${seed.deckId}/edit` },
    { name: 'ws-42-rate-import', path: `/wholesale/rate-decks/${seed.deckId}/import` },

    // ── Carrier trunk detail + config form ──────────────────────
    { name: 'ws-50-trunk-detail', path: `/wholesale/trunks/${seed.trunkId}` },
    { name: 'ws-51-trunk-edit', path: `/wholesale/trunks/${seed.trunkId}/edit` },

    // ── Diagnostics second tab (route simulator) ────────────────
    { name: 'ws-60-diagnostics-route-sim', path: '/wholesale/diagnostics#route-sim' },
  ]
}

async function capturePage(page, entry) {
  const url = `${BASE_URL}${entry.path}`
  const result = {
    ...entry,
    url,
    status: 0,
    ok: false,
    file: `${entry.name}.png`,
    redirect: null,
    error: null,
  }
  try {
    let resp = await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 })
    // Hash-only navigations are same-document (no reload), so Alpine x-data —
    // which reads location.hash at init — would not switch tabs. Force a reload
    // for hash-tab pages so the correct tab renders.
    if (entry.path.includes('#')) {
      resp = (await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 })) || resp
    }
    result.status = resp ? resp.status() : result.status
    // Cap the network-idle wait: pages with periodic polling (notifications,
    // active calls) never reach "networkidle", so a long timeout here is the
    // dominant per-page cost and can push the whole walk past the test limit.
    await page.waitForLoadState('networkidle', { timeout: 1500 }).catch(() => {})
    await page.waitForTimeout(600)

    // Human-review capture (always).
    await snap(page, entry.name, { fullPage: true })

    // Automated pixel-diff gate (opt-in, skip dynamic pages).
    if (VISUAL_GATE && !entry.dynamic) {
      await expect.soft(page).toHaveScreenshot(`${entry.name}.png`, {
        fullPage: true,
        maxDiffPixelRatio: 0.02,
        animations: 'disabled',
      })
    }

    result.ok = result.status > 0 && result.status < 400

    // Subnav functional check: the URL-prefix highlight script must mark exactly
    // one pill as active on every wholesale page.
    const activeCount = await page
      .locator('#wholesale-subnav-items [aria-current="page"]')
      .count()
      .catch(() => -1)
    result.subnavActive = activeCount
    if (activeCount !== 1) {
      result.ok = false
      result.error =
        (result.error ? result.error + '; ' : '') +
        `subnav active pills=${activeCount} (expected 1)`
    }
  } catch (e) {
    result.error = e.message.split('\n')[0]
    await snap(page, `${entry.name}-error`, { fullPage: true }).catch(() => {})
    result.file = `${entry.name}-error.png`
  }
  const tag = result.ok ? '✓' : '✗'
  console.log(`  ${tag} ${String(result.status).padEnd(3)} ${entry.name.padEnd(30)} ${entry.path}`)
  return result
}

// Only run when wholesale is enabled.
const describeFn = ENABLED ? test.describe : test.describe.skip
describeFn('Wholesale baseline — screenshot capture', () => {
  test('login, seed entities, capture all wholesale pages', async ({ page }) => {
    // Walking ~29 pages (some with a reload for hash-tabs) needs headroom beyond
    // the 120s default.
    test.setTimeout(180000)
    await consoleLogin(page)
    const context = page.context()
    const headers = await getJsonHeaders(context)

    // Seed one of each entity so detail/edit pages have real ids.
    const seed = await seedWholesaleEntities(context, headers)
    console.log(
      `  seeded: tenant=${seed.tenantId} profile=${seed.profileId} deck=${seed.deckId} trunk=${seed.trunkId}`
    )

    const results = []
    try {
      for (const entry of wholesalePages(seed)) {
        results.push(await capturePage(page, entry))
      }
    } finally {
      // Always clean up, even if a capture threw.
      await cleanupWholesaleEntities(context, headers, seed)
    }

    const indexPath = writeReviewIndex(results, { dir: path.join(SCREENSHOT_DIR) })
    const failed = results.filter((r) => !r.ok)
    console.log(
      `\n  📸 ${results.length} wholesale pages → ${path.relative(process.cwd(), indexPath)}`
    )
    if (failed.length) {
      console.log(`  ⚠ ${failed.length} page(s) had issues: ${failed.map((f) => f.name).join(', ')}`)
    }
  })
})
