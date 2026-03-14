#!/usr/bin/env python3
"""
RustPBX IVR Module E2E Tests
End-to-end testing for IVR using sipbot with real SIP calls and RTP audio

Usage:
    python tests/e2e_ivr_test.py

Prerequisites:
    1. Build rustpbx: cargo build
    2. Install sipbot: cargo install sipbot
    3. Ensure config.toml.dev exists
"""

import subprocess
import threading
import time
import re
import sys
import atexit
import signal
from pathlib import Path
from typing import Optional, Tuple

# Configuration
PROXY_HOST = '127.0.0.1'
PROXY_PORT = 15061  # Match config.toml.dev
IVR_NUMBER = '1100'  # IVR test number (configured in config/routes/ivr_test.toml)
DEFAULT_PASSWORD = '123456'
CALL_DURATION = 15  # Call duration in seconds
SETUP_DELAY = 2     # Delay for registration completion
RUSTPBX_CONFIG = 'config.toml.dev'
RUSTPBX_STARTUP_TIMEOUT = 10  # rustpbx startup wait time in seconds

# Global rustpbx process
_rustpbx_process: Optional[subprocess.Popen] = None
_rustpbx_output_file = None


class SipBotProcess:
    """sipbot process manager"""

    def __init__(self, name: str):
        self.name = name
        self.process: Optional[subprocess.Popen] = None
        self.output = []
        self.output_lock = threading.Lock()
        self.reader_thread: Optional[threading.Thread] = None

    def start_call(self, username: str, password: str, target: str, hangup: int = CALL_DURATION):
        """Start sipbot to make a call"""
        cmd = [
            'sipbot', 'call',
            '-t', target,
            '--username', username,
            '--password', password,
            '--codecs', 'pcmu',
            '--hangup', str(hangup),
            '-v'
        ]

        print(f"[{self.name}] Starting: {' '.join(cmd)}")
        self.process = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )

        # Start output reader thread
        self.reader_thread = threading.Thread(target=self._read_output, daemon=True)
        self.reader_thread.start()

    def _read_output(self):
        """Continuously read process output"""
        try:
            for line in self.process.stdout:
                line = line.rstrip()
                if line:
                    with self.output_lock:
                        self.output.append(line)
                    print(f"[{self.name}] {line}")
        except Exception as e:
            print(f"[{self.name}] Reader error: {e}")

    def get_output(self) -> str:
        """Get all output"""
        with self.output_lock:
            return '\n'.join(self.output)

    def wait(self, timeout: int = 60) -> int:
        """Wait for process to finish"""
        if self.process:
            try:
                return self.process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                print(f"[{self.name}] Timeout expired, terminating...")
                self.terminate()
                return -1
        return 0

    def terminate(self):
        """Terminate the process"""
        if self.process and self.process.poll() is None:
            print(f"[{self.name}] Terminating...")
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                print(f"[{self.name}] Force killing...")
                self.process.kill()

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.terminate()


class TestResult:
    """Test result tracker"""
    def __init__(self, name: str):
        self.name = name
        self.passed = False
        self.errors = []
        self.start_time = time.time()
        self.end_time = None

    def add_error(self, msg: str):
        self.errors.append(msg)

    def finish(self, passed: bool):
        self.passed = passed
        self.end_time = time.time()

    def duration(self) -> float:
        if self.end_time:
            return self.end_time - self.start_time
        return time.time() - self.start_time

    def print_summary(self):
        status = "✅ PASS" if self.passed else "❌ FAIL"
        print(f"\n{'='*60}")
        print(f"{status} {self.name} ({self.duration():.1f}s)")
        if self.errors:
            print("Errors:")
            for err in self.errors:
                print(f"  - {err}")
        print('='*60)


def cleanup_rustpbx():
    """Clean up rustpbx process"""
    global _rustpbx_process, _rustpbx_output_file

    if _rustpbx_process and _rustpbx_process.poll() is None:
        print("\n" + "="*60)
        print("Cleaning up rustpbx...")
        try:
            _rustpbx_process.terminate()
            _rustpbx_process.wait(timeout=5)
            print("✓ rustpbx stopped gracefully")
        except subprocess.TimeoutExpired:
            print("⚠ rustpbx not responding, force killing...")
            _rustpbx_process.kill()
            _rustpbx_process.wait()
            print("✓ rustpbx killed")
        except Exception as e:
            print(f"⚠ Error stopping rustpbx: {e}")

    if _rustpbx_output_file:
        try:
            _rustpbx_output_file.close()
        except:
            pass


