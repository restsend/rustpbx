document.addEventListener('DOMContentLoaded', () => {
    const startButton = document.getElementById('start');
    const stopButton = document.getElementById('stop');
    const statusDiv = document.getElementById('status');

    let peerConnection = null;
    let audioElement = null;
    let mediaStream = null;

    function addStatusMessage(message) {
        const messageElement = document.createElement('p');
        messageElement.textContent = new Date().toLocaleTimeString() + ': ' + message;
        statusDiv.appendChild(messageElement);
        statusDiv.scrollTop = statusDiv.scrollHeight;
    }
    function enumerateInputDevices() {
        const populateSelect = (select, devices) => {
            let counter = 1;
            devices.forEach((device) => {
                const option = document.createElement('option');
                option.value = device.deviceId;
                option.text = device.label || ('Device #' + counter);
                select.appendChild(option);
                counter += 1;
            });
        };

        navigator.mediaDevices.enumerateDevices().then((devices) => {
            populateSelect(
                document.getElementById('audio-input'),
                devices.filter((device) => device.kind == 'audioinput')
            );
        }).catch((e) => {
            addStatusMessage('Error: ' + e);
        });
    }
    async function startConnection() {
        try {
            audioElement = document.getElementById('audio-player');
            if (!audioElement) {
                addStatusMessage('Audio element not found');
                return;
            }
            if (peerConnection) {
                addStatusMessage('Connection already exists, stopping it first');
                await stopConnection();
            }

            const audioConstraints = {
                video: false,
                audio: {
                    noiseSuppression: true,
                    echoCancellation: true
                }
            };
            const audioInput = document.getElementById('audio-input').value;
            if (audioInput) {
                audioConstraints.audio.deviceId = audioInput;
            }

            mediaStream = await navigator.mediaDevices.getUserMedia(audioConstraints);
            addStatusMessage('Starting WebRTC connection...');
            // Initialize peer connection
            const configuration = {};

            peerConnection = new RTCPeerConnection(configuration);

            mediaStream.getTracks().forEach(track => {
                addStatusMessage('Adding track to peer connection: ' + track.id);
                peerConnection.addTrack(track, mediaStream);
            });

            // When we get a track from the server, add it to our audio element
            peerConnection.ontrack = (event) => {
                addStatusMessage('Received audio track from server');
                audioElement.srcObject = event.streams[0];
            };

            peerConnection.onconnectionstatechange = () => {
                addStatusMessage('Connection state: ' + peerConnection.connectionState);
            };
            // Handle ICE connection state changes
            peerConnection.oniceconnectionstatechange = () => {
                addStatusMessage('ICE connection state: ' + peerConnection.iceConnectionState);
                if (peerConnection.iceConnectionState === 'connected') {
                    stopButton.disabled = false;
                    startButton.disabled = true;
                }
            };

            // Handle ICE candidate events
            peerConnection.onicecandidate = (event) => {
                if (!event.candidate) {
                    handshake().then()
                    return
                }
            };
            // Create SDP offer
            const offer = await peerConnection.createOffer({
                offerToReceiveAudio: true,
                offerToReceiveVideo: false
            });
            await peerConnection.setLocalDescription(offer);

        } catch (error) {
            addStatusMessage('Error: ' + error.message);
            await stopConnection();
        }
    }

    async function handshake() {
        // Send the offer to the server
        addStatusMessage('Sending SDP offer to server');
        const response = await fetch('/webrtc/offer', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({
                sdp: peerConnection.localDescription
            })
        });
        if (!response.ok) {
            addStatusMessage('Sending SDP offer to server failed');
            await stopConnection();
            return
        }

        // Get SDP answer from server
        const { id, answer } = await response.json();
        addStatusMessage('Received SDP answer from server: ' + id);
        addStatusMessage('Setting remote description ');
        // Set remote description
        await peerConnection.setRemoteDescription(new RTCSessionDescription(answer.sdp));
        peerConnection.id = id;
        stopButton.disabled = false;
        startButton.disabled = true;
    }
    async function stopConnection() {
        let id = peerConnection.id;
        if (peerConnection) {
            peerConnection.close();
            peerConnection = null;
        }

        if (audioElement) {
            audioElement.srcObject = null;
            audioElement = null;
        }
        if (mediaStream) {
            mediaStream.getTracks().forEach(track => {
                track.stop();
            });
            mediaStream = null;
        }

        stopButton.disabled = true;
        startButton.disabled = false;

        addStatusMessage('Connection stopped');

        // Notify server
        try {
            await fetch('/webrtc/close', {
                method: 'POST',
                headers: {
                    'Content-Type': 'application/json'
                },
                body: JSON.stringify({
                    id
                })
            });
        } catch (error) {
            addStatusMessage('Error notifying server: ' + error.message);
        }
    }

    async function sendSignal(data) {
        try {
            await fetch('/webrtc/signal', {
                method: 'POST',
                headers: {
                    'Content-Type': 'application/json'
                },
                body: JSON.stringify(data)
            });
        } catch (error) {
            addStatusMessage('Signaling error: ' + error.message);
        }
    }

    // Event listeners
    startButton.addEventListener('click', startConnection);
    stopButton.addEventListener('click', stopConnection);

    // Setup initial page state
    addStatusMessage('WebRTC Audio Demo initialized. Click "Start Audio Connection" to begin.');

    enumerateInputDevices();
}); 