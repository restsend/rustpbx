/**
 * ═══════════════════════════════════════════════════════════════
 * SIP Trunk page — CRUD + JSON form submission test
 *
 * Verifies the Form→Json migration: the Alpine form submits via apiPut
 * (JSON body) and the handler accepts it. Captures interaction screenshots.
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const { snap } = require('./helpers/screenshot')

test.describe('SIP Trunk — JSON form migration', () => {
  test('list renders + create trunk via JSON submission', async ({ page }) => {
    await consoleLogin(page)

    // ── List page ──────────────────────────────────────────────
    await page.goto(`${BASE_URL}/sip-trunk`, { waitUntil: 'domcontentloaded' })
    await page.waitForTimeout(1000)
    await snap(page, 'sip-trunk-list')
    await expect(page.locator('[x-data*="sipTrunkPage"]')).toBeVisible()

    // ── Create form ────────────────────────────────────────────
    await page.goto(`${BASE_URL}/sip-trunk/new`, { waitUntil: 'domcontentloaded' })
    await page.waitForTimeout(800)
    await snap(page, 'sip-trunk-create-form')

    // Fill required fields.
    await page.locator('[data-testid="trunk-name"]').fill('e2e-test-trunk')
    await page.locator('[data-testid="trunk-display-name"]').fill('E2E Test Trunk')

    // Wait for the JSON PUT submission to the API.
    const responsePromise = page.waitForResponse(
      (r) => r.url().includes('/sip-trunk') && r.request().method() === 'PUT',
      { timeout: 15000 }
    )

    await page.locator('button[type="submit"]').click()

    const resp = await responsePromise
    expect(resp.status()).toBeLessThan(400)
    const body = await resp.json().catch(() => ({}))
    expect(body.status === 'ok' || body.id).toBeTruthy()

    await page.waitForTimeout(500)
    await snap(page, 'sip-trunk-create-success')

    console.log(`  ✓ trunk created via JSON PUT (status ${resp.status()})`)
  })
})
