const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

// Execute the production Alpine component: requests are the only mocked boundary.
function component(fetch) {
  const template = fs.readFileSync(path.resolve(__dirname, '../../../../../templates/console/diagnostics.html'), 'utf8');
  const script = template.slice(template.indexOf("document.addEventListener('alpine:init'"), template.lastIndexOf('</script>'))
    .replace(/\{\{[^}]*\|\s*tojson\s*\}\}/g, '{}')
    .replace(/\{\{[^}]*\}\}/g, '');
  let factory;
  vm.runInNewContext(script, {
    document: { addEventListener: (_, callback) => callback() },
    Alpine: { data: (_, callback) => { factory = callback; } },
    URLSearchParams, fetch, crypto: { randomUUID: () => 'recovery-test-request' },
  });
  const state = factory({ data: {} });
  state.apiBase = '/api';
  return state;
}

const snapshot = {
  diagnostic_id: 'diagnostic-1', collected_at: '2026-10-16T01:00:00Z', complete: false,
  summary: { abnormal: 1, suspected: 0, unknown: 1, no_detected_issue: 0 },
  nodes: [{ node_id: 'agent-2', status: 'unreachable', error: 'timeout' }],
  groups: [], agents: [], sessions: [], errors: [],
};

test('agent search sends exact filters without requiring session_id and preserves incomplete evidence', async () => {
  let requested;
  const state = component(async (url) => {
    requested = url;
    return { ok: true, json: async () => ({ data: snapshot }) };
  });
  state.receptionAgentId = ' 489220 ';
  state.receptionSkillGroupId = ' G/1 ';
  await state.diagnoseReception();
  const url = new URL(requested, 'http://localhost');
  assert.equal(url.pathname, '/api/cc/reception/diagnose');
  assert.equal(url.searchParams.get('agent_id'), '489220');
  assert.equal(url.searchParams.get('skill_group_id'), 'G/1');
  assert.equal(url.searchParams.has('session_id'), false);
  assert.equal(state.receptionResult.complete, false);
  assert.equal(state.receptionResult.nodes[0].error, 'timeout');
});

test('a failed new scan clears stale evidence instead of presenting it as current', async () => {
  const state = component(async () => ({ ok: false, status: 503, json: async () => ({ error: 'shared database unavailable' }) }));
  state.receptionResult = snapshot;
  await state.diagnoseReception();
  assert.equal(state.receptionResult, null);
  assert.match(state.receptionError, /shared database unavailable/);
});

test('single-session recovery suppresses duplicate submission and treats transport failure as unknown', async () => {
  let rejectRequest;
  const requests = [];
  const state = component((url, options) => {
    requests.push({ url, body: JSON.parse(options.body) });
    return new Promise((_, reject) => { rejectRequest = reject; });
  });
  state.receptionResult = { complete: true };
  const target = { session_id: 'full-session-id', skill_group_id: 'G/1', recoverable: true };
  const first = state.recoverReception(target);
  await state.recoverReception(target);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].body.session_id, target.session_id);
  assert.equal(requests[0].body.request_id, 'recovery-test-request');
  rejectRequest(new Error('connection lost'));
  await first;
  assert.equal(state.receptionRecoveryResults[target.session_id].status, 'unknown');
  assert.equal(state.receptionRecoveryRequests[target.session_id], 'recovery-test-request');
});

test('a target without explicit recoverability never sends a mutation', async () => {
  let mutations = 0;
  const state = component(async () => { mutations++; });
  await state.recoverReception({ session_id: 'normal-call', recoverable: false });
  assert.equal(mutations, 0);
});

test('skill groups keep per-call faults separate and never omit unassigned evidence', () => {
  const state = component(async () => {});
  state.receptionResult = {
    groups: [{ skill_group_id: 'G1', agent_ids: ['489220'], session_ids: ['stalled'] },
      { skill_group_id: 'G2', agent_ids: ['489220'], session_ids: [] }],
    agents: [{ agent_id: '489220', classification: 'normal', session_ids: ['stalled'] },
      { agent_id: 'unassigned-agent', classification: 'unknown', session_ids: [] }],
    sessions: [{ session_id: 'stalled', recoverable: true }, { session_id: 'unassigned-call', recoverable: false }],
  };
  const groups = state.receptionGroups();
  assert.equal(groups.length, 3);
  assert.equal(state.receptionGroupAgents(groups[0])[0].classification, 'normal');
  assert.equal(state.receptionGroupSessions(groups[2])[0].session_id, 'unassigned-call');
  assert.equal(state.receptionGroupAgents(groups[2])[0].agent_id, 'unassigned-agent');
});

test('recovery reports actual actor outcome, never equates accepted delivery with ringing', async () => {
  const state = component(async () => ({ ok: true, json: async () => ({ data: { outcome: 'dial_requested', reason: 'dispatch_stalled' } }) }));
  state.receptionResult = { complete: true };
  await state.recoverReception({ session_id: 'stalled', recoverable: true });
  assert.equal(state.receptionRecoveryResults.stalled.status, 'dial_requested');
  assert.match(state.receptionRecoveryResults.stalled.message, /振铃待确认/);
});

