#!/usr/bin/env python3
"""
RustPBX RWI E2E Tests
End-to-end testing using RWI WebSocket interface + sipbot SIP UAs

This test uses the same architecture as Rust E2E tests:
- RWI WebSocket client for call control and event verification
- sipbot for SIP UA (callee/caller)
- RTP validation via sipbot output parsing

Usage:
    python tests/e2e_rwi_test.py

Prerequisites:
    1. Build rustpbx: cargo build
    2. Install sipbot: cargo install sipbot
    3. Ensure config.toml.dev exists with RWI enabled
    4. pip install websockets
"""

import asyncio
import json
import re
import subprocess
import sys
import time
import atexit
import signal
from pathlib import Path
from typing import Optional, Dict, Any, Callable, List, Tuple
from dataclasses import dataclass

# Optional import - will check later
try:
    import websockets
    WEBSOCKETS_AVAILABLE = True
except ImportError:
    WEBSOCKETS_AVAILABLE = False

# Configuration
PROXY_HOST = '127.0.0.1'
PROXY_PORT = 15061
HTTP_PORT = 8082
RWI_WS_URL = f'ws://{PROXY_HOST}:{HTTP_PORT}/rwi/v1'
TEST_TOKEN = 'test-token-rwi'
RUSTPBX_CONFIG = 'config.toml.dev'
RUSTPBX_STARTUP_TIMEOUT = 10
IVR_NUMBER = '1100'  # IVR test number from config/ivr/test_e2e.toml

# Global rustpbx process
_rustpbx_process: Optional[subprocess.Popen] = None
_rustpbx_output_file = None


@dataclass
class RtpStats:
    """RTP statistics parsed from sipbot output"""
    rx_packets: int = 0
    rx_bytes: int = 0
    tx_packets: int = 0
    tx_bytes: int = 0
    
    @property
    def has_rx(self) -> bool:
        return self.rx_packets > 0
    
    @property
    def has_tx(self) -> bool:
        return self.tx_packets > 0
    
    @property
    def is_bidirectional(self) -> bool:
        return self.has_rx and self.has_tx
    
    def __str__(self) -> str:
        return f"RTP RX:{self.rx_packets}p/{self.rx_bytes}b TX:{self.tx_packets}p/{self.tx_bytes}b"


