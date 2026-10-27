/**
 * ═══════════════════════════════════════════════════════════════
 * Outbound Policies console UI — apply-all / invisible round-trip.
 *
 * Drives the contact-center SPA (#/outbound-policies) end to end:
 * create a policy with "Apply to all agents" + "Invisible" checked,
 * verify the list renders the All-agents chip and the invisible badge,
 * then re-open the editor and verify both checkboxes round-trip from
 * the API payload (serde emits snake_case `apply_all` — the UI must
 * read both spellings).
 *
 * Requires a CC-enabled PBX with demo_mode on (boot one via
 * regression/scripts/boot_cc_console_ui.py) and CONSOLE_URL set.
 * ═══════════════════════════════════════════════════════════════
 */
const { test, expect } = require('@playwright/test')
const { consoleLogin, BASE_URL } = require('./helpers/login')
const { snap } = require('./helpers/screenshot')

const POLICY_NAME = `ui-verify-${Date.now()}`

test.describe.configure({ mode: 'serial' })

test('outbound policy apply-all + invisible UI round-trip', async ({ page }) => {
  test.setTimeout(120000)
  // Auto-accept the delete confirmation dialog.
  page.on('dialog', (d) => d.accept())

  await consoleLogin(page)

  // ── 1. Open the Outbound Policies view ──────────────────────
  // (SPA hash routing: `#outbound-policies`, no leading slash.)
  await page.goto(`${BASE_URL}/cc#outbound-policies`)
  await expect(page.getByRole('button', { name: 'Add Policy' })).toBeVisible({ timeout: 20000 })

  // ── 2. The editor exposes the two new toggles ───────────────
  await page.getByRole('button', { name: 'Add Policy' }).click()
  const modal = page.locator('#outbound-policy-modal')
  await expect(modal).toBeVisible()
  const applyAll = modal.locator('input[name="applyAll"]')
  const invisible = modal.locator('input[name="invisible"]')
  await expect(applyAll).toBeVisible()
  await expect(invisible).toBeVisible()
  // Defaults: off.
  await expect(applyAll).not.toBeChecked()
  await expect(invisible).not.toBeChecked()
  await snap(page, 'cc-outbound-policy-modal-toggles', { fullPage: false })

  // ── 3. Create a policy with both flags + one rule ───────────
  await modal.locator('input[name="name"]').fill(POLICY_NAME)
  await applyAll.check()
  await invisible.check()
  await modal.getByRole('button', { name: '+ Add rule (line)' }).click()
  const card = modal.locator('#op-lines .rounded-xl').first()
  await card.locator('[data-f="label"]').fill('UI verify')
  await card.locator('[data-f="prefix"]').fill('9')
  await snap(page, 'cc-outbound-policy-create-filled', { fullPage: false })
  await modal.getByRole('button', { name: 'Create' }).click()
  await expect(modal).toBeHidden({ timeout: 15000 })

  // ── 4. The list renders the All-agents chip + invisible badge ──
  const row = page.locator('tr', { hasText: POLICY_NAME })
  await expect(row).toBeVisible({ timeout: 15000 })
  await expect(row.getByText('All agents')).toBeVisible()
  await expect(row.getByText('invisible', { exact: true })).toBeVisible()
  await snap(page, 'cc-outbound-policy-list-badges', { fullPage: false })

  // ── 5. Round-trip: the editor prefills both flags from the API ──
  await row.getByRole('button', { name: 'Edit' }).click()
  const editModal = page.locator('#outbound-policy-modal')
  await expect(editModal).toBeVisible()
  await expect(editModal.locator('input[name="applyAll"]')).toBeChecked({ timeout: 15000 })
  await expect(editModal.locator('input[name="invisible"]')).toBeChecked()
  // Rule survived too.
  await expect(editModal.locator('[data-f="prefix"]').first()).toHaveValue('9')
  await snap(page, 'cc-outbound-policy-edit-roundtrip', { fullPage: false })
  await editModal.getByRole('button', { name: 'Cancel' }).click()

  // ── 6. Cleanup (the row's delete button is icon-only; drive the app API) ──
  await page.evaluate((name) => window.ccApp.deleteOutboundPolicy(name), POLICY_NAME)
  await expect(page.locator('tr', { hasText: POLICY_NAME })).toBeHidden({ timeout: 15000 })
})
