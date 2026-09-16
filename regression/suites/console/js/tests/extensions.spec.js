/**
 * ═══════════════════════════════════════════════════════════════
 * Extensions page — list + create via JSON API
 *
 * Verifies the extensions Alpine page loads and that creating an
 * extension via the JSON form submission works end-to-end.
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const { snap } = require('./helpers/screenshot')

test.describe('Extensions — list + create', () => {
  test('list renders + create extension via JSON', async ({ page }) => {
    await consoleLogin(page)

    // ── List page ──────────────────────────────────────────────
    await page.goto(`${BASE_URL}/extensions`, { waitUntil: 'domcontentloaded' })
    await page.waitForTimeout(1000)
    await snap(page, 'extensions-list')
    await expect(page.locator('[x-data*="extensionsPage"]')).toBeVisible()

    // ── Create form ────────────────────────────────────────────
    await page.goto(`${BASE_URL}/extensions/new`, { waitUntil: 'domcontentloaded' })
    await page.waitForTimeout(800)
    await snap(page, 'extension-create-form')

    // Fill the extension number (required field).
    const extNum = String(2000 + Math.floor(Math.random() * 7999))
    await page.locator('[data-testid="ext-number"]').fill(extNum)

    // Wait for the JSON PUT submission.
    const responsePromise = page.waitForResponse(
      (r) => r.url().includes('/extensions') && r.request().method() === 'PUT',
      { timeout: 15000 }
    )

    await page.locator('button[type="submit"]').click()

    const resp = await responsePromise
    expect(resp.status()).toBeLessThan(400)
    const body = await resp.json().catch(() => ({}))
    expect(body.status === 'ok' || body.id).toBeTruthy()

    await page.waitForTimeout(500)
    await snap(page, 'extension-create-success')
    console.log(`  ✓ extension ${extNum} created via JSON PUT (status ${resp.status()})`)
  })
})
