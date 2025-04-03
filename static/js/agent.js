function mainApp() {
    return {
        // Configuration state
        config: {
            vad: {
                type: 'webrtc',
                enabled: false,
                threshold: 0.5
            },
            recording: {
                enabled: false
            },
            asr: {
                provider: 'tencent',
                appId: '',
                secretId: '',
                secretKey: ''
            },
            tts: {
                provider: 'tencent',
                appId: '',
                secretId: '',
                secretKey: '',
                speaker: '1',
            },
            llm: {
                endpoint: '',
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
            const wsUrl = `${protocol}//${window.location.host}/call/webrtc`;

            this.addLogEntry('info', `Connecting to WebSocket at ${wsUrl}`);

            try {
                this.ws = new WebSocket(wsUrl);

                this.ws.onopen = () => {
                    this.wsStatus = 'connected';
                    this.addLogEntry('success', 'WebSocket connected');
                    this.prepareCall().then().catch((reason) => {
                        this.addLogEntry('warning', `prepareCall failed: ${reason}`);
                    })
                };

                this.ws.onclose = () => {
                    this.wsStatus = 'disconnected';
                    this.addLogEntry('warning', 'WebSocket disconnected');
                    this.endCall()
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
        handleWebSocketMessage(ev) {
            const event = JSON.parse(ev.data);
            try {
                switch (event.event) {
                    case 'hangup':
                        this.handleHangup(event)
                        break
                    case 'trackStart':
                        this.handleTrackStart(event)
                        break
                    case 'answer':
                        this.handleAnswer(event)
                        break
                    case 'ringing':
                        this.handleRinging(event)
                        break
                    case 'speaking':
                        this.handleVadStatus({ active: true })
                        break
                    case 'silence':
                        this.handleVadStatus({ active: false })
                        break
                    case 'transcriptionFinal':
                        this.handleTranscriptionFinal(event)
                        break
                    case 'transcriptionDelta':
                        this.handleTranscriptionDelta(event)
                        break
                    default:
                        this.handleOther(event)
                }
            } catch (error) {
                this.addLogEntry('error', `Error processing WebSocket message: ${error.message} ${event}`);
            }
        },
        handleTranscriptionDelta(data) {
            this.lastAsrResult = data.text
        },
        handleAnswer(event) {
            this.addLogEntry('info', `Call answered`);
            this.peerConnection.setRemoteDescription(new RTCSessionDescription({
                type: 'answer',
                sdp: event.sdp
            })).then()
        },
        handleTrackStart(event) {
            //this.addLogEntry('info', `track start`)
        },
        // Handle ASR result
        handleTranscriptionFinal(data) {
            this.lastAsrResult = data.text;
            // If we have a valid ASR result, process it with LLM
            if (data.text && data.text.trim() !== '') {
                this.addLogEntry('info', `ASR Final: ${data.text}`);
                this.processWithLlm(data.text);
            }
        },

        handleOther(event) {
            this.addLogEntry('info', JSON.stringify(event))
        },
        // Handle VAD status update
        handleVadStatus(event) {
            this.addLogEntry('info', `VAD Status: ${event.active ? 'Speech detected' : 'Silence detected'}`);
        },
        // Process text with LLM
        processWithLlm(text) {
            if (!this.config.llm.endpoint) {
                this.addLogEntry('warning', 'LLM configuration is incomplete');
                return;
            }

            if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                // const llmRequest = {
                //     type: 'llm_request',
                //     text: text,
                //     config: {
                //         provider: this.config.llm.provider,
                //         api_key: this.config.llm.apiKey,
                //         model: this.config.llm.model,
                //         prompt: this.config.llm.prompt
                //     }
                // };

                // this.ws.send(JSON.stringify(llmRequest));
                // this.addLogEntry('info', 'Sent LLM request');
            }
        },

        // Start a call
        startCall() {
            this.saveConfigToLocalStorage();
            this.connectWebSocket();
        },
        async prepareCall() {
            await this.setupPeerConnection();
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
                this.ws.send(JSON.stringify({ command: 'hangup' }));
            }
        },

        // Set up WebRTC peer connection
        async setupPeerConnection() {
            // Close existing connection if any
            if (this.peerConnection) {
                this.peerConnection.close();
            }

            const configuration = {
            };

            let mediaStream = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });

            this.peerConnection = new RTCPeerConnection(configuration);
            mediaStream.getTracks().forEach(track => {
                this.addLogEntry('info', 'Added local audio stream');
                this.peerConnection.addTrack(track, mediaStream);
            });

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
            // Handle ICE candidate events
            this.peerConnection.onicecandidate = (event) => {
                if (!event.candidate) {
                    this.sendInvite()
                    return
                }
            };
        },

        // Create and send WebRTC offer
        async createOffer() {
            if (!this.peerConnection) {
                this.addLogEntry('error', 'No peer connection available');
                return;
            }
            const offer = await this.peerConnection.createOffer({
                offerToReceiveAudio: true,
                offerToReceiveVideo: false
            });
            await this.peerConnection.setLocalDescription(offer);
        },

        sendInvite() {
            if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                const invite = {
                    command: 'invite',
                    options: {
                        offer: this.peerConnection.localDescription.sdp,
                        vad: this.config.vad,
                        asr: {
                            provider: this.config.asr.provider,
                            appId: this.config.asr.appId || undefined,
                            secretId: this.config.asr.secretId || undefined,
                            secretKey: this.config.asr.secretKey || undefined,
                        },
                        tts: {
                            provider: this.config.tts.provider,
                            appId: this.config.tts.appId || undefined,
                            secretId: this.config.tts.secretId || undefined,
                            secretKey: this.config.tts.secretKey || undefined,
                        }
                    },
                };
                this.ws.send(JSON.stringify(invite));
                //this.addLogEntry('info', 'Sent WebRTC offer');
            }
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
    }
}
