# RustPBX E2E Testing

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
./run.sh p2p              # p2p-marked tests
./run.sh -m "queue or ivr"
```

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
