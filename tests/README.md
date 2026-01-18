# RustPBX E2E Test Suite

Python-based end-to-end testing for RustPBX call module using sipbot.

## Quick Start

Simply run the test:

```bash
./tests/e2e_call_test.py
# or
python3 tests/e2e_call_test.py
```

**The test automatically:**
- ✅ Starts rustpbx process
- ✅ Checks rustpbx health status
- ✅ Runs all test scenarios
- ✅ Cleans up rustpbx process on exit

**No manual rustpbx management needed!**

## Test Scenarios

| Test   | Scenario                | Verification                           | Duration |
| ------ | ----------------------- | -------------------------------------- | -------- |
| Test 1 | Normal call + recording | 200 OK, bidirectional audio, CDR files | ~25s     |
| Test 2 | Callee not found        | 404/480/408 error response             | ~15s     |
| Test 3 | Caller cancel           | Normal call termination                | ~4s      |
| Test 4 | Media transmission      | Bidirectional RTP statistics           | ~12s     |

**Total runtime**: ~60 seconds

## Prerequisites

### Required

1. **rustpbx built** - `cargo build` to generate `target/debug/rustpbx`
2. **sipbot installed** - `cargo install sipbot`
3. **Config file** - `config.toml.dev` must exist

**No need to manually start rustpbx** - the test script handles it automatically

### Configuration

Your `config.toml.dev` should include:

```toml
[proxy]
user_backends = [
    { type = "memory", users = [
        { username = "alice", password = "123456" },
        { username = "bob", password = "123456" }
    ]}
]

[recording]
enabled = true
path = "config/cdr"
```

## Features

### Core Capabilities

✅ **Fully automated** - Auto start/cleanup rustpbx  
✅ **Pure Python stdlib** - No third-party dependencies  
✅ **Concurrent control** - threading for real-time sipbot output  
✅ **Health checks** - Verify rustpbx port and process status  
✅ **Exception safe** - atexit and signal handling ensure cleanup  
✅ **Detailed reporting** - Specific error messages for each assertion  
✅ **Pre-flight checks** - Auto-detect sipbot and config file

### sipbot --hangup Parameter

The test uses sipbot's `--hangup N` parameter to automatically terminate calls after N seconds:

```python
bob.start_call('bob', DEFAULT_PASSWORD, 
              'sip:alice@127.0.0.1:15060',
              hangup=20)  # Auto-hangup after 20 seconds
```

This is more reliable than manual timing and ensures consistent test duration.

## Test Architecture

```
tests/e2e_call_test.py
├── rustpbx process management
│   ├── Auto-start (start_rustpbx)
│   ├── Health check (check_rustpbx_health)
│   └── Auto-cleanup (cleanup_rustpbx + atexit)
├── subprocess management (SipBotProcess class)
├── Real-time log collection (threading)
├── 4 independent test scenarios
└── Test result statistics (TestResult class)
```

## Example Output

```
============================================================
RustPBX Call Module E2E Tests
============================================================
✓ sipbot is available

============================================================
Starting rustpbx...
============================================================
✓ rustpbx started (PID: 12345)
✓ Logs: tests/logs/rustpbx_e2e.log
⏳ Waiting for rustpbx to be ready.... Ready!
✓ rustpbx is healthy

============================================================
Starting Test 1: Normal Call + Recording
============================================================
[Alice] Starting: sipbot wait...
[Bob] Starting: sipbot call...
[Bob] Call established
[Alice] Sent 400 chunks
✓ CDR: Found 2 JSON + 3 JSONL files

============================================================
✅ PASS Test 1: Normal Call + Recording (25.0s)
============================================================

...

============================================================
TEST SUMMARY
============================================================
✅ Test 1: Normal Call + Recording (25.0s)
✅ Test 2: Callee Not Found (15.0s)
✅ Test 3: Caller Cancel (4.0s)
✅ Test 4: Media Transmission (12.0s)
============================================================
Result: 4/4 tests passed
============================================================

============================================================
Cleaning up rustpbx...
✓ rustpbx stopped gracefully
============================================================
```