test('fresh diagnosis retries uncertain delivery with the same key, but gives a new proven stall a new operation', async () => {
  const current = { ...snapshot, agents: [], sessions: [{ session_id: 'uncertain', recoverable: true }, { session_id: 'completed', recoverable: true }, { session_id: 'shared', recoverable: false, can_verify_recovery: true }] };
  const state = component(async () => ({ ok: true, json: async () => ({ data: current }) }));
  state.receptionRecoveryRequests = { uncertain: 'keep-existing-request', completed: 'previous-completed-request', shared: 'keep-shared-request' };
  state.receptionRecoveryResults = { uncertain: { status: 'unknown' }, completed: { status: 'dial_requested' }, shared: { status: 'unknown' } };
  await state.diagnoseReception();
  assert.equal(state.receptionRecoveryRequests.uncertain, 'keep-existing-request');
  assert.equal(state.receptionRecoveryRequests.completed, undefined);
  assert.equal(state.receptionRecoveryResults.uncertain, undefined);
  assert.equal(state.receptionRecoveryResults.completed, undefined);
  assert.equal(state.receptionRecoveryRequests.shared, 'keep-shared-request');
  assert.equal(state.receptionRecoveryResults.shared, undefined);
});

test('an incomplete global scan can request independent target verification without claiming recovery', async () => {
  const requests = [];
  const state = component(async (url, options) => {
    requests.push(JSON.parse(options.body));
    return { ok: false, status: 503, json: async () => ({ modified: false, message: 'Target owner evidence incomplete' }) };
  });
  state.receptionResult = { complete: false };
  await state.recoverReception({ session_id: 'observed-target', recoverable: true });
  assert.equal(requests.length, 1);
  assert.equal(requests[0].session_id, 'observed-target');
  assert.equal(state.receptionRecoveryResults['observed-target'].status, 'unknown');
  assert.doesNotMatch(state.receptionRecoveryResults['observed-target'].message, /已恢复|已振铃/);
});

test('a manual break with a reason is displayed as a break', () => {
  const state = component(async () => {});
  assert.equal(state.receptionLabel('away:lunch'), '小休（lunch）');
});

test('a shared queue row outside scan coverage can request strict target owner verification', async () => {
  const requests = [];
  const state = component(async (url, options) => {
    requests.push(JSON.parse(options.body));
    return { ok: false, status: 503, json: async () => ({ modified: false, message: 'Target node unavailable' }) };
  });
  state.receptionResult = { complete: false };
  await state.recoverReception({ session_id: 'shared-waiter', recoverable: false, can_verify_recovery: true });
  assert.equal(requests.length, 1);
  assert.equal(requests[0].session_id, 'shared-waiter');
  assert.equal(state.receptionRecoveryResults['shared-waiter'].status, 'unknown');
});

test('owner-confirmed wrapup describes the previous call without inventing a current binding', () => {
  const state = component(async () => {});
  const wrapup = { status: 'wrapup', binding_evidence: 'owner_runtime_verified', wrapup_session_id: 'completed-full-call-id', bound_session_id: null };
  assert.match(state.receptionAgentBinding(wrapup), /话后关联通话：completed-full-call-id/);
  assert.match(state.receptionAgentBinding(wrapup), /非当前通话绑定/);
  assert.match(state.receptionAgentBinding({ ...wrapup, wrapup_session_id: null }), /所属节点已核实话后/);
  assert.match(state.receptionAgentBinding({ status: 'wrapup' }), /尚未核实所属节点/);
  assert.equal(state.receptionAgentBinding({ status: 'busy', bound_session_id: 'active-full-call-id' }), '绑定会话：active-full-call-id');
  assert.match(state.receptionAgentBinding({ status: 'busy' }), /不能用历史通话推断/);
});


test('known business states have concrete conclusions and an explicitly searched normal agent remains visible', () => {
  const state = component(async () => {});
  assert.equal(state.receptionAgentConclusion({ classification: 'normal', status: 'wrapup' }), '话后处理中');
  assert.equal(state.receptionAgentConclusion({ classification: 'normal', status: 'busy' }), '忙碌');
  assert.equal(state.receptionAgentConclusion({ classification: 'normal', status: 'ringing' }), '振铃中');
  assert.equal(state.receptionAgentConclusion({ classification: 'unknown', status: 'wrapup', reason: 'owner_presence_conflict' }), '状态证据不一致，需刷新核验');
  assert.equal(state.receptionAgentConclusion({ classification: 'abnormal', status: 'idle' }), '已确认状态异常');
  state.receptionAgentId = '489220';
  state.receptionResult = { agents: [{ agent_id: '489220', classification: 'normal', status: 'wrapup', session_ids: [] }], sessions: [] };
  assert.equal(state.receptionGroupAgents({ agent_ids: ['489220'] }).length, 1);
});
