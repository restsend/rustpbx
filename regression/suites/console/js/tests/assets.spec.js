const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')

test('Reports renders a chart using the shared bundled library', async ({ page }) => {
  await consoleLogin(page)
  const failures = []
  page.on('response', response => {
    if (response.url().includes('chart.umd') && response.status() >= 400) {
      failures.push(`${response.status()} ${response.url()}`)
    }
  })
  await page.goto(`${BASE_URL}/reports`)
  await expect.poll(() => page.evaluate(() => {
    const chart = window.Chart?.getChart('call-chart')
    return Boolean(chart && chart.width > 0 && chart.height > 0)
  })).toBe(true)
  expect(failures).toEqual([])
  const chartUrl = await page.locator('script[src*="chart.umd"]').getAttribute('src')
  expect(new URL(chartUrl, BASE_URL).pathname).toMatch(/\/vendor\/chart\.umd\.min\.js$/)
  expect(chartUrl).not.toContain('/static/cc/')
})

test('The routing modal receives its generated height constraint', async ({ page }) => {
  await consoleLogin(page)
  await page.goto(`${BASE_URL}/routing`)
  await expect(page.locator('[class~="max-h-full"]')).toHaveCSS('max-height', '100%')
})