## Logs

- **rustpbx logs**: `tests/logs/rustpbx_e2e.log`
- **Test output**: Console or redirect to file

## Troubleshooting

### "rustpbx binary not found"

```bash
# Build rustpbx
cargo build

# Verify
ls -lh target/debug/rustpbx
```

### "Config file not found"

```bash
# Check config file exists
ls -lh config.toml.dev

# Or use a different config
# Edit RUSTPBX_CONFIG in tests/e2e_call_test.py
```

### "rustpbx startup timeout"

**Cause**: rustpbx takes longer than 10 seconds to start or fails to start

**Debug**:
```bash
# Check rustpbx logs
cat tests/logs/rustpbx_e2e.log

# Test manual start
./target/debug/rustpbx --conf config.toml.dev

# Check port availability
lsof -i :15060
```

### "sipbot not found"

```bash
cargo install sipbot
sipbot --version
```

## Customization

### Modify Call Duration

Edit configuration in `tests/e2e_call_test.py`:

```python
CALL_DURATION = 10  # Change to 10 seconds for faster tests
```

### Add New Test

```python
def test_my_scenario() -> TestResult:
    """Test X: My scenario description"""
    result = TestResult("Test X: My Scenario")
    
    with SipBotProcess("Alice") as alice, SipBotProcess("Bob") as bob:
        try:
            # 1. Setup
            alice.start_wait('alice', DEFAULT_PASSWORD, ALICE_PORT)
            time.sleep(SETUP_DELAY)
            
            # 2. Execute
            bob.start_call('bob', DEFAULT_PASSWORD, 
                          f'sip:alice@{PROXY_HOST}:{PROXY_PORT}',
                          hangup=10)
            bob.wait(timeout=20)
            
            # 3. Verify
            bob_output = bob.get_output()
            if 'Expected Pattern' not in bob_output:
                result.add_error("Pattern not found")
                
            result.finish(len(result.errors) == 0)
            
        except Exception as e:
            result.add_error(f"Exception: {e}")
            result.finish(False)
            
    result.print_summary()
    return result

# Add to tests list in main()
tests.append(test_my_scenario)
```

## Configuration Reference

### Global Constants

```python
PROXY_HOST = '127.0.0.1'              # SIP proxy host
PROXY_PORT = 15060                    # SIP proxy port
RUSTPBX_CONFIG = 'config.toml.dev'    # Config file path
RUSTPBX_STARTUP_TIMEOUT = 10          # Startup timeout (seconds)
CALL_DURATION = 20                    # Default call duration
```

### Modify Config File

To use a different config file:

```python
RUSTPBX_CONFIG = 'my_custom_config.toml'
```

## Performance

### Speed up Tests

1. **Reduce call duration**:
   ```python
   CALL_DURATION = 10  # Default: 20
   ```

2. **Reduce setup delay** (if rustpbx responds quickly):
   ```python
   SETUP_DELAY = 1  # Default: 2
   ```

## Best Practices

### ✅ Recommended
- Run tests in a clean environment
- Check logs after failures
- Use for local development and CI/CD
- Verify config before running

### ⚠️ Cautions
- Ensure `cargo build` is run first
- Verify `config.toml.dev` is correct
- Port 15060 must not be in use
- Don't manually kill rustpbx during tests

## Future Enhancements

- [ ] Callee reject 486 (requires sipbot --reject fix)
- [ ] 183 Session Progress + early media
- [ ] REFER transfer tests
- [ ] Concurrent multi-call tests
- [ ] SIP OPTIONS ping tests
- [ ] Re-INVITE tests
- [ ] Test result export (JSON/HTML)
- [ ] CI/CD integration examples
- [ ] Performance metrics (latency, packet loss)
