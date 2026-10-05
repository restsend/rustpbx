#!/usr/bin/env bash
# Guard: forbid new fixed sleeps in test code that are long enough to be
# "wait for an event" sleeps (the flaky + slow pattern).
#
#   Allowed:  sleep(Duration::from_millis(< 100))  — pacing ticks, not event waits
#   Allowed:  tokio::time::timeout(...)             — bounded waits, not dead time
#   Forbidden: sleep(Duration::from_millis(>= 100)) and any from_secs(...)
#
# Existing violations are grandfathered in TEST_SLEEP_BASELINE (below); the list
# shrinks as Phase-2 refactors land. New occurrences in new/renamed files fail.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SCOPES=(tests src/proxy/tests src/addons/cc/tests src/call)

# Grandfathered files at the time this guard was introduced. Phase-2 refactors
# should shrink this list as sleeps turn into wait_until; new occurrences in
# files NOT listed here fail the check.
TEST_SLEEP_BASELINE=(
  "src/addons/cc/tests/agent_lifecycle_dispatch_tests.rs"
  "src/addons/cc/tests/agent_presence_lifecycle_tests.rs"
  "src/addons/cc/tests/alerting_tests.rs"
  "src/addons/cc/tests/coordination_tests.rs"
  "src/addons/cc/tests/helpers/cdr_capture.rs"
  "src/addons/cc/tests/helpers/e2e_test_server.rs"
  "src/addons/cc/tests/helpers/test_ua.rs"
  "src/call/app/ivr_test.rs"
  "src/call/app/queue_test.rs"
  "src/call/runtime/session_registry.rs"
  "src/proxy/tests/cdr_capture.rs"
  "src/proxy/tests/e2e_test_server.rs"
  "src/proxy/tests/test_auth.rs"
  "src/proxy/tests/test_presence_subscription_leak.rs"
  "src/proxy/tests/test_proxy.rs"
  "src/proxy/tests/test_sip_session_regressions.rs"
  "src/proxy/tests/test_ua.rs"
  "tests/cc_e2e/acd_e2e_test.rs"
  "tests/cc_e2e/queue_alert_e2e.rs"
  "tests/cc_e2e/test_consult_transfer.rs"
  "tests/cc_e2e/test_hold_unhold_e2e.rs"
  "tests/cc_e2e/test_owner_anchor_flows.rs"
  "tests/cc_e2e/webhook_agent_events_e2e_test.rs"
  "tests/call/media_task_leak.rs"
  "tests/common/cdr_capture.rs"
  "tests/common/e2e_test_server.rs"
  "tests/common_selftest/test_ua_tests.rs"
  "tests/proxy_e2e/live_transcript_e2e.rs"
  "tests/proxy_e2e/test_call_e2e.rs"
  "tests/proxy_e2e/test_cluster_home_proxy_e2e.rs"
  "tests/proxy_e2e/test_cluster_session_registry.rs"
  "tests/proxy_e2e/test_early_media_sdp_change_regression.rs"
  "tests/proxy_e2e/test_inbound_refer.rs"
  "tests/proxy_e2e/test_media_e2e.rs"
  "tests/proxy_e2e/test_media_proxy.rs"
  "tests/proxy_e2e/test_network_conference.rs"
  "tests/proxy_e2e/test_proxy_integration.rs"
  "tests/proxy_e2e/test_queue.rs"
  "tests/proxy_e2e/test_rtp_e2e.rs"
  "tests/proxy_e2e/test_security_bans_e2e.rs"
  "tests/proxy_e2e/test_session_hook_e2e.rs"
  "tests/proxy_e2e/test_sip_info_dtmf_e2e.rs"
  "tests/proxy_e2e/test_trunk_b2bua_e2e.rs"
  "tests/proxy_e2e/test_trunk_options.rs"
  "tests/proxy_e2e/webhook_context_e2e.rs"
  "tests/proxy_flow/test_basic_call.rs"
  "tests/proxy_flow/test_busy_wait_e2e.rs"
  "tests/proxy_flow/test_call_error_e2e.rs"
  "tests/proxy_flow/test_dtmf_e2e.rs"
  "tests/proxy_flow/test_hold_e2e.rs"
  "tests/proxy_flow/test_media_commands_e2e.rs"
  "tests/proxy_flow/test_outbound_cancel_e2e.rs"
  "tests/proxy_flow/test_outbound_e2e.rs"
  "tests/proxy_flow/test_presence_e2e.rs"
  "tests/proxy_flow/test_recording_e2e.rs"
  "tests/proxy_flow/test_reinvite_e2e.rs"
  "tests/proxy_flow/test_repro_recording.rs"
  "tests/proxy_flow/test_ringback_e2e.rs"
  "tests/proxy_flow/test_sdes_interop_e2e.rs"
  "tests/proxy_flow/test_transcoding_e2e.rs"
  "tests/proxy_flow/test_video_e2e.rs"
  "tests/proxy_flow/test_webrtc_interop_e2e.rs"
  "tests/proxy_session/test_graceful_shutdown.rs"
  "tests/proxy_session/test_session_hooks.rs"
  "tests/proxy_trunk_b2bua/test_trunk_routing.rs"
  "tests/queue_e2e/test_ivr_queue_agent_full_rwi_e2e.rs"
  "tests/queue_e2e/test_queue_agent_hangup_recording_e2e.rs"
  "tests/queue_e2e/test_queue_bridge_info_e2e.rs"
  "tests/queue_e2e/test_queue_concurrent.rs"
  "tests/queue_e2e/test_queue_escalation_e2e.rs"
  "tests/queue_e2e/test_queue_hold_audio.rs"
  "tests/queue_e2e/test_queue_overflow_uri_override_e2e.rs"
  "tests/queue_e2e/test_queue_routing.rs"
  "tests/queue_e2e/test_queue_transfer_screenpop_headers_e2e.rs"
  "tests/queue_e2e/test_queue_wait_retention_e2e.rs"
  "tests/rwi/comprehensive_event.rs"
  "tests/rwi/integration.rs"
  "tests/rwi/resume_e2e.rs"
  "tests/wholesale/rate_limit_test.rs"
)

violations=0
for scope in "${SCOPES[@]}"; do
  [[ -d "$scope" ]] || continue
  while IFS= read -r -d '' file; do
    # Count sleeps of >= 100ms (millis literal >= 100, or any from_secs).
    found=$(grep -cE 'sleep\(\s*Duration::from_millis\(([1-9][0-9]{2,})\)|sleep\(\s*Duration::from_secs\(' "$file" || true)
    [[ "$found" -eq 0 ]] && continue
    grandfathered=0
    for b in "${TEST_SLEEP_BASELINE[@]}"; do
      [[ "$file" == "$b" ]] && grandfathered=1 && break
    done
    if [[ "$grandfathered" -eq 1 ]]; then
      echo "BASELINED  $found sleeping spots: $file"
    else
      echo "VIOLATION  $found sleeping spots: $file"
      violations=$((violations + 1))
    fi
  done < <(find "$scope" -name '*.rs' -print0)
done

if [[ "$violations" -gt 0 ]]; then
  echo ""
  echo "tests must use tests/common/wait.rs (wait_until/wait_for_value) instead"
  echo "of fixed sleeps >= 100ms. See tests/common/wait.rs docs."
  exit 1
fi
echo "OK: no new fixed sleeps outside the baseline list"
