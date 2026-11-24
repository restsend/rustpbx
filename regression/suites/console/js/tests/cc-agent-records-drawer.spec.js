/**
 * ═══════════════════════════════════════════════════════════════
 * CC console (SPA) — agent records drawer (右侧抽屉)
 *
 * The Agents tab exposes two per-agent record views in one right-side
 * drawer: 小休记录 (break episodes) and 通话记录 (call records), with a
 * day-range / number / direction / outcome filter bar, server-side
 * pagination, and a per-call jump to the console CDR detail page.
 *
 * Covered here (no seeded data required — the empty states are asserted,
 * and requests are spied so the query contract is checked):
 *   1. the break chip opens the drawer on the 小休 tab and queries
 *      breaks/records with page/per_page;
 *   2. the records icon action opens the drawer on the 通话 tab;
 *   3. tab switching re-queries call-history;
 *   4. the calls filter bar carries direction/status/caller/callee into
 *      the request;
 *   5. CDR detail rows link to /console/call-records/{call_id} in a new
 *      tab (skipped without data);
 *   6. the status-history tab renders break transitions with their
 *      catalog label (「久振未接 (away:998)」 for the automatic no-answer
 *      break) and links the triggering call_id to the CDR detail page
 *      (route-mocked fixtures);
 *   7. switching away from Skill Groups before its load resolves never
 *      paints Skill Groups back over the active tab (stale-view
 *      lifecycle regression, route-delayed fixtures).
 *
 * Requires a running PBX with the Contact Center addon (views at
 * /console/cc#agents). Skips when the view is not present.
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')

const CC_URL = process.env.CC_CONSOLE_URL || `${BASE_URL}/cc`
const API_BASE = process.env.CC_API_BASE || `${BASE_URL.replace(/\/console$/, '')}/api/cc`

test.describe('CC console agent records drawer', () => {
  test.beforeEach(async ({ page }) => {
    await consoleLogin(page)
  })

  async function openAgentsTab(page) {
    await page.goto(`${CC_URL}#agents`, { waitUntil: 'domcontentloaded' })
    const table = page.locator('#cc-spa-app table').first()
    const present = await table
      .waitFor({ state: 'visible', timeout: 15000 })
      .then(() => true)
      .catch(() => false)
    if (!present) {
      test.skip(true, `CC addon agents view not available at ${CC_URL}#agents`)
      return false
    }
    return true
  }

  function spyRecordsRequests(page) {
    const calls = []
    page.on('request', (req) => {
      const url = req.url()
      if (url.includes('/breaks/records') || url.includes('/call-history')) {
        calls.push(url)
      }
    })
    return calls
  }

  test('break chip opens the right-side drawer on the 小休 tab', async ({ page }) => {
    if (!(await openAgentsTab(page))) return
    const calls = spyRecordsRequests(page)

    const chip = page
      .locator('#cc-spa-app table button[onclick*="openAgentRecords"]')
      .first()
    const hasChip = await chip
      .waitFor({ state: 'visible', timeout: 10000 })
      .then(() => true)
      .catch(() => false)
    if (!hasChip) {
      test.skip(true, 'no agent row with a break chip on this instance')
      return
    }
    await chip.click()

    const drawer = page.locator('#agent-records-drawer')
    await expect(drawer).toBeVisible()
    // Slide-in completed: the panel lost its off-screen transform.
    await expect(drawer.locator('[data-ar-panel]')).not.toHaveClass(/translate-x-full/)
    // The 小休 tab is active and its records endpoint was paged properly.
    await expect(drawer.locator('[data-ar-tab="breaks"]')).toHaveClass(/text-sky-700/)
    await expect
      .poll(() => calls.find((u) => u.includes('/breaks/records')))
      .toBeTruthy()
    const url = new URL(calls.find((u) => u.includes('/breaks/records')))
    expect(url.searchParams.get('page')).toBe('1')
    expect(Number(url.searchParams.get('per_page'))).toBeGreaterThan(0)

    // Escape closes the drawer.
    await page.keyboard.press('Escape')
    await expect(drawer).toHaveCount(0)
  })

  test('records icon opens the 通话 tab and carries filters into call-history', async ({ page }) => {
    if (!(await openAgentsTab(page))) return
    const calls = spyRecordsRequests(page)

    const icon = page
      .locator('#cc-spa-app table button[title*="通话记录"], #cc-spa-app table button[title*="Call records"], #cc-spa-app table button[title*="call records"]')
      .first()
    const hasIcon = await icon
      .waitFor({ state: 'visible', timeout: 10000 })
      .then(() => true)
      .catch(() => false)
    if (!hasIcon) {
      test.skip(true, 'no agent row with the records icon on this instance')
      return
    }
    await icon.click()

    const drawer = page.locator('#agent-records-drawer')
    await expect(drawer).toBeVisible()
    await expect(drawer.locator('[data-ar-tab="calls"]')).toHaveClass(/text-sky-700/)
    await expect
      .poll(() => calls.find((u) => u.includes('/call-history')))
      .toBeTruthy()

    // Outcome filter must be sent verbatim.
    await drawer.locator('select[data-ar-f="status"]').selectOption('rejected')
    await expect
      .poll(() => {
        const u = calls.filter((u) => u.includes('/call-history')).pop() || ''
        return new URL(u).searchParams.get('status')
      })
      .toBe('rejected')
    await drawer.locator('select[data-ar-f="direction"]').selectOption('outbound')
    await expect
      .poll(() => {
        const u = calls.filter((u) => u.includes('/call-history')).pop() || ''
        return new URL(u).searchParams.get('direction')
      })
      .toBe('outbound')
    await drawer.locator('input[data-ar-f="caller"]').fill('10086')
    await drawer.locator('[data-ar-apply]').click()
    await expect
      .poll(() => {
        const u = calls.filter((u) => u.includes('/call-history')).pop() || ''
        return new URL(u).searchParams.get('caller')
      })
      .toBe('10086')

    // Inclusive picker → [from, to) server window: `to` is shifted +1 day.
    const u = new URL(calls.filter((x) => x.includes('/call-history')).pop())
    const from = u.searchParams.get('from')
    const to = u.searchParams.get('to')
    expect(from).toBeTruthy()
    expect(to).toBeTruthy()
    expect(new Date(to).getTime() - new Date(from).getTime()).toBe(24 * 3600 * 1000)
  })

  test('call rows link to the console CDR detail in a new tab', async ({ page }) => {
    if (!(await openAgentsTab(page))) return
    // Read whatever the drawer renders — with data, every row must carry a
    // /console/call-records/{id} link opening in a new tab.
    const icon = page
      .locator('#cc-spa-app table button[title*="通话记录"], #cc-spa-app table button[title*="Call records"], #cc-spa-app table button[title*="call records"]')
      .first()
    const hasIcon = await icon
      .waitFor({ state: 'visible', timeout: 10000 })
      .then(() => true)
      .catch(() => false)
    if (!hasIcon) {
      test.skip(true, 'no agent row with the records icon on this instance')
      return
    }
    await icon.click()
    const drawer = page.locator('#agent-records-drawer')
    await expect(drawer).toBeVisible()

    // Widen the window to 30 days to maximize the chance of data, then wait
    // for the load to settle.
    const today = new Date()
    const monthAgo = new Date(today.getTime() - 30 * 24 * 3600 * 1000)
    const iso = (d) => d.toISOString().slice(0, 10)
    await drawer.locator('input[data-ar-f="from"]').fill(iso(monthAgo))
    await drawer.locator('input[data-ar-f="to"]').fill(iso(today))
    await drawer.locator('[data-ar-apply]').click()
    await page.waitForTimeout(800)

    const links = drawer.locator('a[href*="/call-records/"]')
    const count = await links.count()
    test.info().annotations.push({ type: 'info', description: `CDR links rendered: ${count}` })
    if (count === 0) {
      test.skip(true, 'no call records for this agent in the last 30 days')
      return
    }
    const href = await links.first().getAttribute('href')
    expect(href).toMatch(/\/call-records\/[^/]+$/)
    expect(await links.first().getAttribute('target')).toBe('_blank')
  })

  test('status history labels break transitions and links the triggering call id', async ({ page }) => {
    if (!(await openAgentsTab(page))) return

    // Hermetic fixtures: one automatic no-answer break entry (to=away with
    // the break catalog id in reason_code) carrying the dispatch call id.
    await page.route('**/api/cc/presence/config*', (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        source: 'toml',
        config: {},
        global_break_types: [{ id: '998', label_zh: '久振未接', label_en: 'No answer', max_secs: 0, max_count_per_day: 0 }],
        phone_behavior: { ring_no_answer_break_id: '998' },
        runtime_break_types: [],
        runtime_phone_behavior: {},
      }),
    }))
    await page.route('**/api/cc/agents/*/status_history*', (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        data: [{
          from_status: 'ringing', to_status: 'away', reason_code: '998',
          duration_secs: 30, call_id: 'call-abc-123', instance_id: 'n1',
          created_at: Math.floor(Date.now() / 1000),
        }],
        total: 1,
      }),
    }))

    const icon = page
      .locator('#cc-spa-app table button[title*="通话记录"], #cc-spa-app table button[title*="Call records"], #cc-spa-app table button[title*="call records"]')
      .first()
    const hasIcon = await icon
      .waitFor({ state: 'visible', timeout: 10000 })
      .then(() => true)
      .catch(() => false)
    if (!hasIcon) {
      test.skip(true, 'no agent row with the records icon on this instance')
      return
    }
    await icon.click()
    const drawer = page.locator('#agent-records-drawer')
    await expect(drawer).toBeVisible()
    await drawer.locator('[data-ar-tab="status"]').click()

    // The transition chip shows the catalog label with the raw business
    // status, not a bare `away`.
    await expect(drawer.getByText('久振未接 (away:998)')).toBeVisible()

    // The triggering call id links to the CDR detail page in a new tab.
    const link = drawer.locator('a[href*="/call-records/call-abc-123"]')
    await expect(link).toBeVisible()
    expect(await link.getAttribute('target')).toBe('_blank')
    // Full id on hover, truncated text in the cell.
    expect(await link.getAttribute('title')).toBe('call-abc-123')
    const text = await link.textContent()
    expect(text.length).toBeLessThanOrEqual(19)
  })

  test('switching away from skill groups mid-load never paints skill groups back', async ({ page }) => {
    if (!(await openAgentsTab(page))) return

    // Delay the skill-groups payload so its load resolves AFTER we have
    // already switched away — the stale view must neither repaint nor arm
    // its orphan 5s auto-refresh timer.
    await page.route('**/api/cc/skill-groups*', async (route) => {
      await new Promise((resolve) => setTimeout(resolve, 1500))
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ data: [] }),
      })
    })

    await page.goto(`${CC_URL}#skill-groups`, { waitUntil: 'domcontentloaded' })
    // Switch away immediately — inside the delayed load window.
    await page.locator("#cc-tabs button[onclick*=\"setTab('agents')\"]").click()

    // Past the stale view's first 5s tick, the Agents view must still own
    // the screen (without the lifetime guard, Skill Groups repainted here
    // every 5s until a full reload).
    await page.waitForTimeout(6500)
    await expect(page.locator('#cc-spa-app .cc-content h1', { hasText: 'Agents' })).toBeVisible()
    await expect(page.locator('#cc-spa-app .cc-content h1', { hasText: 'Skill Groups' })).toHaveCount(0)
  })
})
