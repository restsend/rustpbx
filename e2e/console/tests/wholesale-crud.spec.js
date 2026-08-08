/**
 * ═══════════════════════════════════════════════════════════════
 * Wholesale — CRUD smoke tests for profile / rate-deck / carrier-trunk /
 * tenant-trunk.
 *
 * The existing wholesale.spec.js only covers tenant CRUD. These tests close the
 * gap for the other entities: create via JSON API → assert the row appears on
 * the list page (via data-testid) → delete via JSON API → assert it disappears.
 *
 * They guard the UI refactor: if a template change breaks list rendering or a
 * handler change breaks a create/delete contract, one of these fails.
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const {
  CONSOLE,
  API,
  getJsonHeaders,
  createEntity,
  deleteEntity,
} = require('./helpers/wholesale-seed')

const ENABLED = (process.env.ENABLED_ADDONS || '')
  .split(',')
  .map((s) => s.trim())
  .includes('wholesale')

const describeFn = ENABLED ? test.describe : test.describe.skip

describeFn('Wholesale — CRUD smoke (create → list → delete)', () => {
  test('routing profile CRUD', async ({ page }) => {
    await consoleLogin(page)
    const context = page.context()
    const headers = await getJsonHeaders(context)
    const name = 'e2e-crud-profile-' + Date.now()

    const id = await createEntity(context, headers, API, '/wholesale/profiles', {
      name,
      description: 'crud smoke',
      enable_retry_policy: false,
      max_failover_items: 1,
    })
    expect(id).toBeTruthy()

    await page.goto(`${BASE_URL}/wholesale/profiles`, { waitUntil: 'domcontentloaded' })
    await expect(
      page.locator('[data-testid="profile-row"]').filter({ hasText: name })
    ).toBeVisible()

    const deleted = await deleteEntity(context, headers, API, `/wholesale/profiles/${id}/delete`)
    expect(deleted).toBe(true)

    await page.goto(`${BASE_URL}/wholesale/profiles`, { waitUntil: 'domcontentloaded' })
    await expect(
      page.locator('[data-testid="profile-row"]').filter({ hasText: name })
    ).toHaveCount(0)
  })

  test('rate deck CRUD', async ({ page }) => {
    await consoleLogin(page)
    const context = page.context()
    const headers = await getJsonHeaders(context)
    const name = 'e2e-crud-deck-' + Date.now()

    // rate-deck create is registered on the /api prefix only.
    const id = await createEntity(context, headers, API, '/wholesale/rate-decks', {
      name,
      type: 'sell',
      description: 'crud smoke',
    })
    expect(id).toBeTruthy()

    await page.goto(`${BASE_URL}/wholesale/rate-decks`, { waitUntil: 'domcontentloaded' })
    await expect(
      page.locator('[data-testid="rate-deck-row"]').filter({ hasText: name })
    ).toBeVisible()

    const deleted = await deleteEntity(context, headers, API, `/wholesale/rate-decks/${id}/delete`)
    expect(deleted).toBe(true)

    await page.goto(`${BASE_URL}/wholesale/rate-decks`, { waitUntil: 'domcontentloaded' })
    await expect(
      page.locator('[data-testid="rate-deck-row"]').filter({ hasText: name })
    ).toHaveCount(0)
  })

  test('carrier trunk CRUD', async ({ page }) => {
    await consoleLogin(page)
    const context = page.context()
    const headers = await getJsonHeaders(context)
    const name = 'e2e-crud-trunk-' + Date.now()

    // carrier-trunk create is registered on the console base path only.
    const id = await createEntity(context, headers, CONSOLE, '/wholesale/trunks', {
      name,
      sip_server: '127.0.0.1:5099',
    })
    expect(id).toBeTruthy()

    await page.goto(`${BASE_URL}/wholesale/trunks`, { waitUntil: 'domcontentloaded' })
    await expect(
      page.locator('[data-testid="trunk-row"]').filter({ hasText: name })
    ).toBeVisible()

    const deleted = await deleteEntity(context, headers, API, `/wholesale/trunks/${id}/delete`)
    expect(deleted).toBe(true)

    await page.goto(`${BASE_URL}/wholesale/trunks`, { waitUntil: 'domcontentloaded' })
    await expect(
      page.locator('[data-testid="trunk-row"]').filter({ hasText: name })
    ).toHaveCount(0)
  })

  test('tenant trunk CRUD (nested under tenant)', async ({ page }) => {
    await consoleLogin(page)
    const context = page.context()
    const headers = await getJsonHeaders(context)

    // A tenant trunk needs a parent tenant.
    const tenantName = 'e2e-crud-parent-' + Date.now()
    const tenantId = await createEntity(context, headers, API, '/wholesale/tenants', {
      name: tenantName,
      currency: 'USD',
      credit_limit: 10.0,
      max_concurrent: 1,
      max_cps: 1,
    })
    expect(tenantId).toBeTruthy()

    const trunkName = 'e2e-crud-ttrunk-' + Date.now()
    // tenant-trunk create is registered on the console base path only.
    const trunkId = await createEntity(
      context,
      headers,
      CONSOLE,
      `/wholesale/tenants/${tenantId}/trunks/new`,
      { name: trunkName, ip_acl: '127.0.0.1' }
    )
    expect(trunkId).toBeTruthy()

    // The trunks tab is the default tab on the tenant detail page and is
    // server-rendered, so the new trunk name is visible on first load.
    await page.goto(`${BASE_URL}/wholesale/tenants/${tenantId}`, {
      waitUntil: 'domcontentloaded',
    })
    await expect(page.getByText(trunkName).first()).toBeVisible()

    const deleted = await deleteEntity(
      context,
      headers,
      API,
      `/wholesale/tenants/${tenantId}/trunks/${trunkId}/delete`
    )
    expect(deleted).toBe(true)

    await page.goto(`${BASE_URL}/wholesale/tenants/${tenantId}`, {
      waitUntil: 'domcontentloaded',
    })
    await expect(page.getByText(trunkName)).toHaveCount(0)

    // Clean up the parent tenant.
    await deleteEntity(context, headers, API, `/wholesale/tenants/${tenantId}/delete`)
  })

  test('tenant list search filters via AJAX', async ({ page }) => {
    await consoleLogin(page)
    const context = page.context()
    const headers = await getJsonHeaders(context)

    const suffix = Date.now()
    const nameA = `e2e-search-a-${suffix}`
    const nameB = `e2e-search-b-${suffix}`
    const idA = await createEntity(context, headers, API, '/wholesale/tenants', {
      name: nameA,
      currency: 'USD',
      credit_limit: 1.0,
      max_concurrent: 1,
      max_cps: 1,
    })
    const idB = await createEntity(context, headers, API, '/wholesale/tenants', {
      name: nameB,
      currency: 'USD',
      credit_limit: 1.0,
      max_concurrent: 1,
      max_cps: 1,
    })

    try {
      await page.goto(`${BASE_URL}/wholesale/tenants`, { waitUntil: 'domcontentloaded' })
      await expect(
        page.locator('[data-testid="tenant-row"]').filter({ hasText: nameA })
      ).toBeVisible()
      await expect(
        page.locator('[data-testid="tenant-row"]').filter({ hasText: nameB })
      ).toBeVisible()

      // Searching triggers the debounced AJAX fetchData (format=json).
      const respPromise = page.waitForResponse(
        (r) => r.url().includes('/wholesale/tenants') && r.url().includes('format=json'),
        { timeout: 10000 }
      )
      await page.locator('[data-testid="tenant-search"]').fill(nameA)
      const resp = await respPromise
      expect(resp.status()).toBeLessThan(400)

      await expect(
        page.locator('[data-testid="tenant-row"]').filter({ hasText: nameA })
      ).toBeVisible()
      await expect(
        page.locator('[data-testid="tenant-row"]').filter({ hasText: nameB })
      ).toHaveCount(0)
    } finally {
      await deleteEntity(context, headers, API, `/wholesale/tenants/${idA}/delete`)
      await deleteEntity(context, headers, API, `/wholesale/tenants/${idB}/delete`)
    }
  })
})
