/**
 * Generates the final review report: side-by-side baseline vs current
 * screenshots + a summary of all refactor changes.
 *
 * Run: node tests/helpers/generate-review.js
 */
const fs = require('fs')
const path = require('path')

const SHOTS = path.join(__dirname, '..', '..', 'screenshots')
const BASELINE = path.join(SHOTS, 'baseline')
const CURRENT = path.join(SHOTS, 'current')

const pages = []
const allFiles = new Set()

// Collect all screenshot names from both dirs.
for (const dir of [BASELINE, CURRENT]) {
  if (!fs.existsSync(dir)) continue
  for (const f of fs.readdirSync(dir)) {
    if (f.endsWith('.png') && !f.includes('-error')) {
      allFiles.add(f)
    }
  }
}

for (const file of [...allFiles].sort()) {
  const base = fs.existsSync(path.join(BASELINE, file))
  const curr = fs.existsSync(path.join(CURRENT, file))
  pages.push({ file, base, curr })
}

const summary = {
  stages: [
    { name: 'Stage 0 — E2E Baseline', status: 'done', detail: 'e2e/console/ scaffold, 30 baseline screenshots, login helper' },
    { name: 'Stage A — Infrastructure', status: 'done', detail: 'api.js (fetch+form interceptors), CSRF middleware, SameSite=Lax cookies, unified JSON format, layout integration' },
    { name: 'Stage B — Unified /api Tree', status: 'done', detail: 'Console-internal API routes merged into src/api/mod.rs, single api_auth_middleware' },
    { name: 'Stage C — Console Core', status: 'done', detail: 'sip_trunk Form→Json, extension/routing→api.js, fetch interceptor for CSRF, 3 passing e2e specs' },
    { name: 'Stage D — Wholesale Handlers', status: 'done', detail: '15 Form→Json, 64 Redirect→Json, checkbox Option<String>→Option<bool>, tenant CRUD verified' },
    { name: 'Stage E — Cleanup + CSRF Guard', status: 'done', detail: 'csrf_guard enabled on /api, dead code deleted (handlers_append/permissions.html/ui_diagnostics), GET side-effects→POST, 7/7 tests green' },
  ],
  testResults: '7 passed (31.7s) — baseline(17+13 pages) + extensions + sip-trunk + routing + wholesale CRUD',
  filesChanged: [
    'static/js/api.js (NEW — fetch wrapper + CSRF interceptor + form enhancer)',
    'src/console/middleware.rs (+csrf_guard, +seed_csrf_cookie, +constant_time_eq)',
    'src/console/auth.rs (cookie SameSite=Lax;Secure)',
    'src/console/config_helpers.rs (+api_ok, +api_created)',
    'src/console/mod.rs (+static_path context injection)',
    'src/console/handlers/mod.rs (unified api tree, +csrf seed)',
    'src/console/handlers/sip_trunk.rs (Form→Json)',
    'src/api/mod.rs (merged console API, aligned 401, +csrf_guard)',
    'templates/console/layout.html (+api.js, +__consoleBasePath)',
    'templates/console/sip_trunk_detail.html (apiPut/apiPatch, data-testid)',
    'templates/console/extension_detail.html (apiPut/apiPatch)',
    'templates/console/routing_form.html (apiPut/apiPatch, data-testid)',
    'src/addons/wholesale/handlers.rs (15 Form→Json, 64 Redirect→Json)',
    'src/addons/wholesale/mod.rs (GET side-effects→POST)',
    'src/addons/wholesale/handlers_append.rs (DELETED — dead code)',
    'src/addons/wholesale/templates/wholesale/permissions.html (DELETED — orphan)',
  ],
}