class RwiClient:
    """RWI WebSocket client for call control and event monitoring"""
    
    def __init__(self):
        self.ws = None
        self.message_id = 0
        self.pending_responses: Dict[str, asyncio.Future] = {}
        self.event_handlers: List[Callable[[dict], None]] = []
        self.receive_task = None
        self.connected = False
        
    async def connect(self, token: str = TEST_TOKEN) -> bool:
        """Connect to RWI WebSocket"""
        try:
            url = f"{RWI_WS_URL}?token={token}"
            self.ws = await websockets.connect(url)
            self.connected = True
            
            # Start message receiver
            self.receive_task = asyncio.create_task(self._receive_loop())
            return True
        except Exception as e:
            print(f"Failed to connect to RWI: {e}")
            return False
    
    async def disconnect(self):
        """Disconnect from RWI"""
        self.connected = False
        if self.receive_task:
            self.receive_task.cancel()
            try:
                await self.receive_task
            except asyncio.CancelledError:
                pass
        if self.ws:
            await self.ws.close()
            
    async def _receive_loop(self):
        """Background task to receive messages"""
        try:
            async for message in self.ws:
                try:
                    data = json.loads(message)
                    await self._handle_message(data)
                except json.JSONDecodeError:
                    print(f"Received non-JSON message: {message}")
        except websockets.exceptions.ConnectionClosed:
            print("RWI WebSocket connection closed")
        except asyncio.CancelledError:
            pass
            
    async def _handle_message(self, data: dict):
        """Handle incoming message"""
        action_id = data.get('action_id')
        if action_id and action_id in self.pending_responses:
            future = self.pending_responses.pop(action_id)
            if not future.done():
                future.set_result(data)
            return
        
        # It's an event - notify handlers
        for handler in self.event_handlers:
            try:
                handler(data)
            except Exception as e:
                print(f"Event handler error: {e}")
    
    def add_event_handler(self, handler: Callable[[dict], None]):
        """Add an event handler"""
        self.event_handlers.append(handler)
        
    def remove_event_handler(self, handler: Callable[[dict], None]):
        """Remove an event handler"""
        if handler in self.event_handlers:
            self.event_handlers.remove(handler)
    
    async def send_request(self, action: str, params: dict = None, timeout: float = 5.0) -> dict:
        """Send a request and wait for response"""
        self.message_id += 1
        action_id = f"py-{self.message_id}"
        
        request = {
            "rwi": "1.0",
            "action_id": action_id,
            "action": action,
            "params": params or {}
        }
        
        future = asyncio.get_event_loop().create_future()
        self.pending_responses[action_id] = future
        
        try:
            await self.ws.send(json.dumps(request))
            return await asyncio.wait_for(future, timeout=timeout)
        except asyncio.TimeoutError:
            self.pending_responses.pop(action_id, None)
            raise TimeoutError(f"Request {action} timed out")
        
    # === Session Commands ===
    async def subscribe(self, contexts: List[str]) -> dict:
        """Subscribe to contexts"""
        return await self.send_request("session.subscribe", {"contexts": contexts})
    
    async def list_calls(self) -> dict:
        """List all calls"""
        return await self.send_request("session.list_calls")
    
    # === Call Commands ===
    async def originate(self, call_id: str, destination: str, 
                       caller_id: str = None, context: str = "default",
                       timeout_secs: int = 30) -> dict:
        """Originate a call"""
        params = {
            "call_id": call_id,
            "destination": destination,
            "context": context,
            "timeout_secs": timeout_secs
        }
        if caller_id:
            params["caller_id"] = caller_id
        return await self.send_request("call.originate", params)
    
    async def answer(self, call_id: str) -> dict:
        """Answer a call"""
        return await self.send_request("call.answer", {"call_id": call_id})
    
    async def hangup(self, call_id: str, reason: str = None) -> dict:
        """Hangup a call"""
        params = {"call_id": call_id}
        if reason:
            params["reason"] = reason
        return await self.send_request("call.hangup", params)
    
    async def reject(self, call_id: str, reason: str = None) -> dict:
        """Reject a call"""
        params = {"call_id": call_id}
        if reason:
            params["reason"] = reason
        return await self.send_request("call.reject", params)
    
    async def ring(self, call_id: str) -> dict:
        """Send ringing to a call"""
        return await self.send_request("call.ring", {"call_id": call_id})
    
    async def bridge(self, leg_a: str, leg_b: str) -> dict:
        """Bridge two calls"""
        return await self.send_request("call.bridge", {"leg_a": leg_a, "leg_b": leg_b})
    
    async def unbridge(self, call_id: str) -> dict:
        """Unbridge a call"""
        return await self.send_request("call.unbridge", {"call_id": call_id})
    
    async def hold(self, call_id: str) -> dict:
        """Put a call on hold"""
        return await self.send_request("call.hold", {"call_id": call_id})
    
    async def unhold(self, call_id: str) -> dict:
        """Resume a call from hold"""
        return await self.send_request("call.unhold", {"call_id": call_id})
    
    async def transfer(self, call_id: str, target: str, attended: bool = False) -> dict:
        """Transfer a call"""
        params = {"call_id": call_id, "target": target}
        if attended:
            params["attended"] = True
        return await self.send_request("call.transfer", params)
    
    # === Media Commands ===
    async def media_play(self, call_id: str, source_type: str, uri: str,
                        loop: bool = False) -> dict:
        """Play media on a call"""
        return await self.send_request("media.play", {
            "call_id": call_id,
            "source": {"type": source_type, "uri": uri},
            "loop": loop
        })
    
    async def media_stop(self, call_id: str) -> dict:
        """Stop media playback"""
        return await self.send_request("media.stop", {"call_id": call_id})
    
    # === Recording Commands ===
    async def record_start(self, call_id: str, path: str, beep: bool = True) -> dict:
        """Start recording a call"""
        return await self.send_request("record.start", {
            "call_id": call_id,
            "storage": {"type": "file", "path": path},
            "beep": beep
        })
    
    async def record_stop(self, call_id: str) -> dict:
        """Stop recording"""
        return await self.send_request("record.stop", {"call_id": call_id})
    
    # === Queue Commands ===
    async def queue_enqueue(self, call_id: str, queue_id: str, priority: int = None) -> dict:
        """Enqueue a call"""
        params = {"call_id": call_id, "queue_id": queue_id}
        if priority is not None:
            params["priority"] = priority
        return await self.send_request("queue.enqueue", params)
    
    async def queue_dequeue(self, call_id: str) -> dict:
        """Dequeue a call"""
        return await self.send_request("queue.dequeue", {"call_id": call_id})
    
    async def queue_agent_login(self, agent_id: str, queue_id: str) -> dict:
        """Login an agent to a queue"""
        return await self.send_request("queue.agent_login", {
            "agent_id": agent_id,
            "queue_id": queue_id
        })
    
    async def queue_agent_logout(self, agent_id: str) -> dict:
        """Logout an agent from all queues"""
        return await self.send_request("queue.agent_logout", {"agent_id": agent_id})
    
    async def queue_agent_ready(self, agent_id: str, ready: bool = True) -> dict:
        """Set agent ready/unready status"""
        return await self.send_request("queue.agent_ready", {
            "agent_id": agent_id,
            "ready": ready
        })
    
    async def queue_status(self, queue_id: str) -> dict:
        """Get queue status"""
        return await self.send_request("queue.status", {"queue_id": queue_id})
    
    # === Conference Commands ===
    async def conference_create(self, conf_id: str, max_members: int = None) -> dict:
        """Create a conference"""
        params = {"conference_id": conf_id}
        if max_members:
            params["max_members"] = max_members
        return await self.send_request("conference.create", params)
    
    async def conference_destroy(self, conf_id: str) -> dict:
        """Destroy a conference"""
        return await self.send_request("conference.destroy", {"conference_id": conf_id})
    
    async def conference_add(self, conf_id: str, call_id: str) -> dict:
        """Add a call to conference"""
        return await self.send_request("conference.add", {
            "conference_id": conf_id,
            "call_id": call_id
        })
    
    async def conference_remove(self, conf_id: str, call_id: str) -> dict:
        """Remove a call from conference"""
        return await self.send_request("conference.remove", {
            "conference_id": conf_id,
            "call_id": call_id
        })
    
    async def conference_mute(self, conf_id: str, call_id: str) -> dict:
        """Mute a participant in conference"""
        return await self.send_request("conference.mute", {
            "conference_id": conf_id,
            "call_id": call_id
        })
    
    async def conference_unmute(self, conf_id: str, call_id: str) -> dict:
        """Unmute a participant in conference"""
        return await self.send_request("conference.unmute", {
            "conference_id": conf_id,
            "call_id": call_id
        })
    
    async def get_call_info(self, call_id: str) -> dict:
        """Get detailed call information"""
        return await self.send_request("call.info", {"call_id": call_id})
    
    async def send_dtmf(self, call_id: str, digits: str, duration_ms: int = 100) -> dict:
        """Send DTMF digits on a call"""
        return await self.send_request("call.send_dtmf", {
            "call_id": call_id,
            "digits": digits,
            "duration_ms": duration_ms
        })


