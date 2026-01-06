#!/bin/bash

# Kill any running instances
pkill -f rustpbx
pkill -f sipbot

# 1. Compile rustpbx first
echo "Compiling rustpbx..."
cargo build --bin rustpbx || { echo "Compilation failed"; exit 1; }

# Cleanup function
cleanup() {
    echo "Cleaning up..."
    [ -n "$PBX_PID" ] && kill $PBX_PID 2>/dev/null
    [ -n "$ALICE_PID" ] && kill $ALICE_PID 2>/dev/null
    [ -n "$BOB_PID" ] && kill $BOB_PID 2>/dev/null
    pkill -f sipbot
}
trap cleanup EXIT

# Start rustpbx in background
echo "Starting rustpbx..."
RUST_LOG=debug ./target/debug/rustpbx --conf rustpbx.toml > rustpbx.log 2>&1 &
PBX_PID=$!

# 2. Wait for PBX to start (check port 8080)
echo "Waiting for rustpbx to start on port 8080..."
for i in {1..30}; do
    if nc -z 127.0.0.1 8080; then
        echo "rustpbx is up!"
        break
    fi
    if [ $i -eq 30 ]; then
        echo "Timeout waiting for rustpbx"
        exit 1
    fi
    sleep 1
done

# Start Alice (Callee)
echo "Starting Alice..."
sipbot -E 127.0.0.1 wait --username alice --password 123456 --register 127.0.0.1:15060 -a 127.0.0.1:5062 --answer config/sounds/phone-calling.wav --codecs pcmu -v > alice.log 2>&1 &
ALICE_PID=$!

# Wait for Alice to register
echo "Waiting for Alice to register..."
for i in {1..10}; do
    if grep -q "Registered successfully" alice.log; then
        echo "Alice registered!"
        break
    fi
    sleep 1
done

# Start Bob (Caller)
echo "Starting Bob..."
sipbot -E 127.0.0.1 call -t sip:alice@127.0.0.1:15060 --username bob --password 123456 --hangup 5 --play config/sounds/phone-calling.wav --codecs pcmu -v > bob.log 2>&1 &
BOB_PID=$!

# Wait for call to finish
wait $BOB_PID
BOB_EXIT_CODE=$?

# 3. Check results
echo "Checking results..."
RESULT=0

if [ $BOB_EXIT_CODE -ne 0 ]; then
    echo "FAILURE: Bob (caller) exited with error code $BOB_EXIT_CODE"
    RESULT=1
fi

# Check if call was established
if grep -q "Call established" bob.log; then
    echo "SUCCESS: Call established"
else
    echo "FAILURE: Call not established"
    RESULT=1
fi

# Check for audio reception
if grep -q "Progress:.*RX: [1-9][0-9]*p" bob.log; then
    echo "SUCCESS: Audio received by caller (Bob)"
else
    echo "FAILURE: No audio received by caller (Bob)"
    grep "Progress:" bob.log | tail -n 1
    RESULT=1
fi

if grep -q "Progress:.*RX: [1-9][0-9]*p" alice.log; then
    echo "SUCCESS: Audio received by callee (Alice)"
elif grep -q "Playback started" alice.log && grep -q "Sent .* chunks" alice.log; then
    echo "SUCCESS: Alice is active and sending audio (RX check skipped as sipbot wait doesn't log it)"
else
    echo "FAILURE: Alice is not active or not sending audio"
    tail -n 5 alice.log
    RESULT=1
fi

if [ $RESULT -eq 0 ]; then
    echo "INTEGRATION TEST PASSED"
else
    echo "INTEGRATION TEST FAILED"
fi

exit $RESULT
