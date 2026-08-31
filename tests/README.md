# RustPBX E2E Testing

## Running locally

```bash
cargo test-dev            # full local suite: --features addon-cc
                          #   (covers cc_openapi_contract_test)
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
  - `wholesale.rs` → requires `addon-wholesale`
  Use `cargo test-dev` (`--features addon-cc`) or `cargo test-all`
  (`--features commerce,wholesale,contact-center`) for the full local suite.
- Ports are randomized via `portpicker` (`tests/helpers/test_server.rs`), so tests can
  run concurrently; flaky SIP/RTP tests can be re-run with the same `--test <name>` filter.
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