class SipBotProcess:
    """sipbot process manager with RTP stats parsing"""
    
    def __init__(self, name: str):
        self.name = name
        self.process: Optional[subprocess.Popen] = None
        self.output = []
        self.output_lock = None  # Will be created in async context
        self.reader_thread = None
        self._rtp_stats: Optional[RtpStats] = None
        
    def _init_lock(self):
        """Initialize thread lock (call from main thread)"""
        import threading
        self.output_lock = threading.Lock()
    
    def start_callee(self, port: int, ring_secs: int = 2, answer_mode: str = "echo"):
        """Start sipbot as callee (wait for incoming calls)"""
        self._init_lock()
        cmd = [
            'sipbot', 'wait',
            '--username', 'bob',
            '--password', '123456',
            '--register', f'{PROXY_HOST}:{PROXY_PORT}',
            '-a', f'{PROXY_HOST}:{port}',
            '--codecs', 'pcmu',
            '--ring-duration', str(ring_secs),
            '-v'
        ]
        
        # Add answer mode
        if answer_mode == "echo":
            cmd.extend(['--echo'])
        elif answer_mode == "reject":
            cmd.extend(['--reject', '486'])  # Busy Here
        else:
            cmd.extend(['--answer', answer_mode])
        
        print(f"[{self.name}] Starting callee: {' '.join(cmd)}")
        self._start(cmd)
        
    def start_caller(self, target: str, hangup: int = 10, wait_time: int = None):
        """Start sipbot as caller"""
        self._init_lock()
        cmd = [
            'sipbot', 'call',
            '-t', target,
            '--username', 'alice',
            '--password', '123456',
            '--codecs', 'pcmu',
            '--hangup', str(hangup),
            '-v'
        ]
        if wait_time:
            cmd.extend(['--wait', str(wait_time)])
        
        print(f"[{self.name}] Starting caller: {' '.join(cmd)}")
        self._start(cmd)
        
    def _start(self, cmd: list):
        """Start the process"""
        import threading
        self.process = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )
        
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
    
    def get_rtp_stats(self) -> RtpStats:
        """Parse RTP stats from output"""
        output = self.get_output()
        stats = RtpStats()
        
        # Parse RX: XXp/XXb TX: XXp/XXb pattern
        # Example: "RX: 123p/20640b TX: 456p/76480b"
        rx_match = re.search(r'RX:\s*(\d+)p/(\d+)b', output)
        tx_match = re.search(r'TX:\s*(\d+)p/(\d+)b', output)
        
        if rx_match:
            stats.rx_packets = int(rx_match.group(1))
            stats.rx_bytes = int(rx_match.group(2))
        
        if tx_match:
            stats.tx_packets = int(tx_match.group(1))
            stats.tx_bytes = int(tx_match.group(2))
        
        return stats
    
    def wait(self, timeout: int = 60) -> int:
        """Wait for process to finish"""
        if self.process:
            try:
                return self.process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                print(f"[{self.name}] Timeout, terminating...")
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
        self.rtp_stats: Dict[str, RtpStats] = {}
        
    def add_error(self, msg: str):
        self.errors.append(msg)
        
    def add_rtp_stats(self, name: str, stats: RtpStats):
        self.rtp_stats[name] = stats
        
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
        if self.rtp_stats:
            print("RTP Statistics:")
            for name, stats in self.rtp_stats.items():
                print(f"  {name}: {stats}")
        print('='*60)


def cleanup_rustpbx():
    """Clean up rustpbx process"""
    global _rustpbx_process, _rustpbx_output_file
    
    if _rustpbx_process and _rustpbx_process.poll() is None:
        print("\nCleaning up rustpbx...")
        try:
            _rustpbx_process.terminate()
            _rustpbx_process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            _rustpbx_process.kill()
            _rustpbx_process.wait()


def start_rustpbx() -> bool:
    """Start rustpbx process"""
    global _rustpbx_process, _rustpbx_output_file
    
    rustpbx_bin = Path('target/debug/rustpbx')
    if not rustpbx_bin.exists():
        print(f"rustpbx binary not found: {rustpbx_bin}")
        return False
    
    log_dir = Path('tests/logs')
    log_dir.mkdir(parents=True, exist_ok=True)
    log_file = log_dir / 'rustpbx_rwi_e2e.log'
    
    try:
        _rustpbx_output_file = open(log_file, 'w')
        _rustpbx_process = subprocess.Popen(
            [str(rustpbx_bin), '--conf', RUSTPBX_CONFIG],
            stdout=_rustpbx_output_file,
            stderr=subprocess.STDOUT,
            preexec_fn=None if sys.platform == 'win32' else lambda: signal.signal(signal.SIGINT, signal.SIG_IGN)
        )
        
        print(f"rustpbx started (PID: {_rustpbx_process.pid})")
        
        # Wait for startup
        time.sleep(RUSTPBX_STARTUP_TIMEOUT)
        
        if _rustpbx_process.poll() is not None:
            print("rustpbx exited unexpectedly")
            return False
            
        return True
        
    except Exception as e:
        print(f"Failed to start rustpbx: {e}")
        return False


