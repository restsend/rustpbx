# RustPBX E2E Testing

## Running locally

```bash
cargo test-dev            # full local suite: --features addon-cc,addon-sbc
                          #   (covers cc_openapi_contract_test + trunk_health_e2e)
cargo test                # subset only: feature-gated integration tests are skipped
cargo test --test call -- ringback_mode   # single test from the call suite
cargo test --test rwi  -- --nocapture      # RWI suite, full output
```

- Test binaries (each aggregates its same-named subdirectory via `#[path]`):
  `call.rs` (tests/call/), `rwi.rs` (tests/rwi/), `cc_e2e.rs` (tests/cc_e2e/),
  `ivr_e2e.rs` (tests/ivr_e2e/), `proxy_e2e.rs` (tests/proxy_e2e/),
  `proxy_flow.rs` (tests/proxy_flow/), `proxy_routing.rs` (tests/proxy_routing/),
  `proxy_rwi.rs` (tests/proxy_rwi/), `proxy_session.rs` (tests/proxy_session/),
  `proxy_trunk_b2bua.rs` (tests/proxy_trunk_b2bua/), `queue_e2e.rs`
  (tests/queue_e2e/), `wholesale.rs` (tests/wholesale/) plus
  `cc_openapi_contract_test.rs`. Each binary runs its tests in parallel threads.
- Feature-gated tests are skipped by a bare `cargo test`:
  - `cc_openapi_contract_test.rs` / `cc_e2e.rs` → require `addon-cc`
  - `tests/call/trunk_health_e2e.rs` → requires `addon-sbc`
  - `wholesale.rs` → requires `addon-wholesale`
  Use `cargo test-dev` (`--features addon-cc,addon-sbc`) or `cargo test-all`
  (`--features commerce,wholesale,contact-center,addon-sbc`) for the full local suite.
- Ports are randomized via `portpicker` (`tests/helpers/test_server.rs`), so tests can
  run concurrently; flaky SIP/RTP tests can be re-run with the same `--test <name>` filter.
- Coverage (optional): `cargo install cargo-llvm-cov && cargo llvm-cov --features addon-cc,addon-sbc`.

## Python E2E (sipbot)

Python + sipbot end-to-end testing for RustPBX. There are two pytest suites:

| Suite | Path | Focus |
|---|---|---|
| **Unified PBX E2E** (recommended) | [`e2e/`](../e2e) | P2P call, queue, IVR, CDR+record, sipflow, voicemail, wholesale, HTTP router, SBC |
| **CC e2e-regression** | [`src/addons/cc/e2e-regression/`](../src/addons/cc/e2e-regression) | CC addon: trunk/routing/IVR/queue/ACD/presence/webhook, Playwright widget |

Both suites spawn `sipbot` as a subprocess (the external CLI) and drive a real
`rustpbx` binary via SIP + RWI WebSocket + HTTP REST.

## Unified PBX E2E suite

```bash
cd e2e
python3 -m pip install -r requirements.txt
./run.sh                  # all tests
./run.sh fast             # everything except `slow`-marked tests
./run.sh p2p              # p2p-marked tests
./run.sh -m "queue or ivr"
./run.sh all -- -n 2      # forward extra pytest args (e.g. -n for xdist; PBX uses fixed ports so keep `-n 1`)
```
Runs default to `--tb=short --durations=15` and write an HTML report to `$RUSTPBX_E2E_REPORT_DIR/index.html` when `pytest-html` is installed.

Feature areas (pytest markers): `p2p`, `queue`, `ivr`, `cdr`, `record`,
`sipflow`, `voicemail`, `wholesale`, `http_router`, `sbc`.

Requirements:
1. `rustpbx` built with community addons: `cargo build --features "addon-cc addon-sbc addon-voicemail addon-wholesale"` (the suite also builds it automatically on first run).
2. `sipbot` installed: `cargo install sipbot`.

Env overrides: `RUSTPBX_E2E_ADDONS`, `RUSTPBX_SIP_PORT` (15070),
`RUSTPBX_HTTP_PORT` (18080), `RUSTPBX_E2E_REPORT_DIR`.

## CC e2e-regression suite

```bash
cd src/addons/cc/e2e-regression
python3 -m pip install -r requirements.txt
./run.sh {all|tier1|tier2|tier3|fast|playwright}
```

## Notes

- Audio content assertions (sine generation, RMS, dominant frequency, Goertzel)
  use [`e2e/helpers/audio_verifier.py`](../e2e/helpers/audio_verifier.py), a port
  of the removed Rust `tests/helpers/audio_verifier.rs`.
- The legacy standalone scripts (`tests/e2e_call_test.py`, `tests/e2e_ivr_test.py`,
  `tests/e2e_rwi_test.py`) were removed — their scenarios are covered by the
  pytest suites above.
