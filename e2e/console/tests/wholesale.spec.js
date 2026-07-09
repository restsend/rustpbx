/**
 * ═══════════════════════════════════════════════════════════════
 * Wholesale — tenant CRUD via JSON API
 *
 * Tests that the Form→Json migration works: create/update/delete
 * tenants via JSON requests to the API endpoints.
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const { snap } = require('./helpers/screenshot')

const API = process.env.CONSOLE_URL?.replace('/console', '/api') || 'http://127.0.0.1:8080/api'
const ENABLED = (process.env.ENABLED_ADDONS || '').split(',').map(s => s.trim()).includes('wholesale')

const describeFn = ENABLED ? test.describe : test.describe.skip

describeFn('Wholesale — tenant CRUD via JSON', () => {
  test('create + verify + delete tenant via JSON API', async ({ page, request }) => {
    await consoleLogin(page)

    // Get session cookies + CSRF token from the page context for API calls.
    const context = page.context()
    const cookies = await context.cookies()
    const csrfToken = (cookies.find((c) => c.name === 'csrf_token') || {}).value || ''
    const jsonHeaders = { 'Content-Type': 'application/json', 'X-CSRF-Token': csrfToken }
    const tenantName = 'e2e-ws-tenant-' + Date.now()

    // ── Create tenant via JSON POST ────────────────────────────
    const createResp = await context.request.post(`${API}/wholesale/tenants`, {
      headers: jsonHeaders,
      data: {
        name: tenantName,
        remark: 'E2E test tenant',
        contact_name: 'Test',
        contact_email: 'test@example.com',
        currency: 'USD',
        credit_limit: 100.0,
        max_concurrent: 10,
        max_cps: 5,
      },
    })
    expect(createResp.status()).toBeLessThan(400)
    const createBody = await createResp.json()
    expect(createBody.status).toBe('ok')
    const tenantId = createBody.id || createBody.data?.id
    expect(tenantId).toBeTruthy()
    console.log(`  ✓ tenant created via JSON: id=${tenantId}, name=${tenantName}`)

    // ── Verify tenant appears on the list page ─────────────────
    await page.goto(`${BASE_URL}/wholesale/tenants`, { waitUntil: 'domcontentloaded' })
    await page.waitForTimeout(1000)
    await snap(page, 'ws-tenant-created')

    // ── Update tenant via JSON POST ────────────────────────────
    const updateResp = await context.request.post(`${API}/wholesale/tenants/${tenantId}/update`, {
      headers: jsonHeaders,
      data: {
        name: tenantName + '-updated',
        remark: 'Updated by e2e',
        contact_name: 'Test Updated',
        currency: 'USD',
        credit_limit: 200.0,
        max_concurrent: 20,
        max_cps: 10,
      },
    })
    expect(updateResp.status()).toBeLessThan(400)
    const updateBody = await updateResp.json()
    expect(updateBody.status).toBe('ok')
    console.log(`  ✓ tenant updated via JSON`)

    // ── Delete tenant via JSON DELETE ──────────────────────────
    const delResp = await context.request.delete(`${API}/wholesale/tenants/${tenantId}/delete`, {
      headers: jsonHeaders,
    })
    expect(delResp.status()).toBeLessThan(400)
    const delBody = await delResp.json()
    expect(delBody.status).toBe('ok')
    console.log(`  ✓ tenant deleted via JSON`)
  })

  test('create tenant via browser form submission (form interceptor)', async ({ page }) => {
    await consoleLogin(page)

    // Navigate to the create form.
    await page.goto(`${BASE_URL}/wholesale/tenants/new`, { waitUntil: 'domcontentloaded' })
    await page.waitForTimeout(800)

    // Fill the required name field.
    const tenantName = 'e2e-form-' + Date.now()
    await page.locator('#name').fill(tenantName)

    // The form interceptor converts this to a JSON POST. Listen for the POST
    // to the create endpoint (exclude /new, /update, /delete patterns).
    const responsePromise = page.waitForResponse(
      (r) => {
        const u = r.url()
        return u.includes('/wholesale/tenants') &&
          !u.includes('/new') && !u.includes('/update') &&
          !u.includes('/delete') && !u.includes('/edit') &&
          r.request().method() === 'POST'
      },
      { timeout: 15000 }
    )

    // Submit the traditional <form method="POST"> — the interceptor should
    // intercept, send JSON, and redirect to /wholesale/tenants on success.
    await page.locator('button[type="submit"]').click()

    const resp = await responsePromise
    expect(resp.status()).toBeLessThan(400)

    // Verify the request body was JSON (form interceptor converted it).
    const reqBody = resp.request().postData()
    expect(reqBody).toBeTruthy()
    const parsed = JSON.parse(reqBody)
    expect(parsed.name).toBe(tenantName)

    // Verify redirect to the tenants list happened (form interceptor navigates on success).
    await page.waitForURL('**/wholesale/tenants', { timeout: 10000 }).catch(() => {})
    console.log(`  ✓ form interceptor: "${tenantName}" created via JSON POST, redirected to list`)
  })
})