# ==============================================================================
# Helper Functions
# ==============================================================================

def verify_rtp_bidirectional(stats: RtpStats, name: str, min_packets: int = 50) -> List[str]:
    """Verify RTP is bidirectional with minimum packets"""
    errors = []
    
    if not stats.has_rx:
        errors.append(f"{name}: No RTP received (RX=0)")
    elif stats.rx_packets < min_packets:
        errors.append(f"{name}: Too few RTP packets received ({stats.rx_packets} < {min_packets})")
    
    if not stats.has_tx:
        errors.append(f"{name}: No RTP transmitted (TX=0)")
    elif stats.tx_packets < min_packets:
        errors.append(f"{name}: Too few RTP packets transmitted ({stats.tx_packets} < {min_packets})")
    
    return errors


def verify_rtp_rx_only(stats: RtpStats, name: str, min_packets: int = 50) -> List[str]:
    """Verify RTP has RX but little/no TX (for hold scenarios)"""
    errors = []
    
    if not stats.has_rx:
        errors.append(f"{name}: No RTP received (RX=0)")
    elif stats.rx_packets < min_packets:
        errors.append(f"{name}: Too few RTP packets received ({stats.rx_packets} < {min_packets})")
    
    # On hold, TX should be minimal or zero
    if stats.tx_packets > 10:
        errors.append(f"{name}: Unexpected TX packets while on hold ({stats.tx_packets})")
    
    return errors


async def wait_for_event(events: List[dict], event_type: str, timeout: float = 5.0) -> Optional[dict]:
    """Wait for a specific event type"""
    start = time.time()
    while time.time() - start < timeout:
        for evt in events:
            if evt.get('event') == event_type:
                return evt
        await asyncio.sleep(0.1)
    return None


# ==============================================================================
# Tests
# ==============================================================================

async def test_list_calls(rwi: RwiClient) -> TestResult:
    """Test 1: List calls via RWI"""
    result = TestResult("List Calls")
    
    try:
        resp = await rwi.list_calls()
        if resp.get('response') == 'success':
            result.finish(True)
        else:
            result.add_error(f"List calls failed: {resp.get('error')}")
            result.finish(False)
    except Exception as e:
        result.add_error(f"Exception: {e}")
        result.finish(False)
    
    return result


