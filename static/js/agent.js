document.addEventListener('DOMContentLoaded', () => {
    window.mainApp = () => {
        return {
            // Configuration state
            config: {
                vad: {
                    enabled: false,
                    threshold: 0.5
                },
                recording: {
                    enabled: false
                },
                asr: {
                    provider: '',
                    appId: '',
                    secretId: '',
                    secretKey: ''
                },
                tts: {
                    provider: '',
                    appId: '',
                    secretId: '',
                    secretKey: '',
                    speaker: 'female'
                },
                llm: {
                    provider: '',
                    apiKey: '',
                    model: '',
                    prompt: 'You are a helpful assistant.'
                },
                // Add UI state for tabbed interface
                uiState: {
                    activeTab: 'asr'
                }
            },

            // WebSocket connection
            ws: null,
            wsStatus: 'disconnected',

            // WebRTC connection
            peerConnection: null,
            rtcStatus: 'disconnected',
            callActive: false,

            // Debug information
            eventLog: [],
            lastAsrResult: '',
            lastLlmResponse: '',
            lastTtsMessage: '',

            // Initialize the application
            init() {
                this.connectWebSocket();
                this.saveConfigToLocalStorage();
                this.loadConfigFromLocalStorage();
                this.addLogEntry('info', 'Application initialized');
            },

            // Format timestamp for log entries
            formatTime(timestamp) {
                const date = new Date(timestamp);
                return date.toLocaleTimeString('en-US', { hour12: false });
            },

            // Get CSS class for log entry based on type
            getLogEntryClass(type) {
                const classes = {
                    'SYSTEM': 'bg-gray-100 text-gray-800',
                    'ASR': 'bg-blue-100 text-blue-800',
                    'LLM': 'bg-purple-100 text-purple-800',
                    'TTS': 'bg-green-100 text-green-800',
                    'ERROR': 'bg-red-100 text-red-800',
                    'WARNING': 'bg-yellow-100 text-yellow-800'
                };
                return classes[type] || 'bg-gray-100 text-gray-800';
            },

            // Add entry to event log with modern formatting
            logEvent(type, message, data = {}) {
                const event = {
                    type,
                    message,
                    timestamp: Date.now(),
                    ...data
                };

                this.eventLog.push(event);

                // Update specific result fields if applicable
                if (type === 'ASR' && data.asrResult) {
                    this.lastAsrResult = data.asrResult;
                } else if (type === 'LLM' && data.llmResponse) {
                    this.lastLlmResponse = data.llmResponse;
                } else if (type === 'TTS' && data.ttsMessage) {
                    this.lastTtsMessage = data.ttsMessage;
                }

                // Auto-scroll to bottom
                this.$nextTick(() => {
                    const eventLogEl = document.getElementById('eventLog');
                    if (eventLogEl) {
                        eventLogEl.scrollTop = eventLogEl.scrollHeight;
                    }
                });
            },

            // Add entry to event log (backward compatibility)
            addLogEntry(type, message) {
                const mappedType = this.mapLegacyLogType(type);
                this.logEvent(mappedType, message);

                // Keep log limited to 100 entries
                if (this.eventLog.length > 100) {
                    this.eventLog.shift();
                }
            },

            // Map legacy log types to new format
            mapLegacyLogType(type) {
                const typeMap = {
                    'info': 'SYSTEM',
                    'error': 'ERROR',
                    'warning': 'WARNING',
                    'success': 'SYSTEM'
                };
                return typeMap[type] || 'SYSTEM';
            },

            // Connect to WebSocket server
            connectWebSocket() {
                const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
                const wsUrl = `${protocol}//${window.location.host}/ws`;

                this.addLogEntry('info', `Connecting to WebSocket at ${wsUrl}`);

                try {
                    this.ws = new WebSocket(wsUrl);

                    this.ws.onopen = () => {
                        this.wsStatus = 'connected';
                        this.addLogEntry('success', 'WebSocket connected');
                    };

                    this.ws.onclose = () => {
                        this.wsStatus = 'disconnected';
                        this.addLogEntry('warning', 'WebSocket disconnected');

                        // Try to reconnect after 5 seconds
                        setTimeout(() => this.connectWebSocket(), 5000);
                    };

                    this.ws.onerror = (error) => {
                        this.wsStatus = 'error';
                        this.addLogEntry('error', `WebSocket error: ${error.message || 'Unknown error'}`);
                    };

                    this.ws.onmessage = (event) => {
                        this.handleWebSocketMessage(event);
                    };
                } catch (error) {
                    this.addLogEntry('error', `Failed to connect to WebSocket: ${error.message}`);
                }
            },

            // Handle incoming WebSocket messages
            handleWebSocketMessage(event) {
                try {
                    const data = JSON.parse(event.data);

                    this.addLogEntry('info', `Received ${data.type} event`);

                    switch (data.type) {
                        case 'asr_result':
                            this.handleAsrResult(data);
                            break;
                        case 'vad_status':
                            this.handleVadStatus(data);
                            break;
                        case 'webrtc_offer':
                            this.handleWebRtcOffer(data);
                            break;
                        case 'webrtc_ice_candidate':
                            this.handleIceCandidate(data);
                            break;
                        case 'tts_status':
                            this.handleTtsStatus(data);
                            break;
                        case 'llm_response':
                            this.handleLlmResponse(data);
                            break;
                        default:
                            this.addLogEntry('info', `Unhandled event type: ${data.type}`);
                    }
                } catch (error) {
                    this.addLogEntry('error', `Error processing WebSocket message: ${error.message}`);
                }
            },

            // Handle ASR result
            handleAsrResult(data) {
                this.lastAsrResult = data.text;
                this.addLogEntry('info', `ASR Result: ${data.text}`);

                // If we have a valid ASR result, process it with LLM
                if (data.text && data.text.trim() !== '') {
                    this.processWithLlm(data.text);
                }
            },

            // Handle VAD status update
            handleVadStatus(data) {
                this.addLogEntry('info', `VAD Status: ${data.active ? 'Speech detected' : 'Silence detected'}`);
            },

            // Handle WebRTC offer
            handleWebRtcOffer(data) {
                this.addLogEntry('info', 'Received WebRTC offer');

                if (this.peerConnection) {
                    this.peerConnection.setRemoteDescription(new RTCSessionDescription(data.offer))
                        .then(() => this.createAnswer())
                        .catch(error => {
                            this.addLogEntry('error', `Error setting remote description: ${error.message}`);
                        });
                }
            },

            // Handle ICE candidate
            handleIceCandidate(data) {
                if (this.peerConnection) {
                    try {
                        const candidate = new RTCIceCandidate(data.candidate);
                        this.peerConnection.addIceCandidate(candidate)
                            .catch(error => {
                                this.addLogEntry('error', `Error adding ICE candidate: ${error.message}`);
                            });
                    } catch (error) {
                        this.addLogEntry('error', `Invalid ICE candidate: ${error.message}`);
                    }
                }
            },

            // Handle TTS status
            handleTtsStatus(data) {
                this.lastTtsMessage = data.text;
                this.addLogEntry('info', `TTS Message: ${data.text}`);
            },

            // Handle LLM response
            handleLlmResponse(data) {
                this.lastLlmResponse = data.text;
                this.addLogEntry('info', `LLM Response: ${data.text}`);

                // Send TTS request with LLM response text
                this.sendTtsRequest(data.text);
            },

            // Process text with LLM
            processWithLlm(text) {
                if (!this.config.llm.provider || !this.config.llm.apiKey) {
                    this.addLogEntry('warning', 'LLM configuration is incomplete');
                    return;
                }

                if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                    const llmRequest = {
                        type: 'llm_request',
                        text: text,
                        config: {
                            provider: this.config.llm.provider,
                            api_key: this.config.llm.apiKey,
                            model: this.config.llm.model,
                            prompt: this.config.llm.prompt
                        }
                    };

                    this.ws.send(JSON.stringify(llmRequest));
                    this.addLogEntry('info', 'Sent LLM request');
                }
            },

            // Send TTS request
            sendTtsRequest(text) {
                if (!this.config.tts.provider) {
                    this.addLogEntry('warning', 'TTS configuration is incomplete');
                    return;
                }

                if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                    const ttsRequest = {
                        type: 'tts_request',
                        text: text,
                        config: {
                            provider: this.config.tts.provider,
                            app_id: this.config.tts.appId,
                            secret_id: this.config.tts.secretId,
                            secret_key: this.config.tts.secretKey,
                            speaker: this.config.tts.speaker
                        }
                    };

                    this.ws.send(JSON.stringify(ttsRequest));
                    this.addLogEntry('info', 'Sent TTS request');
                }
            },

            // Start a call
            startCall() {
                this.setupPeerConnection();
                this.callActive = true;
                this.addLogEntry('info', 'Starting call...');

                // Create and send offer
                this.createOffer();
            },

            // End a call
            endCall() {
                if (this.peerConnection) {
                    this.peerConnection.close();
                    this.peerConnection = null;
                }

                this.rtcStatus = 'disconnected';
                this.callActive = false;
                this.addLogEntry('info', 'Call ended');

                // Send end call command to server
                if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                    this.ws.send(JSON.stringify({ type: 'end_call' }));
                }
            },

            // Set up WebRTC peer connection
            setupPeerConnection() {
                // Close existing connection if any
                if (this.peerConnection) {
                    this.peerConnection.close();
                }

                const configuration = {
                    iceServers: [
                        { urls: 'stun:stun.l.google.com:19302' }
                    ]
                };

                this.peerConnection = new RTCPeerConnection(configuration);

                // Handle ICE candidate events
                this.peerConnection.onicecandidate = (event) => {
                    if (event.candidate) {
                        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                            this.ws.send(JSON.stringify({
                                type: 'webrtc_ice_candidate',
                                candidate: event.candidate
                            }));
                        }
                    }
                };

                // Handle connection state changes
                this.peerConnection.onconnectionstatechange = () => {
                    switch (this.peerConnection.connectionState) {
                        case 'connected':
                            this.rtcStatus = 'connected';
                            this.addLogEntry('success', 'WebRTC connected');
                            break;
                        case 'disconnected':
                        case 'failed':
                        case 'closed':
                            this.rtcStatus = 'disconnected';
                            this.addLogEntry('warning', `WebRTC ${this.peerConnection.connectionState}`);
                            break;
                    }
                };

                // Get user media (audio only) and add to peer connection
                navigator.mediaDevices.getUserMedia({ audio: true, video: false })
                    .then(stream => {
                        stream.getTracks().forEach(track => {
                            this.peerConnection.addTrack(track, stream);
                        });
                        this.addLogEntry('info', 'Added local audio stream');
                    })
                    .catch(error => {
                        this.addLogEntry('error', `Error accessing microphone: ${error.message}`);
                    });

                // Handle incoming audio
                this.peerConnection.ontrack = (event) => {
                    const remoteAudio = new Audio();
                    remoteAudio.srcObject = event.streams[0];
                    remoteAudio.play()
                        .catch(error => {
                            this.addLogEntry('error', `Error playing remote audio: ${error.message}`);
                        });
                    this.addLogEntry('info', 'Received remote audio stream');
                };
            },

            // Create and send WebRTC offer
            createOffer() {
                if (!this.peerConnection) {
                    this.addLogEntry('error', 'No peer connection available');
                    return;
                }

                this.peerConnection.createOffer()
                    .then(offer => {
                        return this.peerConnection.setLocalDescription(offer);
                    })
                    .then(() => {
                        // Send the offer to the server
                        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                            const offerMessage = {
                                type: 'webrtc_offer',
                                offer: this.peerConnection.localDescription,
                                config: {
                                    vad: this.config.vad,
                                    recording: this.config.recording,
                                    asr: this.config.asr
                                }
                            };

                            this.ws.send(JSON.stringify(offerMessage));
                            this.addLogEntry('info', 'Sent WebRTC offer');
                        }
                    })
                    .catch(error => {
                        this.addLogEntry('error', `Error creating offer: ${error.message}`);
                    });
            },

            // Create and send answer to an offer
            createAnswer() {
                if (!this.peerConnection) {
                    this.addLogEntry('error', 'No peer connection available');
                    return;
                }

                this.peerConnection.createAnswer()
                    .then(answer => {
                        return this.peerConnection.setLocalDescription(answer);
                    })
                    .then(() => {
                        // Send the answer to the server
                        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                            const answerMessage = {
                                type: 'webrtc_answer',
                                answer: this.peerConnection.localDescription
                            };

                            this.ws.send(JSON.stringify(answerMessage));
                            this.addLogEntry('info', 'Sent WebRTC answer');
                        }
                    })
                    .catch(error => {
                        this.addLogEntry('error', `Error creating answer: ${error.message}`);
                    });
            },

            // Save configuration to local storage
            saveConfigToLocalStorage() {
                localStorage.setItem('rustpbx_config', JSON.stringify(this.config));
            },

            // Load configuration from local storage
            loadConfigFromLocalStorage() {
                const savedConfig = localStorage.getItem('rustpbx_config');
                if (savedConfig) {
                    try {
                        const parsedConfig = JSON.parse(savedConfig);
                        // Deep merge to preserve defaults for any new config options
                        this.config = this.deepMerge(this.config, parsedConfig);
                        this.addLogEntry('info', 'Loaded configuration from local storage');
                    } catch (error) {
                        this.addLogEntry('error', `Error loading configuration: ${error.message}`);
                    }
                }
            },

            // Deep merge utility for objects
            deepMerge(target, source) {
                const result = { ...target };

                for (const key in source) {
                    if (source[key] instanceof Object && key in target) {
                        result[key] = this.deepMerge(target[key], source[key]);
                    } else {
                        result[key] = source[key];
                    }
                }

                return result;
            }
        };
    };
});
