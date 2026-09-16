/**
 * Shared helpers for seeding / cleaning up wholesale entities in E2E tests.
 *
 * Both the baseline screenshot spec and the CRUD smoke spec need real entity
 * IDs (tenant / profile / rate-deck / carrier-trunk) to reach detail & edit
 * pages. These helpers create a minimal, self-consistent set over the JSON API
 * and tear it down afterwards so runs are repeatable and leave no residue.
 *
 * NOTE on URL prefixes: wholesale mutation endpoints are registered on the
 * console base path, the /api prefix, or both (see addons/wholesale/mod.rs).
 * `CONSOLE` and `API` below let each call target the prefix the route actually
 * exists on — e.g. carrier-trunk create is base-path-only, rate-deck create is
 * /api-only.
 */

const CONSOLE =
  process.env.CONSOLE_URL || 'http://127.0.0.1:8080/console'
const API = CONSOLE.replace(/\/console\/?$/, '/api')

/**
 * Build JSON headers carrying the session CSRF token (double-submit cookie).
 * Must be called after consoleLogin() so the csrf_token cookie exists.
 */
async function getJsonHeaders(context) {
  const cookies = await context.cookies()
  const csrfToken = (cookies.find((c) => c.name === 'csrf_token') || {}).value || ''
  return { 'Content-Type': 'application/json', 'X-CSRF-Token': csrfToken }
}

/**
 * POST a create payload to `${base}${path}` and return the new entity id.
 * Throws on failure so a broken seed aborts the test loudly instead of
 * producing bogus screenshots.
 */
async function createEntity(context, headers, base, path, body) {
  const resp = await context.request.post(`${base}${path}`, { headers, data: body })
  const text = await resp.text()
  if (resp.status() >= 400) {
    throw new Error(`seed create ${path} failed: HTTP ${resp.status()} ${text}`)
  }
  let json
  try {
    json = JSON.parse(text)
  } catch {
    throw new Error(`seed create ${path} returned non-JSON: ${text}`)
  }
  const id = json.id ?? json.data?.id
  if (!id) throw new Error(`seed create ${path} returned no id: ${text}`)
  return id
}

/** DELETE `${base}${path}`; returns true on success. Never throws. */
async function deleteEntity(context, headers, base, path) {
  try {
    const resp = await context.request.delete(`${base}${path}`, { headers })
    return resp.status() < 400
  } catch {
    return false
  }
}

/**
 * Seed one rate-deck, one carrier trunk, one routing profile and one tenant,
 * wired together (tenant → profile + deck, trunk → deck). Returns all ids.
 */
async function seedWholesaleEntities(context, headers) {
  // Fixed (non-timestamped) names so baseline screenshots are pixel-stable
  // across runs for the VISUAL_GATE toHaveScreenshot comparisons. Safe because
  // each run.sh boots a fresh in-memory sqlite DB and cleanup runs in finally.
  // rate-deck create is registered on the /api prefix only.
  const deckId = await createEntity(context, headers, API, '/wholesale/rate-decks', {
    name: 'e2e-seed-deck',
    type: 'sell',
    description: 'e2e seed deck',
  })

  // carrier-trunk create is registered on the console base path only.
  const trunkId = await createEntity(context, headers, CONSOLE, '/wholesale/trunks', {
    name: 'e2e-seed-trunk',
    sip_server: '127.0.0.1:5060',
    rate_deck_id: deckId,
  })

  const profileId = await createEntity(context, headers, API, '/wholesale/profiles', {
    name: 'e2e-seed-profile',
    description: 'e2e seed profile',
    enable_retry_policy: false,
    max_failover_items: 1,
  })

  const tenantId = await createEntity(context, headers, API, '/wholesale/tenants', {
    name: 'e2e-seed-tenant',
    currency: 'USD',
    credit_limit: 100.0,
    max_concurrent: 10,
    max_cps: 5,
    routing_profile_id: profileId,
    rate_deck_id: deckId,
  })

  return { deckId, trunkId, profileId, tenantId }
}

/** Remove seeded entities in reverse dependency order (best-effort). */
async function cleanupWholesaleEntities(context, headers, seed) {
  if (!seed) return
  if (seed.tenantId) await deleteEntity(context, headers, API, `/wholesale/tenants/${seed.tenantId}/delete`)
  if (seed.profileId) await deleteEntity(context, headers, API, `/wholesale/profiles/${seed.profileId}/delete`)
  if (seed.trunkId) await deleteEntity(context, headers, API, `/wholesale/trunks/${seed.trunkId}/delete`)
  if (seed.deckId) await deleteEntity(context, headers, API, `/wholesale/rate-decks/${seed.deckId}/delete`)
}

module.exports = {
  CONSOLE,
  API,
  getJsonHeaders,
  createEntity,
  deleteEntity,
  seedWholesaleEntities,
  cleanupWholesaleEntities,
}