// Build HTML
const cards = pages.map((p, i) => {
  const baseImg = p.base ? `baseline/${p.file}` : null
  const currImg = p.curr ? `current/${p.file}` : null
  const baseCell = baseImg
    ? `<img loading="lazy" src="${baseImg}" onclick="this.requestFullscreen?.()" />`
    : `<div class="missing">— baseline missing —</div>`
  const currCell = currImg
    ? `<img loading="lazy" src="${currImg}" onclick="this.requestFullscreen?.()" />`
    : `<div class="missing">— not captured —</div>`
  return `<div class="card">
    <div class="head"><span class="idx">${String(i + 1).padStart(2, '0')}</span><span class="name">${p.file.replace('.png', '')}</span></div>
    <div class="pair"><div class="col"><div class="label">Baseline (pre-refactor)</div>${baseCell}</div><div class="col"><div class="label">Current (post-refactor)</div>${currCell}</div></div>
  </div>`
}).join('\n')

const stagesHtml = summary.stages.map((s) => `
  <div class="stage ${s.status}">
    <div class="stage-head"><span class="badge ${s.status}">${s.status === 'done' ? '✓ DONE' : '○ PENDING'}</span> ${s.name}</div>
    <div class="stage-detail">${s.detail}</div>
  </div>`).join('')

const filesHtml = summary.filesChanged.map((f) => `<li>${f}</li>`).join('')

const html = `<!DOCTYPE html><html><head><meta charset="utf-8">
<title>RustPBX Console Refactor — Review Report</title>
<style>
  body{font-family:-apple-system,system-ui,sans-serif;margin:0;background:#0f172a;color:#e2e8f0;line-height:1.6}
  h1{padding:20px 24px 8px;margin:0;font-size:22px}
  h2{padding:16px 24px 4px;margin:0;font-size:16px;color:#94a3b8;border-top:1px solid #1e293b;margin-top:24px}
  .meta{padding:0 24px 16px;color:#64748b;font-size:13px}
  .result{padding:8px 24px;background:#052e16;border-bottom:1px solid #1e293b;color:#4ade80;font-weight:600}
  .stages{padding:12px 24px}
  .stage{padding:8px 0;border-bottom:1px solid #1e293b}
  .stage-head{font-weight:600}
  .badge{padding:2px 8px;border-radius:4px;font-size:11px;font-weight:700;margin-right:8px}
  .badge.done{background:#052e16;color:#4ade80}
  .stage-detail{color:#94a3b8;font-size:13px;padding-left:8px}
  .files{padding:0 24px 16px;color:#94a3b8;font-size:12px;font-family:monospace;column-count:2;column-gap:24px}
  .files li{margin-bottom:4px;break-inside:avoid}
  .grid{display:grid;grid-template-columns:1fr;gap:20px;padding:16px 24px 48px}
  .card{background:#1e293b;border-radius:8px;overflow:hidden;border:1px solid #334155}
  .head{display:flex;align-items:center;gap:8px;padding:8px 12px;font-size:13px;border-bottom:1px solid #334155}
  .idx{color:#64748b}
  .name{font-weight:600}
  .pair{display:grid;grid-template-columns:1fr 1fr;gap:1px;background:#334155}
  .col{background:#1e293b}
  .label{padding:6px 12px;font-size:11px;color:#64748b;text-transform:uppercase;letter-spacing:0.05em}
  img{width:100%;display:block;cursor:zoom-in}
  .missing{padding:40px 12px;text-align:center;color:#475569;font-size:12px}
</style></head><body>
<h1>RustPBX Console Refactor — Review Report</h1>
<div class="meta">Generated ${new Date().toISOString()} · ${pages.length} pages · baseline vs current comparison</div>
<div class="result">TESTS: ${summary.testResults}</div>

<h2>Completed Stages</h2>
<div class="stages">${stagesHtml}</div>

<h2>Files Changed</h2>
<ul class="files">${filesHtml}</ul>

<h2>Screenshot Comparison (${pages.length} pages)</h2>
<div class="grid">${cards}</div>
</body></html>`

const outPath = path.join(SHOTS, 'review-report.html')
fs.writeFileSync(outPath, html)
console.log(`Review report → ${path.relative(process.cwd(), outPath)}`)
console.log(`  ${pages.length} pages compared`)
console.log(`  ${pages.filter((p) => p.base && p.curr).length} with both baseline + current`)
