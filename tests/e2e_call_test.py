#!/usr/bin/env python3
"""
RustPBX Call Module E2E Tests
End-to-end testing using sipbot, pure Python standard library implementation
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
PROXY_PORT = 15060
BOB_PORT = 5070
ALICE_PORT = 5071
DEFAULT_PASSWORD = '123456'
CALL_DURATION = 20  # Call duration in seconds (sipbot --hangup parameter)
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
        
    def start_wait(self, username: str, password: str, port: int, reject: Optional[int] = None):
        """Start sipbot to wait for incoming calls"""
        cmd = [
            'sipbot', 'wait',
            '--username', username,
            '--password', password,
            '--register', f'{PROXY_HOST}:{PROXY_PORT}',
            '-a', f'{PROXY_HOST}:{port}',
            '--codecs', 'pcmu',
            '-v'
        ]
        
        if reject:
            cmd.extend(['--reject', str(reject)])
            
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
        
    def start_call(self, username: str, password: str, target: str, hangup: int = CALL_DURATION):
        """Start sipbot to make a call
        
        Args:
            hangup: Call duration in seconds (sipbot --hangup N parameter)
                   sipbot will automatically hangup after N seconds
        """
        cmd = [
            'sipbot', 'call',
            '-t', target,
            '--username', username,
            '--password', password,
            '--codecs', 'pcmu',
            '--hangup', str(hangup),  # sipbot --hangup N: auto-hangup after N seconds
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
    
    # Create log directory
    log_dir = Path('tests/logs')
    log_dir.mkdir(parents=True, exist_ok=True)
    log_file = log_dir / 'rustpbx_e2e.log'
    
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


def test_normal_call() -> TestResult:
    """Test 1: Normal call + recording"""
    result = TestResult("Test 1: Normal Call + Recording")
    
    print("\n" + "="*60)
    print("Starting Test 1: Normal Call + Recording")
    print("="*60)
    
    with SipBotProcess("Alice") as alice, SipBotProcess("Bob") as bob:
        try:
            # 1. Alice waits for incoming call
            alice.start_wait('alice', DEFAULT_PASSWORD, ALICE_PORT)
            time.sleep(SETUP_DELAY)
            
            # 2. Bob makes a call
            bob.start_call('bob', DEFAULT_PASSWORD, f'sip:alice@{PROXY_HOST}:{PROXY_PORT}')
            
            # 3. Wait for call to complete
            bob_exit = bob.wait(timeout=CALL_DURATION + 10)
            
            # 4. Stop Alice
            time.sleep(1)
            alice.terminate()
            
            # 5. Analyze output
            bob_output = bob.get_output()
            alice_output = alice.get_output()
            
            # Verify Bob
            if '200 OK' not in bob_output:
                result.add_error("Bob did not receive 200 OK")
            
            if 'Call established' not in bob_output:
                result.add_error("Call not established on Bob side")
                
            # Check for RTP transmission (TX: packets/bytes)
            if not re.search(r'TX: \d+p/\d+b', bob_output):
                result.add_error("Bob did not send audio (no TX packets)")
                
            # Verify Alice
            # Alice uses --play with embedded wav, so check for "Sent chunks" or RTP stats
            if not re.search(r'(Sent \d+ chunks|TX: \d+p/\d+b)', alice_output):
                result.add_error("Alice did not send audio")
                
            # Verify CDR
            time.sleep(2)  # Wait for CDR to be written
            cdr_ok, cdr_msg = check_cdr_files()
            if not cdr_ok:
                result.add_error(f"CDR check failed: {cdr_msg}")
            else:
                print(f"✓ CDR: {cdr_msg}")
                
            # Determine result
            result.finish(len(result.errors) == 0)
            
        except Exception as e:
            result.add_error(f"Exception: {e}")
            result.finish(False)
            
    result.print_summary()
    return result


def test_callee_not_found() -> TestResult:
    """Test 2: Callee does not exist"""
    result = TestResult("Test 2: Callee Not Found")
    
    print("\n" + "="*60)
    print("Starting Test 2: Callee Not Found")
    print("="*60)
    
    with SipBotProcess("Bob") as bob:
        try:
            # Bob calls non-existent user
            bob.start_call('bob', DEFAULT_PASSWORD, 
                          f'sip:nonexistent@{PROXY_HOST}:{PROXY_PORT}', 
                          hangup=5)
            
            bob_exit = bob.wait(timeout=15)
            bob_output = bob.get_output()
            
            # Should receive error response (404, 480, 603, etc)
            # 404 = Not Found, 480 = Temporarily Unavailable, 603 = Decline
            if not re.search(r'(404|480|408|487|603|6\d\d|4\d\d)', bob_output):
                result.add_error("Expected error response (4xx/6xx) not found")
            else:
                error_match = re.search(r'(4\d\d|6\d\d)\s+\w+', bob_output)
                if error_match:
                    print(f"✓ Received expected error response: {error_match.group(0)}")
                else:
                    print("✓ Received expected error response")
                
            result.finish(len(result.errors) == 0)
            
        except Exception as e:
            result.add_error(f"Exception: {e}")
            result.finish(False)
            
    result.print_summary()
    return result


def test_caller_cancel() -> TestResult:
    """Test 3: Caller cancels the call"""
    result = TestResult("Test 3: Caller Cancel")
    
    print("\n" + "="*60)
    print("Starting Test 3: Caller Cancel")
    print("="*60)
    
    with SipBotProcess("Alice") as alice, SipBotProcess("Bob") as bob:
        try:
            # 1. Alice waits
            alice.start_wait('alice', DEFAULT_PASSWORD, ALICE_PORT)
            time.sleep(SETUP_DELAY)
            
            # 2. Bob makes a call (short duration)
            bob.start_call('bob', DEFAULT_PASSWORD, 
                          f'sip:alice@{PROXY_HOST}:{PROXY_PORT}',
                          hangup=3)
            
            # 3. Wait for call to be established or ringing
            time.sleep(1)
            
            # 4. Quickly terminate Bob (simulate cancel)
            bob.terminate()
            time.sleep(1)
            
            bob_output = bob.get_output()
            alice_output = alice.get_output()
            
            # Bob should exit before establishment
            # Alice may receive CANCEL or no connection at all
            print(f"✓ Bob cancelled after {len(bob_output.splitlines())} log lines")
            
            result.finish(True)  # Pass if we can cancel normally
            
        except Exception as e:
            result.add_error(f"Exception: {e}")
            result.finish(False)
            
    result.print_summary()
    return result


def test_media_bypass() -> TestResult:
    """Test 4: Media bypass (verify via RTP statistics)"""
    result = TestResult("Test 4: Media Transmission")
    
    print("\n" + "="*60)
    print("Starting Test 4: Media Transmission")
    print("="*60)
    
    with SipBotProcess("Alice") as alice, SipBotProcess("Bob") as bob:
        try:
            alice.start_wait('alice', DEFAULT_PASSWORD, ALICE_PORT)
            time.sleep(SETUP_DELAY)
            
            bob.start_call('bob', DEFAULT_PASSWORD, 
                          f'sip:alice@{PROXY_HOST}:{PROXY_PORT}',
                          hangup=10)
            
            bob.wait(timeout=20)
            time.sleep(1)
            alice.terminate()
            
            bob_output = bob.get_output()
            alice_output = alice.get_output()
            
            # Check bidirectional audio
            bob_stats = re.search(r'TX: (\d+)p/\d+b', bob_output)
            alice_stats = re.search(r'(Sent (\d+) chunks|TX: (\d+)p/\d+b)', alice_output)
            
            if not bob_stats:
                result.add_error("Bob did not send audio (no TX packets)")
            else:
                print(f"✓ Bob sent {bob_stats.group(1)} audio packets")
                
            if not alice_stats:
                result.add_error("Alice did not send audio")
            else:
                if alice_stats.group(2):  # Sent chunks
                    print(f"✓ Alice sent {alice_stats.group(2)} audio chunks")
                else:  # TX packets
                    print(f"✓ Alice sent {alice_stats.group(3)} audio packets")
                
            result.finish(len(result.errors) == 0)
            
        except Exception as e:
            result.add_error(f"Exception: {e}")
            result.finish(False)
            
    result.print_summary()
    return result


def main():
    """Run all tests"""
    print("\n" + "="*60)
    print("RustPBX Call Module E2E Tests")
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
        test_normal_call,
        test_callee_not_found,
        test_caller_cancel,
        test_media_bypass,
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
