# RustPBX E2E Testing

## Running locally

```bash
cargo test-fast                     # unit tests only (lib + workspace members)
cargo test-dev                      # full local suite: --features commerce,wholesale,contact-center
cargo test-compile                  # pre-build every test binary without running
scripts/run_cargo_tests.sh --parallel   # all suites, binaries concurrently (default nproc/2)
scripts/run_cargo_tests.sh --serial     # baseline mode: one binary after another
scripts/run_cargo_tests.sh --parallel 3 # explicit concurrency
scripts/run_cargo_tests.sh --list       # list the test binaries
scripts/check_test_sleeps.sh            # guard: no NEW fixed sleeps >= 100ms in tests
cargo test --test e2e_core -- ringback_mode # single test from the core suite
cargo test --test e2e_core -- rwi:: --nocapture # RWI submodule, full output
```

`run_cargo_tests.sh` builds once via cargo, then invokes the test executables
directly (ports are portpicker-randomized, so binaries run concurrently) and
writes per-suite timing + logs under `target/test-logs/<run_id>/`. On this
machine the full suite went from **251s serial to ~52s parallel**.

- Test binaries (explicit `[[test]]` targets; `autotests = false` in Cargo.toml):
  - `e2e_core` — all non-addon suites (`tests/call.rs`, `rwi.rs`, `ivr_e2e.rs`,
    `proxy_e2e.rs`, `proxy_flow.rs`, `proxy_routing.rs`, `proxy_rwi.rs`,
    `proxy_session.rs`, `proxy_trunk_b2bua.rs`, `queue_e2e.rs`,
    `realtime_bridge.rs`, `common_selftest.rs` included as modules; test paths
    read `e2e_core::<suite>::...`)
  - `e2e_addons` — `cc_e2e` (addon-cc) + `wholesale` (addon-wholesale), each
    module feature-gated
  - `cc_openapi_contract_test` — addon-cc contract test
  The old per-suite binaries were merged to cut link time (~13 full links of
  the crate down to 3 on incremental builds).
- Feature-gated tests are skipped by a bare `cargo test`:
  - `cc_openapi_contract_test.rs` / `cc_e2e.rs` → require `addon-cc`
  - `wholesale.rs` → requires `addon-wholesale`
  Use `cargo test-dev` (`--features addon-cc`) or `cargo test-all`
  (`--features commerce,wholesale,contact-center`) for the full local suite.
- Ports are randomized via `portpicker` (`tests/helpers/test_server.rs`), so tests can
  run concurrently; flaky SIP/RTP tests can be re-run with the same `--test <name>` filter.
- **Waiting for events**: use `tests/common/wait.rs` (`wait_until` /
  `wait_for_value`) or `CdrCapture::wait_for_records` instead of
  `sleep(X); assert(...)`. Fixed sleeps >= 100ms in test code are blocked by
  `scripts/check_test_sleeps.sh` (existing ones are grandfathered in its
  `TEST_SLEEP_BASELINE`); shrink that list as you convert call sites.
- **TestUa events are drain-on-read**: `process_dialog_events()` consumes the
  queue. Use the `next_incoming_call(&mut ua, timeout_ms)` helper and handle
  the returned dialog id immediately — never `wait_for_event`-style poll and
  then re-fetch the batch (a second drain sees nothing; several suites
  silently no-op'd that way for months while still "passing").
- **INVITEs should carry a proper CRLF SDP offer**: SDP-less INVITEs take a
  degraded path (callee notification delayed well past a second), and bare-`\r`
  SDP bodies fail to parse. The UA scenario suites now always send
  `create_test_sdp(...)` offers/answers.
- Coverage (optional): `cargo install cargo-llvm-cov && cargo llvm-cov --features addon-cc`.

## Python E2E (sipbot)

Python + sipbot end-to-end testing for RustPBX. There are two pytest suites:

| Suite | Path | Focus |
|---|---|---|
| **Unified PBX E2E** (recommended) | [`e2e/`](../e2e) | P2P call, queue, IVR, CDR+record, sipflow, voicemail, wholesale, HTTP router |
| **CC e2e-regression** | [`src/addons/cc/e2e-regression/`](../src/addons/cc/e2e-regression) | CC addon: trunk/routing/IVR/queue/ACD/presence/webhook, Playwright widget |

Both suites spawn `sipbot` as a subprocess (the external CLI) and drive a real
`rustpbx` binary via SIP + RWI WebSocket + HTTP REST.

## Unified PBX E2E suite

```bash
cd e2e
python3 -m pip install -r requirements.txt
./run.sh                  # recommended: scenarios (core then wholesale)
./run.sh scenarios        # same as above — separated addon scenarios
./run.sh core             # CC-core file routes: IVR/queue/p2p/... (no wholesale)
./run.sh wholesale        # wholesale billing only (opt-in via set_wholesale)
./run.sh fast             # core, excluding `slow`
./run.sh p2p              # p2p-marked tests (still CC-core addons)
./run.sh -m "queue or ivr"
./run.sh all -- -n 2      # single session; keep `-n 1` when PBX uses fixed ports
```

**Do not** put `wholesale` in `RUSTPBX_E2E_ADDONS` for core/IVR runs.
`WholesaleRouteInvite` replaces default file routing; IVR/queue then fail with
SIP 480 (user offline). Wholesale tests call `ConfigBuilder.set_wholesale()`
themselves — use `./run.sh wholesale` or `./run.sh scenarios`.

Full multi-suite regression (cargo + core + wholesale + CC e2e-regression):

```bash
./scripts/run_full_regression.sh
./scripts/run_full_regression.sh --only core
```

Runs default to `--tb=short --durations=15` and write an HTML report under
`$RUSTPBX_E2E_REPORT_DIR/` when `pytest-html` is installed.

Feature areas (pytest markers): `p2p`, `queue`, `ivr`, `cdr`, `record`,
`sipflow`, `voicemail`, `wholesale`, `http_router`.

Requirements:
1. `rustpbx` built with community addons: `cargo build --features "addon-cc addon-voicemail addon-wholesale"` (the suite also builds it automatically on first run).
2. `sipbot` installed: `cargo install sipbot`.

Env overrides: `RUSTPBX_E2E_ADDONS` (core only; default `cc`), `RUSTPBX_SIP_PORT` (15070),
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
