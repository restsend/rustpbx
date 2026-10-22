const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

// Exercise the shipped Alpine component, replacing only its HTTP/DOM boundaries.
function component(apiGet, extra = {}) {
  const template = fs.readFileSync(path.resolve(__dirname, '../../../../../templates/console/releases.html'), 'utf8');
  const script = template.slice(template.indexOf('function releasesPage()'), template.lastIndexOf('</script>'))
    .replace(/\{\{[\s\S]*?\}\}/g, 'null');
  return vm.runInNewContext(script + '\nreleasesPage()', {
    URLSearchParams, apiGet, window: {}, ...extra,
  });
}
const tick = () => new Promise(resolve => setImmediate(resolve));
const row = id => ({ id, data: { body_preview: 'preview' } });

test('report table sends agent filter and appends deterministic cursor pages', async () => {
  const calls = [];
  const page = component(async url => {
    calls.push(url);
    if (!url.startsWith('/client-reports/list?')) return { data: [] };
    return url.includes('before_id=3') ? { data: [row(2)], next_before_id: null }
      : { data: [row(3)], next_before_id: 3 };
  });
  page.tlAgent = '459280';
  await page.loadTelemetry();
  const first = new URLSearchParams(calls.find(url => url.includes('/list?')).split('?')[1]);
  assert.equal(first.get('agent_id'), '459280');
  assert.equal(page.tlNextBeforeId, 3);
  await page.loadMoreReports();
  assert.deepEqual(Array.from(page.reports, item => item.id), [3, 2]);
  assert.equal(page.tlNextBeforeId, null);
});

test('refresh discards an earlier pending list response and exposes load failures', async () => {
  let resolveOld;
  let listCalls = 0;
  const page = component(async url => {
    if (!url.includes('/list?')) return { data: [] };
    if (++listCalls === 1) return new Promise(resolve => { resolveOld = resolve; });
    if (listCalls === 3) throw new Error('database unavailable');
    return { data: [row(9)], next_before_id: null };
  });
  const old = page.loadTelemetry();
  await tick();
  await page.loadTelemetry();
  resolveOld({ data: [row(1)], next_before_id: 1 });
  await old;
  assert.deepEqual(Array.from(page.reports, item => item.id), [9]);
  await page.loadTelemetry();
  assert.match(page.tlListError, /database unavailable/);
});

test('body pages replace preview then append UTF8 byte offsets until complete', async () => {
  const calls = [];
  const page = component(async url => {
    calls.push(url);
    return calls.length === 1
      ? { text: '中', offset: 0, total_bytes: 6, next_offset: 3, truncated: true }
      : { text: '文', offset: 3, total_bytes: 6, next_offset: null, truncated: false };
  });
  await page.openBody(row(7));
  await page.loadFullBody();
  assert.equal(page.tlBody.text, '中');
  assert.equal(page.tlBody.full, false);
  assert.equal(page.tlBody.nextOffset, 3);
  assert.equal(page.tlBody.loadedBytes, 3);
  await page.loadFullBody();
  assert.equal(new URLSearchParams(calls[1].split('?')[1]).get('offset'), '3');
  assert.equal(page.tlBody.text, '中文');
  assert.equal(page.tlBody.full, true);
  assert.equal(page.tlBody.loadedBytes, 6);
});

test('body requests cannot append twice or overwrite a subsequently opened report', async () => {
  let resolve;
  let calls = 0;
  const page = component(async () => { calls++; return new Promise(done => { resolve = done; }); });
  await page.openBody(row(1));
  const pending = page.loadFullBody();
  const duplicate = page.loadFullBody();
  assert.equal(calls, 1);
  await page.openBody(row(2));
  resolve({ text: 'old body', offset: 0, total_bytes: 8, next_offset: null, truncated: false });
  await Promise.all([pending, duplicate]);
  assert.equal(page.tlBody.id, 2);
  assert.equal(page.tlBody.text, 'preview');
});

test('full download uses authenticated API client and downloads the response blob', async () => {
  const calls = [];
  const blob = { contents: 'complete log' };
  let clicked;
  let revoked;
  const anchor = { click() { clicked = this.download; }, remove() {} };
  const page = component(async url => { calls.push(url); return { blob: async () => blob }; }, {
    document: { createElement: () => anchor, body: { appendChild() {} } },
    URL: { createObjectURL(value) { assert.equal(value, blob); return 'blob:report'; }, revokeObjectURL(value) { revoked = value; } },
    setTimeout(callback) { callback(); },
  });
  await page.openBody(row(7));
  await page.downloadBody();
  assert.equal(calls[0], '/client-reports/body?id=7&download=true');
  assert.equal(clicked, 'client-report-7.txt');
  assert.equal(revoked, 'blob:report');
});


test('legacy partial preview remains visible when complete download is unavailable', async () => {
  const page = component(async url => {
    if (new URLSearchParams(url.split('?')[1]).get('download') === 'true') {
      throw new Error('complete body is unavailable from this peer');
    }
    if (url.startsWith('/client-reports/body?')) {
      return { text: 'available legacy log fragment', truncated: true };
    }
    throw new Error('complete body is unavailable from this peer');
  });
  await page.openBody(row(7));
  await page.loadFullBody();
  assert.equal(page.tlBody.text, 'available legacy log fragment');
  assert.equal(page.tlBody.full, false);
  await page.downloadBody();
  assert.equal(page.tlBody.text, 'available legacy log fragment');
  assert.equal(page.tlBody.full, false);
  assert.match(page.tlBody.error, /complete body is unavailable/);
  assert.equal(page.tlBody.downloading, false);
});
