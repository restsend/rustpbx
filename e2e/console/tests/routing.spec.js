/**
 * ═══════════════════════════════════════════════════════════════
 * Routing page — list + create via JSON API
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const { snap } = require('./helpers/screenshot')

test.describe('Routing — list + create', () => {
  test('list renders + create route via JSON', async ({ page }) => {
    await consoleLogin(page)

    // ── List page ──────────────────────────────────────────────
    await page.goto(`${BASE_URL}/routing`, { waitUntil: 'domcontentloaded' })
    await page.waitForTimeout(1000)
    await snap(page, 'routing-list')
    await expect(page.locator('[x-data*="routingConsole"]')).toBeVisible()

    // ── Create form ────────────────────────────────────────────
    await page.goto(`${BASE_URL}/routing/new`, { waitUntil: 'domcontentloaded' })
    await page.waitForTimeout(800)
    await snap(page, 'routing-create-form')

    // Fill the route name (required field).
    const routeName = 'e2e-test-route-' + Date.now()
    await page.locator('[data-testid="route-name"]').fill(routeName)

    // Wait for the JSON PUT submission.
    const responsePromise = page.waitForResponse(
      (r) => r.url().includes('/routing') && r.request().method() === 'PUT',
      { timeout: 15000 }
    )

    await page.locator('button[type="submit"]').click()

    const resp = await responsePromise
    expect(resp.status()).toBeLessThan(400)
    const body = await resp.json().catch(() => ({}))
    expect(body.status === 'ok' || body.id).toBeTruthy()

    await page.waitForTimeout(500)
    await snap(page, 'routing-create-success')
    console.log(`  ✓ route "${routeName}" created via JSON PUT (status ${resp.status()})`)
  })
})
