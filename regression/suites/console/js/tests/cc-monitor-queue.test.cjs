const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

// Execute the actual monitor closure, exposing its view functions before DOM startup.
function monitor(snapshot) {
  const template = fs.readFileSync(path.resolve(__dirname, '../../../../../src/addons/cc/templates/contact_center/spa.html'), 'utf8');
  const start = template.indexOf('function MonitorView() {');
  const end = template.indexOf('// ============ Typed Extras Helpers', start);
  const script = template.slice(start, end).replace('\n  render();\n  return {', `
  snapshot = inputSnapshot;
  snapshotAt = Date.now();
  groupDrill = { groupId: 'q1', tab: 'calls', status: '', q: '' };
  return { filteredQueue, drillCallBase, queueMeta, queueStatusTabs, queueToolsHtml, queueRowsHtml,
    pruneBoardIds, queueSelected, setSnapshot(value) { snapshot = value; } };
  return {`);
  return vm.runInNewContext(script + '\nMonitorView();', {
    inputSnapshot: snapshot, window: { ccApp: {} },
    localStorage: { getItem: () => null },
    t: (_, fallback) => fallback, esc: String, ccEsc: String, ccJsAttr: JSON.stringify,
    tf: (_, fallback) => fallback, monSearchInput: () => '', getStatusColor: () => '',
  });
}

function call(call_id, state) {
  return {
    call_id, state, caller: 'caller-test', callee: '1000', direction: 'inbound',
    duration_secs: 60, talk_secs: null, started_at: '2026-10-16T01:00:00Z',
    local: false, node: 'peer',
    route: { queue_id: 'q1', queue_name: 'Support', priority: 7,
      queue_position: state === 'queued' ? 1 : null, queue_wait_secs: 60,
      required_skills: ['support'], overflow: null, dispatch_state: state === 'queued' ? 'waiting' : 'dispatching' },
  };
}

test('queue board and group drilldown keep waiting and ringing calls outside the active-call page', () => {
  // Independent pages let connected calls fill active_calls without hiding ACD calls.
  const waiting = call('acd-waiting', 'queued');
  const ringing = call('acd-ringing', 'ringing');
  const view = monitor({ active_calls: Array.from({ length: 200 }, (_, i) => call('talking-' + i, 'active')),
    acd_calls: [waiting, ringing], queued_calls: [], agents: [], skill_groups: [] });
  assert.deepEqual(Array.from(view.filteredQueue(), row => row.call_id), ['acd-waiting', 'acd-ringing']);
  assert.deepEqual(Array.from(view.drillCallBase(), row => row.call_id), ['acd-waiting', 'acd-ringing']);
  assert.equal(view.queueMeta(view.filteredQueue()[0]).priority, 7);
  assert.match(view.queueStatusTabs(), /Queued.*1/s);
  assert.match(view.queueStatusTabs(), /Ringing.*1/s);
  assert.match(view.queueToolsHtml(), /monQueueLoadMore\(\)" disabled/);
  const rows = view.queueRowsHtml(view.filteredQueue(), true);
  assert.match(rows, /monQueueHangupOne\("acd-waiting"\)/);
  assert.match(rows, /monQueueTransferOne\("acd-ringing"\)/);
  assert.match(rows, /caller-test/);
});

test('monitor still reads ACD calls from snapshots produced by older nodes', () => {
  const view = monitor({ active_calls: [call('old-waiting', 'queued'), call('talking', 'active')] });
  assert.deepEqual(Array.from(view.filteredQueue(), row => row.call_id), ['old-waiting']);
});

test('selection follows the independent ACD page and clears when calls leave the queue', () => {
  const waiting = call('acd-waiting', 'queued');
  const view = monitor({ active_calls: [call('talking-new', 'active')], acd_calls: [waiting] });
  view.queueSelected.add('acd-waiting');
  view.queueSelected.add('talking-new');
  view.pruneBoardIds();
  assert.deepEqual(Array.from(view.queueSelected), ['acd-waiting']);
  view.setSnapshot({ active_calls: [call('acd-waiting', 'active')], acd_calls: [] });
  view.pruneBoardIds();
  assert.equal(view.queueSelected.size, 0);
  assert.deepEqual(Array.from(view.filteredQueue()), []);
});