def start_rustpbx() -> bool:
    """Start rustpbx process"""
    global _rustpbx_process, _rustpbx_output_file

    print("\n" + "="*60)
    print("Starting rustpbx...")
    print("="*60)

    # Check config file
    config_path = Path(RUSTPBX_CONFIG)
    if not config_path.exists():
        print(f"❌ Config file not found: {RUSTPBX_CONFIG}")
        return False

    # Check rustpbx executable
    rustpbx_bin = Path('target/debug/rustpbx')
    if not rustpbx_bin.exists():
        print(f"❌ rustpbx binary not found: {rustpbx_bin}")
        print("   Please build first: cargo build")
        return False

    # Check IVR config files
    ivr_route = Path('config/routes/ivr_test.toml')
    ivr_config = Path('config/ivr/test_e2e.toml')
    if not ivr_route.exists():
        print(f"❌ IVR route config not found: {ivr_route}")
        return False
    if not ivr_config.exists():
        print(f"❌ IVR config not found: {ivr_config}")
        return False

    print(f"✓ IVR route config: {ivr_route}")
    print(f"✓ IVR config: {ivr_config}")

    # Create log directory
    log_dir = Path('tests/logs')
    log_dir.mkdir(parents=True, exist_ok=True)
    log_file = log_dir / 'rustpbx_ivr_e2e.log'

    try:
        # Start rustpbx
        _rustpbx_output_file = open(log_file, 'w')
        _rustpbx_process = subprocess.Popen(
            [str(rustpbx_bin), '--conf', RUSTPBX_CONFIG],
            stdout=_rustpbx_output_file,
            stderr=subprocess.STDOUT,
            preexec_fn=None if sys.platform == 'win32' else lambda: signal.signal(signal.SIGINT, signal.SIG_IGN)
        )

        print(f"✓ rustpbx started (PID: {_rustpbx_process.pid})")
        print(f"✓ Logs: {log_file}")

        # Wait for rustpbx to be ready
        print(f"⏳ Waiting for rustpbx to be ready...", end="", flush=True)
        for i in range(RUSTPBX_STARTUP_TIMEOUT):
            time.sleep(1)
            print(".", end="", flush=True)

            # Check if process is still alive
            if _rustpbx_process.poll() is not None:
                print("\n❌ rustpbx exited unexpectedly")
                print(f"   Check logs: {log_file}")
                return False

            # Check if port is available
            if check_rustpbx_port():
                print(" Ready!")
                return True

        print("\n❌ rustpbx startup timeout")
        print(f"   Check logs: {log_file}")
        cleanup_rustpbx()
        return False

    except Exception as e:
        print(f"\n❌ Failed to start rustpbx: {e}")
        cleanup_rustpbx()
        return False


def check_rustpbx_port() -> bool:
    """Check if rustpbx port is available"""
    try:
        import socket
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.settimeout(0.5)
        # Try to send a UDP packet
        sock.sendto(b"OPTIONS sip:ping@127.0.0.1 SIP/2.0\r\n\r\n",
                   (PROXY_HOST, PROXY_PORT))
        sock.close()
        return True
    except Exception:
        return False


def check_rustpbx_health() -> Tuple[bool, str]:
    """Check rustpbx health status"""
    global _rustpbx_process

    # Check if process is still running
    if not _rustpbx_process or _rustpbx_process.poll() is not None:
        return False, "rustpbx process is not running"

    # Check port
    if not check_rustpbx_port():
        return False, f"rustpbx port {PROXY_PORT} is not responding"

    return True, "rustpbx is healthy"


def check_cdr_files() -> Tuple[bool, str]:
    """Check if CDR files are generated"""
    cdr_dir = Path('config/cdr')
    if not cdr_dir.exists():
        return False, "CDR directory not found"

    # Find latest CDR files
    json_files = list(cdr_dir.rglob('*.json'))
    jsonl_files = list(cdr_dir.rglob('*.jsonl'))

    if not json_files and not jsonl_files:
        return False, "No CDR files found"

    return True, f"Found {len(json_files)} JSON + {len(jsonl_files)} JSONL files"