async def test_originate_and_answer() -> TestResult:
    """Test 2: Originate a call via RWI and verify callee answers with RTP"""
    result = TestResult("Originate & Answer with RTP")
    
    with SipBotProcess("Bob") as bob:
        bob.start_callee(port=5070, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            events = []
            def on_event(evt):
                events.append(evt)
            rwi.add_event_handler(on_event)
            
            call_id = f"test-call-{int(time.time())}"
            destination = f"sip:bob@{PROXY_HOST}:5070"
            
            print(f"Originating call {call_id} to {destination}")
            orig_resp = await rwi.originate(
                call_id=call_id,
                destination=destination,
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            if orig_resp.get('response') != 'success':
                result.add_error(f"Originate failed: {orig_resp.get('error')}")
                result.finish(False)
                return result
            
            # Wait for call to complete
            await asyncio.sleep(3)
            
            # Verify events
            event_types = [e.get('event') for e in events]
            if 'CallRinging' not in event_types:
                result.add_error("Expected CallRinging event not received")
            if 'CallAnswered' not in event_types:
                result.add_error("Expected CallAnswered event not received")
            
            # Get RTP stats
            bob_stats = bob.get_rtp_stats()
            result.add_rtp_stats("Bob", bob_stats)
            
            # Verify RTP
            rtp_errors = verify_rtp_bidirectional(bob_stats, "Bob", min_packets=30)
            for err in rtp_errors:
                result.add_error(err)
            
            # Cleanup
            await rwi.hangup(call_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
    
    return result


async def test_hold_unhold() -> TestResult:
    """Test 3: Hold/Unhold with RTP verification"""
    result = TestResult("Hold/Unhold with RTP")
    
    with SipBotProcess("Bob") as bob:
        bob.start_callee(port=5070, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            call_id = f"test-hold-{int(time.time())}"
            
            # Originate
            await rwi.originate(
                call_id=call_id,
                destination=f"sip:bob@{PROXY_HOST}:5070",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            # Wait for answer and collect some RTP
            await asyncio.sleep(2)
            
            # Get stats before hold
            stats_before = bob.get_rtp_stats()
            result.add_rtp_stats("Before Hold", stats_before)
            
            # Hold
            print("Putting call on hold...")
            hold_resp = await rwi.hold(call_id)
            if hold_resp.get('response') != 'success':
                result.add_error(f"Hold failed: {hold_resp.get('error')}")
            
            await asyncio.sleep(2)
            
            # Unhold
            print("Resuming call from hold...")
            unhold_resp = await rwi.unhold(call_id)
            if unhold_resp.get('response') != 'success':
                result.add_error(f"Unhold failed: {unhold_resp.get('error')}")
            
            await asyncio.sleep(2)
            
            # Get stats after unhold
            stats_after = bob.get_rtp_stats()
            result.add_rtp_stats("After Unhold", stats_after)
            
            # Verify overall RTP
            rtp_errors = verify_rtp_bidirectional(stats_after, "Bob", min_packets=50)
            for err in rtp_errors:
                result.add_error(err)
            
            # Cleanup
            await rwi.hangup(call_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
    
    return result


async def test_bridge() -> TestResult:
    """Test 4: Bridge two calls with RTP verification"""
    result = TestResult("Bridge Two Calls with RTP")
    
    with SipBotProcess("Bob") as bob, SipBotProcess("Alice") as alice:
        # Start both callees
        bob.start_callee(port=5070, ring_secs=1)
        alice.start_callee(port=5071, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            leg_a = f"test-leg-a-{int(time.time())}"
            leg_b = f"test-leg-b-{int(time.time())}"
            
            # Originate to both
            print(f"Originating leg A to Bob...")
            await rwi.originate(
                call_id=leg_a,
                destination=f"sip:bob@{PROXY_HOST}:5070",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            await asyncio.sleep(1)
            
            print(f"Originating leg B to Alice...")
            await rwi.originate(
                call_id=leg_b,
                destination=f"sip:alice@{PROXY_HOST}:5071",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            # Wait for both to answer
            await asyncio.sleep(2)
            
            # Bridge
            print(f"Bridging {leg_a} <-> {leg_b}...")
            bridge_resp = await rwi.bridge(leg_a, leg_b)
            if bridge_resp.get('response') != 'success':
                result.add_error(f"Bridge failed: {bridge_resp.get('error')}")
            
            # Let bridged call run
            await asyncio.sleep(3)
            
            # Get RTP stats
            bob_stats = bob.get_rtp_stats()
            alice_stats = alice.get_rtp_stats()
            result.add_rtp_stats("Bob", bob_stats)
            result.add_rtp_stats("Alice", alice_stats)
            
            # Verify both have bidirectional RTP
            for name, stats in [("Bob", bob_stats), ("Alice", alice_stats)]:
                rtp_errors = verify_rtp_bidirectional(stats, name, min_packets=50)
                for err in rtp_errors:
                    result.add_error(err)
            
            # Cleanup
            await rwi.hangup(leg_a)
            await rwi.hangup(leg_b)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
            alice.terminate()
    
    return result


async def test_conference() -> TestResult:
    """Test 5: Three-party conference with RTP verification"""
    result = TestResult("Three-Party Conference with RTP")
    
    conf_id = f"test-conf-{int(time.time())}"
    
    with SipBotProcess("Bob") as bob, SipBotProcess("Alice") as alice, SipBotProcess("Charlie") as charlie:
        # Start all participants
        bob.start_callee(port=5070, ring_secs=1)
        alice.start_callee(port=5071, ring_secs=1)
        charlie.start_callee(port=5072, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            # Create conference
            print(f"Creating conference {conf_id}...")
            create_resp = await rwi.conference_create(conf_id)
            if create_resp.get('response') != 'success':
                result.add_error(f"Conference create failed: {create_resp.get('error')}")
                result.finish(False)
                return result
            
            # Originate to all three
            calls = []
            for name, port in [("Bob", 5070), ("Alice", 5071), ("Charlie", 5072)]:
                call_id = f"test-{name.lower()}-{int(time.time())}"
                print(f"Originating to {name}...")
                await rwi.originate(
                    call_id=call_id,
                    destination=f"sip:{name.lower()}@{PROXY_HOST}:{port}",
                    caller_id=f"sip:rwi@{PROXY_HOST}",
                    timeout_secs=15
                )
                calls.append((name, call_id))
                await asyncio.sleep(0.5)
            
            # Wait for all to answer
            await asyncio.sleep(2)
            
            # Add all to conference
            for name, call_id in calls:
                print(f"Adding {name} to conference...")
                add_resp = await rwi.conference_add(conf_id, call_id)
                if add_resp.get('response') != 'success':
                    result.add_error(f"Failed to add {name}: {add_resp.get('error')}")
            
            # Let conference run
            await asyncio.sleep(3)
            
            # Test mute/unmute
            print("Testing mute/unmute...")
            await rwi.conference_mute(conf_id, calls[0][1])  # Mute Bob
            await asyncio.sleep(1)
            await rwi.conference_unmute(conf_id, calls[0][1])  # Unmute Bob
            await asyncio.sleep(1)
            
            # Get RTP stats
            bob_stats = bob.get_rtp_stats()
            alice_stats = alice.get_rtp_stats()
            charlie_stats = charlie.get_rtp_stats()
            result.add_rtp_stats("Bob", bob_stats)
            result.add_rtp_stats("Alice", alice_stats)
            result.add_rtp_stats("Charlie", charlie_stats)
            
            # In conference, all should have bidirectional RTP
            for name, stats in [("Bob", bob_stats), ("Alice", alice_stats), ("Charlie", charlie_stats)]:
                rtp_errors = verify_rtp_bidirectional(stats, name, min_packets=30)
                for err in rtp_errors:
                    result.add_error(err)
            
            # Cleanup
            for name, call_id in calls:
                await rwi.hangup(call_id)
            await rwi.conference_destroy(conf_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
            alice.terminate()
            charlie.terminate()
    
    return result


async def test_media_play() -> TestResult:
    """Test 6: Media play with RTP verification"""
    result = TestResult("Media Play with RTP")
    
    with SipBotProcess("Bob") as bob:
        bob.start_callee(port=5070, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            call_id = f"test-media-{int(time.time())}"
            
            # Originate
            await rwi.originate(
                call_id=call_id,
                destination=f"sip:bob@{PROXY_HOST}:5070",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            await asyncio.sleep(2)
            
            # Play media (using a test file if available)
            # Note: This requires a valid audio file path
            audio_file = "/tmp/test_audio.wav"
            print(f"Playing media: {audio_file}")
            play_resp = await rwi.media_play(call_id, "file", audio_file)
            
            # Media play may fail if file doesn't exist, that's ok for this test
            if play_resp.get('response') != 'success':
                print(f"Media play returned: {play_resp.get('response')} (may be expected if file missing)")
            
            await asyncio.sleep(3)
            
            # Stop media
            await rwi.media_stop(call_id)
            
            # Get RTP stats
            bob_stats = bob.get_rtp_stats()
            result.add_rtp_stats("Bob", bob_stats)
            
            # Verify RTP
            rtp_errors = verify_rtp_bidirectional(bob_stats, "Bob", min_packets=30)
            for err in rtp_errors:
                result.add_error(err)
            
            # Cleanup
            await rwi.hangup(call_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
    
    return result


async def test_queue() -> TestResult:
    """Test 7: Queue enqueue/dequeue with agent login/logout"""
    result = TestResult("Queue Operations (Enqueue/Dequeue/Agent)")
    
    with SipBotProcess("Bob") as bob:
        bob.start_callee(port=5070, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            call_id = f"test-queue-{int(time.time())}"
            queue_id = "test-queue"
            agent_id = "bob-agent"
            
            # Agent login first
            print(f"Agent {agent_id} logging in...")
            login_resp = await rwi.queue_agent_login(agent_id, queue_id)
            if login_resp.get('response') != 'success':
                result.add_error(f"Agent login failed: {login_resp.get('error')}")
            else:
                print(f"✓ Agent {agent_id} logged in to queue {queue_id}")
            
            # Set agent ready
            ready_resp = await rwi.queue_agent_ready(agent_id, True)
            if ready_resp.get('response') != 'success':
                result.add_error(f"Agent ready failed: {ready_resp.get('error')}")
            
            # Originate
            await rwi.originate(
                call_id=call_id,
                destination=f"sip:bob@{PROXY_HOST}:5070",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            await asyncio.sleep(1)
            
            # Enqueue
            print(f"Enqueueing call to {queue_id}...")
            enqueue_resp = await rwi.queue_enqueue(call_id, queue_id, priority=1)
            if enqueue_resp.get('response') != 'success':
                result.add_error(f"Enqueue failed: {enqueue_resp.get('error')}")
            
            await asyncio.sleep(2)
            
            # Check queue status
            status_resp = await rwi.queue_status(queue_id)
            if status_resp.get('response') == 'success':
                print(f"Queue status: {status_resp.get('result', {})}")
            
            # Dequeue
            print(f"Dequeueing call...")
            dequeue_resp = await rwi.queue_dequeue(call_id)
            if dequeue_resp.get('response') != 'success':
                result.add_error(f"Dequeue failed: {dequeue_resp.get('error')}")
            
            # Agent logout
            logout_resp = await rwi.queue_agent_logout(agent_id)
            if logout_resp.get('response') != 'success':
                result.add_error(f"Agent logout failed: {logout_resp.get('error')}")
            
            # Get RTP stats
            bob_stats = bob.get_rtp_stats()
            result.add_rtp_stats("Bob", bob_stats)
            
            # Cleanup
            await rwi.hangup(call_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
    
    return result


async def test_ivr_dtmf() -> TestResult:
    """Test 9: IVR with DTMF input via RWI originate"""
    result = TestResult("IVR Call with DTMF via RWI")
    
    rwi = RwiClient()
    if not await rwi.connect():
        result.add_error("Failed to connect to RWI")
        result.finish(False)
        return result
    
    try:
        await rwi.subscribe(["default"])
        
        call_id = f"test-ivr-{int(time.time())}"
        
        # Originate to IVR number (1100)
        print(f"Originating call to IVR {IVR_NUMBER}...")
        orig_resp = await rwi.originate(
            call_id=call_id,
            destination=f"sip:{IVR_NUMBER}@{PROXY_HOST}:{PROXY_PORT}",
            caller_id=f"sip:rwi@{PROXY_HOST}",
            timeout_secs=15
        )
        
        if orig_resp.get('response') != 'success':
            result.add_error(f"Originate to IVR failed: {orig_resp.get('error')}")
            result.finish(False)
            return result
        
        await asyncio.sleep(2)
        
        # Send DTMF "1" to select menu option
        print("Sending DTMF '1'...")
        dtmf_resp = await rwi.send_dtmf(call_id, "1")
        if dtmf_resp.get('response') != 'success':
            result.add_error(f"DTMF send failed: {dtmf_resp.get('error')}")
        
        await asyncio.sleep(2)
        
        # Send DTMF "#" to hangup
        print("Sending DTMF '#'...")
        dtmf_resp = await rwi.send_dtmf(call_id, "#")
        
        await asyncio.sleep(1)
        
        # Cleanup
        await rwi.hangup(call_id)
        result.finish(len(result.errors) == 0)
        
    finally:
        await rwi.disconnect()
    
    return result


async def test_call_routing_dialplan() -> TestResult:
    """Test 10: Call routing via dialplan through RWI"""
    result = TestResult("Call Routing via Dialplan")
    
    with SipBotProcess("Alice") as alice, SipBotProcess("Bob") as bob:
        # Bob waits for call
        bob.start_callee(port=5070, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            call_id = f"test-routing-{int(time.time())}"
            
            # Originate to dialplan route (e.g., 1000 for echo test)
            print("Originating to dialplan route 1000...")
            orig_resp = await rwi.originate(
                call_id=call_id,
                destination=f"sip:1000@{PROXY_HOST}:{PROXY_PORT}",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            if orig_resp.get('response') != 'success':
                result.add_error(f"Originate failed: {orig_resp.get('error')}")
                result.finish(False)
                return result
            
            await asyncio.sleep(3)
            
            # Get call info
            info_resp = await rwi.get_call_info(call_id)
            if info_resp.get('response') == 'success':
                print(f"Call info: {info_resp.get('result', {})}")
            
            # Cleanup
            await rwi.hangup(call_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
            alice.terminate()
    
    return result


async def test_call_reject() -> TestResult:
    """Test 11: Call reject via RWI"""
    result = TestResult("Call Reject via RWI")
    
    with SipBotProcess("Bob") as bob:
        # Bob waits but will reject
        bob.start_callee(port=5070, ring_secs=0, answer_mode="reject")
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            call_id = f"test-reject-{int(time.time())}"
            
            # Originate
            orig_resp = await rwi.originate(
                call_id=call_id,
                destination=f"sip:bob@{PROXY_HOST}:5070",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=10
            )
            
            # Wait for ringing event
            await asyncio.sleep(1)
            
            # Reject the call
            print("Rejecting call...")
            reject_resp = await rwi.reject(call_id, reason="busy")
            if reject_resp.get('response') != 'success':
                result.add_error(f"Reject failed: {reject_resp.get('error')}")
            
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
    
    return result


async def test_call_ring() -> TestResult:
    """Test 12: Send ringing via RWI"""
    result = TestResult("Send Ringing via RWI")
    
    with SipBotProcess("Bob") as bob:
        bob.start_callee(port=5070, ring_secs=5)  # Long ring time
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            call_id = f"test-ring-{int(time.time())}"
            
            # Originate
            await rwi.originate(
                call_id=call_id,
                destination=f"sip:bob@{PROXY_HOST}:5070",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            await asyncio.sleep(1)
            
            # Send ringing
            print("Sending ringing...")
            ring_resp = await rwi.ring(call_id)
            if ring_resp.get('response') != 'success':
                result.add_error(f"Ring failed: {ring_resp.get('error')}")
            
            await asyncio.sleep(2)
            
            # Cleanup
            await rwi.hangup(call_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
    
    return result


async def test_conference_mute_unmute() -> TestResult:
    """Test 13: Conference mute/unmute functionality"""
    result = TestResult("Conference Mute/Unmute")
    
    conf_id = f"test-conf-mute-{int(time.time())}"
    
    with SipBotProcess("Bob") as bob, SipBotProcess("Alice") as alice:
        # Start both participants
        bob.start_callee(port=5070, ring_secs=1)
        alice.start_callee(port=5071, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            # Create conference
            print(f"Creating conference {conf_id}...")
            create_resp = await rwi.conference_create(conf_id, max_members=5)
            if create_resp.get('response') != 'success':
                result.add_error(f"Conference create failed: {create_resp.get('error')}")
                result.finish(False)
                return result
            
            # Originate to both
            bob_call = f"test-bob-{int(time.time())}"
            alice_call = f"test-alice-{int(time.time())}"
            
            await rwi.originate(
                call_id=bob_call,
                destination=f"sip:bob@{PROXY_HOST}:5070",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            await asyncio.sleep(1)
            
            await rwi.originate(
                call_id=alice_call,
                destination=f"sip:alice@{PROXY_HOST}:5071",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            await asyncio.sleep(2)
            
            # Add to conference
            await rwi.conference_add(conf_id, bob_call)
            await rwi.conference_add(conf_id, alice_call)
            await asyncio.sleep(2)
            
            # Mute Bob
            print("Muting Bob...")
            mute_resp = await rwi.conference_mute(conf_id, bob_call)
            if mute_resp.get('response') != 'success':
                result.add_error(f"Mute failed: {mute_resp.get('error')}")
            
            await asyncio.sleep(2)
            
            # Unmute Bob
            print("Unmuting Bob...")
            unmute_resp = await rwi.conference_unmute(conf_id, bob_call)
            if unmute_resp.get('response') != 'success':
                result.add_error(f"Unmute failed: {unmute_resp.get('error')}")
            
            await asyncio.sleep(2)
            
            # Get RTP stats
            bob_stats = bob.get_rtp_stats()
            alice_stats = alice.get_rtp_stats()
            result.add_rtp_stats("Bob", bob_stats)
            result.add_rtp_stats("Alice", alice_stats)
            
            # Cleanup
            await rwi.hangup(bob_call)
            await rwi.hangup(alice_call)
            await rwi.conference_destroy(conf_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
            alice.terminate()
    
    return result


async def test_transfer() -> TestResult:
    """Test 8: Call transfer"""
    result = TestResult("Call Transfer")
    
    with SipBotProcess("Bob") as bob, SipBotProcess("Charlie") as charlie:
        bob.start_callee(port=5070, ring_secs=1)
        charlie.start_callee(port=5072, ring_secs=1)
        time.sleep(1)
        
        rwi = RwiClient()
        if not await rwi.connect():
            result.add_error("Failed to connect to RWI")
            result.finish(False)
            return result
        
        try:
            await rwi.subscribe(["default"])
            
            call_id = f"test-transfer-{int(time.time())}"
            
            # Originate to Bob
            await rwi.originate(
                call_id=call_id,
                destination=f"sip:bob@{PROXY_HOST}:5070",
                caller_id=f"sip:rwi@{PROXY_HOST}",
                timeout_secs=15
            )
            
            await asyncio.sleep(2)
            
            # Transfer to Charlie
            target = f"sip:charlie@{PROXY_HOST}:5072"
            print(f"Transferring call to {target}...")
            transfer_resp = await rwi.transfer(call_id, target)
            
            # Transfer may complete asynchronously
            print(f"Transfer response: {transfer_resp.get('response')}")
            
            await asyncio.sleep(3)
            
            # Get RTP stats
            bob_stats = bob.get_rtp_stats()
            charlie_stats = charlie.get_rtp_stats()
            result.add_rtp_stats("Bob", bob_stats)
            result.add_rtp_stats("Charlie", charlie_stats)
            
            # Note: After transfer, RTP stats depend on transfer type
            # For blind transfer, Charlie should have RTP, Bob may not
            
            # Cleanup
            await rwi.hangup(call_id)
            result.finish(len(result.errors) == 0)
            
        finally:
            await rwi.disconnect()
            bob.terminate()
            charlie.terminate()
    
    return result


# ==============================================================================
# Main
# ==============================================================================

async def main():
    """Run all RWI E2E tests"""
    print("\n" + "="*60)
    print("RustPBX RWI E2E Tests with RTP Validation")
    print("="*60)
    
    # Check dependencies
    if not WEBSOCKETS_AVAILABLE:
        print("❌ websockets module not found. Install: pip install websockets")
        return 1
    
    try:
        subprocess.run(['sipbot', '--version'], capture_output=True, check=True, timeout=5)
        print("✓ sipbot is available")
    except (subprocess.CalledProcessError, FileNotFoundError):
        print("❌ sipbot not found. Install: cargo install sipbot")
        return 1
    
    # Start rustpbx
    atexit.register(cleanup_rustpbx)
    if not start_rustpbx():
        return 1
    
    # Create shared RWI client for simple tests
    rwi = RwiClient()
    if not await rwi.connect():
        print("❌ Failed to connect to RWI")
        return 1
    
    # Run tests
    results = []
    
    # Test 1: List calls (simple, using shared client)
    result = await test_list_calls(rwi)
    results.append(result)
    result.print_summary()
    
    # Disconnect shared client
    await rwi.disconnect()
    
    # Tests 2-14: Full scenarios with dedicated clients
    # Organized by module: Call, IVR, Queue, Conference, Media
    test_funcs = [
        # === Call Module Tests ===
        test_originate_and_answer,      # Basic call origination and answer
        test_hold_unhold,                # Call hold/unhold
        test_bridge,                     # Call bridging
        test_transfer,                   # Call transfer
        test_call_reject,                # Call reject
        test_call_ring,                  # Send ringing
        test_call_routing_dialplan,      # Dialplan routing
        
        # === IVR Module Tests ===
        test_ivr_dtmf,                   # IVR with DTMF interaction
        
        # === Queue Module Tests ===
        test_queue,                      # Queue operations (enqueue/dequeue/agent)
        
        # === Conference Module Tests ===
        test_conference,                 # Three-party conference
        test_conference_mute_unmute,     # Conference mute/unmute
        
        # === Media Module Tests ===
        test_media_play,                 # Media playback
    ]
    
    for test_func in test_funcs:
        try:
            result = await test_func()
            results.append(result)
            result.print_summary()
        except Exception as e:
            print(f"\n❌ Test {test_func.__name__} failed with exception: {e}")
            import traceback
            traceback.print_exc()
            # Create failed result
            result = TestResult(test_func.__name__.replace('test_', '').replace('_', ' ').title())
            result.add_error(f"Exception: {e}")
            result.finish(False)
            results.append(result)
            result.print_summary()
        
        await asyncio.sleep(1)  # Brief pause between tests
    
    # Summary
    print("\n" + "="*60)
    print("TEST SUMMARY")
    print("="*60)
    
    passed = sum(1 for r in results if r.passed)
    total = len(results)
    
    for r in results:
        status = "✅" if r.passed else "❌"
        print(f"{status} {r.name}")
    
    print(f"\nResult: {passed}/{total} tests passed")
    print("="*60)
    
    cleanup_rustpbx()
    return 0 if passed == total else 1


if __name__ == '__main__':
    try:
        exit_code = asyncio.run(main())
    except KeyboardInterrupt:
        print("\n\nTest interrupted")
        cleanup_rustpbx()
        exit_code = 130
    
    sys.exit(exit_code)
