/**
 * Console login helper.
 *
 * Handles the pre-refactor PRG flow (POST /login form → 303 → dashboard) and
 * will keep working post-refactor since we decided to retain auth POST+Redirect.
 */
const { expect } = require('@playwright/test')

const BASE_URL = process.env.CONSOLE_URL || 'http://127.0.0.1:8080/console'
const DEFAULT_IDENTIFIER = process.env.CONSOLE_IDENTIFIER || 'demo@miuda.ai'
const DEFAULT_PASSWORD = process.env.CONSOLE_PASSWORD || 'hello@miuda.ai'

/**
 * Log into the admin console as the demo superuser.
 * After this resolves, `page` holds the session cookie and is on the dashboard.
 */
async function consoleLogin(page, opts = {}) {
  const baseURL = opts.baseURL || BASE_URL
  const identifier = opts.identifier || DEFAULT_IDENTIFIER
  const password = opts.password || DEFAULT_PASSWORD

  await page.goto(`${baseURL}/login`)
  await page.locator('input[name="identifier"]').fill(identifier)
  await page.locator('input[name="password"]').fill(password)
  await page.locator('button[type="submit"]').click()

  // PRG: POST /login → 303 redirect → GET dashboard.
  // Use domcontentloaded + URL assertion (networkidle is flaky due to polling).
  await page.waitForLoadState('domcontentloaded', { timeout: 15000 })
  await expect(page).not.toHaveURL(/\/login(\?|$)/, { timeout: 15000 })
  return page
}

module.exports = { consoleLogin, BASE_URL, DEFAULT_IDENTIFIER, DEFAULT_PASSWORD }
