const path = require('path')
const os = require('os')
const fs = require('fs')

// Robustly locate a chromium executable across intel/arm64/linux, then fall
// back to letting Playwright auto-resolve (executablePath: undefined).
function findChromium() {
  const candidates = []
  const localBrowsers = path.join(__dirname, 'node_modules', 'playwright-core', '.local-browsers')
  const systemCache = path.join(os.homedir(), 'Library', 'Caches', 'ms-playwright')
  // linux
  candidates.push(path.join(localBrowsers, 'chromium-1223', 'chrome-linux64', 'chrome'))
  // mac intel
  candidates.push(path.join(systemCache, 'chromium-1223', 'chrome-mac', 'Chromium.app', 'Contents', 'MacOS', 'Chromium'))
  // mac arm64
  candidates.push(path.join(systemCache, 'chromium-1223', 'chrome-mac-arm64', 'Chromium.app', 'Contents', 'MacOS', 'Chromium'))
  // headless shell (arm64) — fine for headless runs
  candidates.push(path.join(systemCache, 'chromium_headless_shell-1223', 'chrome-headless-shell-mac-arm64', 'chrome-headless-shell'))
  return candidates.find((c) => fs.existsSync(c))
}
const exePath = findChromium()

/** @type {import('@playwright/test').PlaywrightTestConfig} */
const config = {
  testDir: './tests',
  timeout: 120000,
  retries: 0,
  // Don't fail the whole run on a single page error — we want all screenshots.
  failOnFlake: false,
  use: {
    headless: true,
    viewport: { width: 1600, height: 1000 },
    screenshot: 'on',
    video: 'off',
    trace: 'retain-on-failure',
    permissions: ['microphone'],
    launchOptions: {
      ...(exePath ? { executablePath: exePath } : {}),
      args: [
        '--use-fake-ui-for-media-stream',
        '--use-fake-device-for-media-stream',
        '--allow-http-screen-capture',
        '--no-sandbox',
      ],
    },
  },
  projects: [
    { name: 'chromium', use: { browserName: 'chromium' } },
  ],
}

module.exports = config
