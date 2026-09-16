/**
 * Screenshot + manifest helpers for human-review screenshot capture.
 */
const path = require('path')
const fs = require('fs')

const SCREENSHOT_DIR = process.env.SCREENSHOT_DIR || path.join(__dirname, '..', '..', 'screenshots', 'current')

function ensureDir(dir) {
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true })
}

/**
 * Capture a named screenshot (viewport-only by default; fullPage via opts).
 */
async function snap(page, name, opts = {}) {
  ensureDir(SCREENSHOT_DIR)
  const p = path.join(SCREENSHOT_DIR, `${name}.png`)
  await page.screenshot({ path: p, fullPage: opts.fullPage !== false })
  return p
}

/**
 * Build a review-friendly index.html grid from a manifest.json.
 */
function writeReviewIndex(results, opts = {}) {
  const dir = opts.dir || SCREENSHOT_DIR
  ensureDir(dir)
  fs.writeFileSync(path.join(dir, 'manifest.json'), JSON.stringify(results, null, 2))

  const rows = results.map((r, i) => {
    const statusColor = r.ok ? '#16a34a' : '#dc2626'
    const badge = r.redirect ? `<span class="badge redirect">→ ${r.redirect}</span>` : ''
    const err = r.error ? `<div class="err">${escapeHtml(r.error)}</div>` : ''
    return `<div class="card">
      <div class="head"><span class="idx">${String(i + 1).padStart(2, '0')}</span>
        <span class="name">${escapeHtml(r.name)}</span>
        <span class="status" style="background:${statusColor}">${r.status || 'ERR'}</span>
        ${badge}</div>
      <img loading="lazy" src="${escapeHtml(r.file)}" onclick="this.requestFullscreen?.()" />
      <div class="path">${escapeHtml(r.path)}</div>
      ${err}
    </div>`
  }).join('\n')

  const html = `<!DOCTYPE html><html><head><meta charset="utf-8">
<title>Console Screenshot Review — ${new Date().toISOString().slice(0, 19)}</title>
<style>
  body{font-family:-apple-system,system-ui,sans-serif;margin:0;background:#0f172a;color:#e2e8f0}
  h1{padding:16px 24px;margin:0;background:#1e293b;font-size:18px}
  .meta{padding:8px 24px;color:#94a3b8;font-size:13px;background:#1e293b}
  .grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(420px,1fr));gap:16px;padding:16px}
  .card{background:#1e293b;border-radius:8px;overflow:hidden;border:1px solid #334155}
  .head{display:flex;align-items:center;gap:8px;padding:8px 12px;font-size:13px}
  .idx{color:#64748b;font-variant-numeric:tabular-nums}
  .name{flex:1;font-weight:600}
  .status{padding:2px 8px;border-radius:4px;font-size:11px;font-weight:700}
  .badge.redirect{background:#2563eb;padding:2px 8px;border-radius:4px;font-size:11px}
  .path{padding:4px 12px 8px;color:#64748b;font-family:monospace;font-size:11px;word-break:break-all}
  img{width:100%;display:block;border-top:1px solid #334155;cursor:zoom-in}
  .err{padding:4px 12px 8px;color:#f87171;font-family:monospace;font-size:11px;white-space:pre-wrap}
</style></head><body>
<h1>Console Page Screenshot Review</h1>
<div class="meta">${results.length} pages · generated ${new Date().toISOString()} · ok: ${results.filter(r => r.ok).length} / fail: ${results.filter(r => !r.ok).length}</div>
<div class="grid">${rows}</div>
</body></html>`

  const indexPath = path.join(dir, 'index.html')
  fs.writeFileSync(indexPath, html)
  return indexPath
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]))
}

module.exports = { snap, writeReviewIndex, SCREENSHOT_DIR, ensureDir }
