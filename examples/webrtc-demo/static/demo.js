document.addEventListener('DOMContentLoaded', () => {
    const startButton = document.getElementById('start');
    const stopButton = document.getElementById('stop');
    const statusDiv = document.getElementById('status');

    let peerConnection = null;
    let audioElement = null;

    function addStatusMessage(message) {
        const messageElement = document.createElement('p');
        messageElement.textContent = new Date().toLocaleTimeString() + ': ' + message;
        statusDiv.appendChild(messageElement);
        statusDiv.scrollTop = statusDiv.scrollHeight;
    }

    async function startConnection() {
        try {
            if (peerConnection) {
                addStatusMessage('Connection already exists, stopping it first');
                await stopConnection();
            }

            addStatusMessage('Starting WebRTC connection...');

            // Create an audio element for output
            audioElement = new Audio();
            audioElement.autoplay = true;

            // Initialize peer connection
            const configuration = {
                iceServers: [
                    { urls: 'stun:stun.l.google.com:19302' }
                ]
            };

            peerConnection = new RTCPeerConnection(configuration);

            // When we get a track from the server, add it to our audio element
            peerConnection.ontrack = (event) => {
                addStatusMessage('Received audio track from server');
                audioElement.srcObject = event.streams[0];
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
                if (event.candidate) {
                    // Send ICE candidate to server
                    sendSignal({
                        type: 'ice-candidate',
                        candidate: event.candidate
                    });
                }
            };

            // Create SDP offer
            const offer = await peerConnection.createOffer({
                offerToReceiveAudio: true,
                offerToReceiveVideo: false
            });

            await peerConnection.setLocalDescription(offer);

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
                throw new Error('Server error: ' + response.status);
            }

            // Get SDP answer from server
            const answerData = await response.json();
            addStatusMessage('Received SDP answer from server');

            // Set remote description
            await peerConnection.setRemoteDescription(new RTCSessionDescription(answerData.sdp));

        } catch (error) {
            addStatusMessage('Error: ' + error.message);
            await stopConnection();
        }
    }

    async function stopConnection() {
        if (peerConnection) {
            peerConnection.close();
            peerConnection = null;
        }

        if (audioElement) {
            audioElement.srcObject = null;
            audioElement = null;
        }

        stopButton.disabled = true;
        startButton.disabled = false;

        addStatusMessage('Connection stopped');

        // Notify server
        try {
            await fetch('/webrtc/close', {
                method: 'POST'
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
}); 