def test_ivr_basic_call() -> TestResult:
    """Test 1: Basic IVR call - SIP call enters IVR and receives RTP audio"""
    result = TestResult("Test 1: IVR Basic Call + RTP Audio")

    print("\n" + "="*60)
    print("Starting Test 1: IVR Basic Call + RTP Audio")
    print(f"  Calling IVR number: {IVR_NUMBER}")
    print("="*60)

    with SipBotProcess("Caller") as caller:
        try:
            # Caller calls the IVR number (1100)
            # IVR should auto-answer and play greeting audio
            caller.start_call('bob', DEFAULT_PASSWORD,
                             f'sip:{IVR_NUMBER}@{PROXY_HOST}:{PROXY_PORT}',
                             hangup=CALL_DURATION)

            # Wait for call to complete
            caller.wait(timeout=CALL_DURATION + 10)

            # Analyze output
            caller_output = caller.get_output()

            # Verify call was established (IVR auto-answers)
            if '200 OK' not in caller_output:
                result.add_error("Caller did not receive 200 OK from IVR")

            if 'Call established' not in caller_output:
                result.add_error("Call to IVR not established")

            # Check for RTP reception (RX: packets/bytes) - this is IVR audio
            rx_stats = re.search(r'RX: (\d+)p/\d+b', caller_output)
            if not rx_stats:
                result.add_error("Caller did not receive IVR audio (no RX packets)")
            else:
                packets = int(rx_stats.group(1))
                # IVR greeting is typically a few seconds, expect at least 50 packets
                # (assuming 20ms per packet, 5 seconds = 250 packets, but we're lenient)
                if packets < 50:
                    result.add_error(f"Caller received too few RTP packets from IVR: {packets}")
                else:
                    print(f"✓ Caller received {packets} RTP packets from IVR (greeting audio)")

            # Check for RTP transmission (TX: packets/bytes) - caller's audio
            tx_stats = re.search(r'TX: (\d+)p/\d+b', caller_output)
            if not tx_stats:
                result.add_error("Caller did not send audio (no TX packets)")
            else:
                print(f"✓ Caller sent {tx_stats.group(1)} audio packets")

            # Verify CDR
            time.sleep(2)  # Wait for CDR to be written
            cdr_ok, cdr_msg = check_cdr_files()
            if not cdr_ok:
                result.add_error(f"CDR check failed: {cdr_msg}")
            else:
                print(f"✓ CDR: {cdr_msg}")

            result.finish(len(result.errors) == 0)

        except Exception as e:
            result.add_error(f"Exception: {e}")
            result.finish(False)

    result.print_summary()
    return result


def test_ivr_quick_hangup() -> TestResult:
    """Test 2: Quick hangup - caller hangs up while IVR is playing"""
    result = TestResult("Test 2: IVR Quick Hangup")

    print("\n" + "="*60)
    print("Starting Test 2: IVR Quick Hangup")
    print(f"  Calling IVR number: {IVR_NUMBER}")
    print("="*60)

    with SipBotProcess("Caller") as caller:
        try:
            # Short call - hangup after 3 seconds
            caller.start_call('bob', DEFAULT_PASSWORD,
                             f'sip:{IVR_NUMBER}@{PROXY_HOST}:{PROXY_PORT}',
                             hangup=3)

            # Wait for call to complete
            caller.wait(timeout=10)

            # Analyze output
            caller_output = caller.get_output()

            # Verify call was established
            if '200 OK' not in caller_output:
                result.add_error("Caller did not receive 200 OK from IVR")
            else:
                print("✓ Call established with IVR")

            # Should have received some RTP packets before hangup
            rx_stats = re.search(r'RX: (\d+)p', caller_output)
            if rx_stats:
                print(f"✓ Received {rx_stats.group(1)} RTP packets before hangup")

            result.finish(len(result.errors) == 0)

        except Exception as e:
            result.add_error(f"Exception: {e}")
            result.finish(False)

    result.print_summary()
    return result


def main():
    """Run all IVR e2e tests"""
    print("\n" + "="*60)
    print("RustPBX IVR Module E2E Tests")
    print("="*60)

    # Register cleanup function
    atexit.register(cleanup_rustpbx)

    # Check sipbot availability
    try:
        subprocess.run(['sipbot', '--version'],
                      capture_output=True,
                      check=True,
                      timeout=5)
        print("✓ sipbot is available")
    except (subprocess.CalledProcessError, FileNotFoundError, subprocess.TimeoutExpired):
        print("❌ Error: sipbot not found or not working")
        print("   Please install: cargo install sipbot")
        return 1

    # Start rustpbx
    if not start_rustpbx():
        return 1

    # Verify rustpbx health
    healthy, msg = check_rustpbx_health()
    if not healthy:
        print(f"❌ Error: {msg}")
        return 1
    print(f"✓ {msg}")

    # Run tests
    results = []

    tests = [
        test_ivr_basic_call,
        test_ivr_quick_hangup,
    ]

    for test_func in tests:
        result = test_func()
        results.append(result)
        time.sleep(2)  # Interval between tests

    # Summary
    print("\n" + "="*60)
    print("TEST SUMMARY")
    print("="*60)

    passed = sum(1 for r in results if r.passed)
    total = len(results)

    for r in results:
        status = "✅" if r.passed else "❌"
        print(f"{status} {r.name} ({r.duration():.1f}s)")

    print("="*60)
    print(f"Result: {passed}/{total} tests passed")
    print("="*60)

    # Cleanup
    cleanup_rustpbx()

    return 0 if passed == total else 1


if __name__ == '__main__':
    try:
        exit_code = main()
    except KeyboardInterrupt:
        print("\n\n⚠ Test interrupted by user")
        cleanup_rustpbx()
        exit_code = 130
    except Exception as e:
        print(f"\n\n❌ Unexpected error: {e}")
        import traceback
        traceback.print_exc()
        cleanup_rustpbx()
        exit_code = 1

    sys.exit(exit_code)
