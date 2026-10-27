/**
 * ═══════════════════════════════════════════════════════════════
 * CC console (SPA) — search inputs keep focus across re-render
 *
 * Regression: every CC SPA view rebuilds its whole `.cc-content` DOM on
 * each search keystroke (agents/skill-groups/skills debounce ~300ms, then
 * re-render; outbound-policies re-renders synchronously). The rebuild used
 * to replace the focused <input>, so typing one character and pausing
 * dropped focus and the remaining characters were lost.
 *
 * This test types one character at a time with a pause longer than the
 * debounce, forcing a re-render after every keystroke, and asserts the
 * input stays focused and accumulates the full value.
 *
 * Requires a running PBX with the Contact Center addon (the views live at
 * /console/cc#<tab>). If a view is not present the individual test skips.
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')

const CC_URL = process.env.CC_CONSOLE_URL || `${BASE_URL}/cc`

// Pause between keystrokes — must exceed the 300ms search debounce so a full
// re-render happens after every character.
const KEY_GAP_MS = 450
const TEXT = 'zzq'

const VIEWS = [
  { name: 'agents', hash: '#agents', input: '#agent-search' },
  { name: 'skill-groups', hash: '#skill-groups', input: '#sg-search' },
  { name: 'skills', hash: '#skills', input: '#skill-search' },
  { name: 'outbound-policies', hash: '#outbound-policies', input: '#op-search' },
]

test.describe('CC console search inputs — focus survives re-render', () => {
  test.beforeEach(async ({ page }) => {
    await consoleLogin(page)
  })

  for (const v of VIEWS) {
    test(`${v.name}: ${v.input} keeps focus while typing slowly`, async ({ page }) => {
      await page.goto(`${CC_URL}${v.hash}`, { waitUntil: 'domcontentloaded' })

      const input = page.locator(v.input)
      const present = await input
        .waitFor({ state: 'visible', timeout: 10000 })
        .then(() => true)
        .catch(() => false)
      if (!present) {
        test.skip(true, `CC addon view "${v.name}" not available at ${CC_URL}${v.hash}`)
        return
      }

      await input.click()
      for (const ch of TEXT) {
        await page.keyboard.type(ch)
        // Let the debounce fire and the full view re-render.
        await page.waitForTimeout(KEY_GAP_MS)
      }

      await expect(input, 'search input must retain focus after re-render').toBeFocused()
      await expect(input, 'no typed characters may be lost').toHaveValue(TEXT)
    })
  }
})
