function mainApp() {
    return {
        // Configuration state
        config: {
            denoise: {
                enabled: false,
            },
            vad: {
                type: 'webrtc',
                enabled: false,
                voiceThreshold: 0.6,
                ratio: 0.5,
                speechPadding: 160,
                silencePadding: 300,
            },
            recording: {
                enabled: false,
                samplerate: 16000,
            },
            asr: {
                provider: 'tencent',
                inactivityTimeout: 15000 // 15 seconds timeout for ASR inactivity
            },
            tts: {
                provider: 'tencent',
                speaker: '301030',
                greeting: "Hello, how can I help you today?"
            },
            llm: {
                useProxy: true,
                baseurl: '',
                apiKey: '',
                model: 'qwen-14b',
                prompt: 'You are a helpful assistant.'
            },
            // Add UI state for tabbed interface
            uiState: {
                activeTab: 'llm'
            }
        },

        // WebSocket connection
        ws: null,
        wsStatus: 'disconnected',

        // WebRTC connection
        peerConnection: null,
        rtcStatus: 'disconnected',
        callActive: false,

        // ASR inactivity tracking
        asrLastActivityTime: null,
        asrInactivityTimer: null,

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
            const hours = date.getHours().toString().padStart(2, '0');
            const minutes = date.getMinutes().toString().padStart(2, '0');
            const seconds = date.getSeconds().toString().padStart(2, '0');
            const milliseconds = date.getMilliseconds().toString().padStart(3, '0');
            return `${hours}:${minutes}:${seconds}.${milliseconds}`;
        },

        // Get CSS class for log entry based on type
        getLogEntryClass(type) {
            const classes = {
                'SYSTEM': 'bg-gray-100 text-gray-800',
                'VAD': 'bg-blue-100 text-blue-800',
                'ASR': 'bg-blue-100 text-blue-800',
                'LLM': 'bg-purple-100 text-purple-800',
                'TTS': 'bg-green-100 text-green-800',
                'ERROR': 'bg-red-100 text-red-800',
                'WARNING': 'bg-yellow-100 text-yellow-800',
                'METRICS': 'bg-indigo-100 text-indigo-800'
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

        // Clear all event log entries
        clearEventLog() {
            this.eventLog = [];
            this.addLogEntry('SYSTEM', 'Console cleared');
        },

        // Map legacy log types to new format
        mapLegacyLogType(type) {
            const typeMap = {
                'info': 'SYSTEM',
                'error': 'ERROR',
                'warning': 'WARNING',
                'success': 'SYSTEM',
                'VAD': 'VAD',
                'ASR': 'ASR',
                'LLM': 'LLM',
                'TTS': 'TTS'
            };
            return typeMap[type] || 'SYSTEM';
        },

        // Start monitoring ASR inactivity
        startAsrInactivityMonitor() {
            // Clear any existing timer
            this.stopAsrInactivityMonitor();

            // Set the initial activity timestamp
            this.asrLastActivityTime = Date.now();

            // Set up the timer to check for inactivity
            this.asrInactivityTimer = setInterval(() => {
                const currentTime = Date.now();
                const inactivityDuration = currentTime - this.asrLastActivityTime;

                // If inactivity exceeds the timeout, hang up
                if (inactivityDuration > this.config.asr.inactivityTimeout) {
                    this.addLogEntry('warning', `ASR inactivity timeout (${this.config.asr.inactivityTimeout}ms) exceeded. Hanging up.`);
                    this.endCall();
                    this.stopAsrInactivityMonitor();
                }
            }, 1000); // Check every second
        },

        // Stop monitoring ASR inactivity
        stopAsrInactivityMonitor() {
            if (this.asrInactivityTimer) {
                clearInterval(this.asrInactivityTimer);
                this.asrInactivityTimer = null;
            }
        },

        // Update ASR activity timestamp
        updateAsrActivity() {
            this.asrLastActivityTime = Date.now();
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
                    this.stopAsrInactivityMonitor();
                    this.endCall();
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
                        this.handleVadStatus({ active: false, duration: event.duration, startTime: event.startTime })
                        break
                    case 'asrFinal':
                        this.updateAsrActivity();
                        this.handleTranscriptionFinal(event)
                        break
                    case 'asrDelta':
                        this.updateAsrActivity();
                        this.handleTranscriptionDelta(event)
                        break
                    case 'metrics':
                        this.handleMetrics(event)
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
            })).then(() => {
                // Start ASR inactivity monitor after call is established
                this.startAsrInactivityMonitor();

                // Play greeting message when call is established
                if (this.config.tts.greeting && this.config.tts.greeting.trim() !== '') {
                    this.addLogEntry('info', 'Playing greeting message');
                    this.sendTtsRequest(this.config.tts.greeting);
                }
            })
        },
        handleHangup(event) {
            this.addLogEntry('info', `Call hungup: ${event.reason || 'No reason'}`);
            this.stopAsrInactivityMonitor();
            this.endCall();
        },
        handleTrackStart(event) {
            //this.addLogEntry('info', `track start`)
        },
        // Handle ASR result
        handleTranscriptionFinal(data) {
            this.lastAsrResult = data.text;
            // If we have a valid ASR result, process it with LLM
            if (data.text && data.text.trim() !== '') {
                this.addLogEntry('ASR', `ASR Final: ${data.text}`);
                this.processWithLlm(data.text).then().catch((reason) => {
                    this.addLogEntry('error', `processWithLlm failed: ${reason}`);
                })
            }
        },

        handleOther(event) {
            this.addLogEntry('info', JSON.stringify(event))
        },
        // Handle VAD status update
        handleVadStatus(event) {
            if (event.active) {
                this.addLogEntry('VAD', `Speech detected`);
                // Reset ASR activity timer when speech is detected
                this.updateAsrActivity();
            } else {
                this.addLogEntry('VAD', `Silence detected, duration: ${event.duration} ms, startTime: ${event.startTime} ms`);
            }
        },
        async playWav(url) {
            const playCommand = {
                command: 'play',
                url,
            };
            this.ws.send(JSON.stringify(playCommand));
            this.addLogEntry('info', 'play wav')
        },
        // Process text with LLM
        async processWithLlm(text) {
            // When using proxy, we use the internal endpoint directly
            let apiEndpoint;
            let headers = { 'Content-Type': 'application/json' };

            if (this.config.llm.useProxy) {
                // Use internal proxy
                apiEndpoint = '/llm/v1/chat/completions';
                // No Authorization header needed for proxy
            } else {
                // Use direct external endpoint
                if (!this.config.llm.baseurl) {
                    this.addLogEntry('warning', 'LLM configuration is incomplete');
                    return;
                }

                // Make the fetch request to the configured LLM endpoint
                let baseurl = this.config.llm.baseurl;
                if (!baseurl.endsWith('/')) {
                    baseurl += '/';
                }
                apiEndpoint = baseurl + 'chat/completions';

                // Add API key if present
                if (this.config.llm.apiKey) {
                    headers['Authorization'] = `Bearer ${this.config.llm.apiKey}`;
                }
            }

            // Create abort controller for the fetch request
            const controller = new AbortController();
            const signal = controller.signal;

            // Store the full LLM response
            let fullLlmResponse = '';
            let autoHangup = undefined;
            // Prepare the request payload
            const payload = {
                model: this.config.llm.useProxy ? 'qwen-14b' : this.config.llm.model,
                messages: [
                    { role: 'system', content: this.config.llm.prompt },
                    { role: 'user', content: text }
                ],
                stream: true,
                tools: [
                    {
                        type: "function",
                        function: {
                            name: "hangup",
                            description: "End the call with the user",
                            parameters: {
                                type: "object",
                                properties: {},
                                required: []
                            }
                        }
                    }
                ]
            };

            // Log request information
            this.addLogEntry('LLM', `Sending request to ${this.config.llm.useProxy ? 'local proxy' : 'external API'}`);

            let start = new Date();
            fetch(apiEndpoint, {
                method: 'POST',
                headers: headers,
                body: JSON.stringify(payload),
                signal: signal
            })
                .then(response => {
                    if (!response.ok) {
                        throw new Error(`HTTP error! Status: ${response.status}`);
                    }

                    if (!response.body) {
                        throw new Error('ReadableStream not supported in this browser.');
                    }

                    // Process the response as a stream
                    const reader = response.body.getReader();
                    const decoder = new TextDecoder();

                    // Function to process each chunk
                    const processStream = ({ done, value }) => {
                        if (done) {
                            let duration = new Date() - start;
                            // When stream is complete, send the full response for TTS
                            this.logEvent('LLM', `${duration} ms`, { llmResponse: fullLlmResponse });
                            this.sendTtsRequest(fullLlmResponse);
                            return;
                        }

                        // Decode the chunk
                        const chunk = decoder.decode(value, { stream: true });

                        // Process SSE format - each line starts with "data: "
                        const lines = chunk.split('\n');

                        lines.forEach(line => {
                            if (line.startsWith('data: ')) {
                                const data = line.substring(6);

                                // Handle special case for "[DONE]" message
                                if (data === '[DONE]') {
                                    return;
                                }

                                try {
                                    // Parse the JSON data
                                    const jsonData = JSON.parse(data);
                                    //let autoHangup = undefined
                                    // Check for tool calls (especially hangup)
                                    if (jsonData.choices && jsonData.choices[0].delta && jsonData.choices[0].delta.tool_calls) {
                                        const toolCalls = jsonData.choices[0].delta.tool_calls;
                                        toolCalls.forEach(toolCall => {
                                            if (toolCall.function && toolCall.function.name === 'hangup') {
                                                this.addLogEntry('LLM', 'LLM requested to hang up the call');
                                                autoHangup = true
                                                //this.endCall();
                                            }
                                        });
                                    }

                                    // Extract the content as before
                                    if (jsonData.choices && jsonData.choices[0].delta && jsonData.choices[0].delta.content) {
                                        const content = jsonData.choices[0].delta.content;
                                        fullLlmResponse += content;

                                        // Update the UI with the latest response
                                        this.lastLlmResponse = fullLlmResponse;
                                    }
                                } catch (error) {
                                    this.addLogEntry('error', `Error parsing SSE JSON: ${error.message}`);
                                }
                            }
                        });

                        // Continue reading the stream
                        return reader.read().then(processStream);
                    };

                    // Start processing the stream
                    return reader.read().then(processStream);
                })
                .catch(error => {
                    this.addLogEntry('error', `LLM API error: ${error.message}`);

                    // If there was already some response, send that for TTS
                    if (fullLlmResponse) {
                        this.sendTtsRequest(fullLlmResponse, autoHangup);
                    }
                });
        },

        // Send TTS request to the WebSocket
        sendTtsRequest(text, autoHangup) {
            if (!text || text.trim() === '') {
                this.addLogEntry('warning', 'Cannot send empty text to TTS');
                return;
            }

            if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                const ttsCommand = {
                    command: 'tts',
                    text: text,
                    autoHangup
                };

                this.ws.send(JSON.stringify(ttsCommand));
                this.addLogEntry('TTS', `${text.substring(0, 50)}${text.length > 50 ? '...' : ''}`);
                this.lastTtsMessage = text;
            } else {
                this.addLogEntry('error', 'WebSocket not connected, cannot send TTS request');
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
            this.stopAsrInactivityMonitor();

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
            let iceServers = await fetch('/iceservers').then(res => res.json())
            const configuration = {
                iceServers
            };

            let mediaStream = await navigator.mediaDevices.getUserMedia({
                audio: {
                    advanced: [{
                        echoCancellation: true,
                    }]
                }, video: false
            });

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
                let recorder = this.config.recording.enabled ? {
                    samplerate: this.config.recording.samplerate,
                    ptime: this.config.recording.ptime
                } : undefined;

                let vad = this.config.vad.enabled ? {
                    type: this.config.vad.type,
                    voiceThreshold: this.config.vad.type == 'silero' ? parseFloat(this.config.vad.voiceThreshold) : undefined,
                    ratio: parseFloat(this.config.vad.ratio),
                    speechPadding: parseInt(this.config.vad.speechPadding),
                } : undefined;
                let denoise = this.config.denoise.enabled ? true : undefined;
                const invite = {
                    command: 'invite',
                    options: {
                        offer: this.peerConnection.localDescription.sdp,
                        vad,
                        denoise,
                        asr: {
                            provider: this.config.asr.provider
                        },
                        recorder,
                        tts: {
                            provider: this.config.tts.provider,
                            speaker: this.config.tts.speaker || '301030'
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
        },

        // Handle metrics events
        handleMetrics(event) {
            // Format the metrics data for display
            let formattedMessage = '';

            // Extract the key parts from the metrics key (e.g., "ttfb.tts.tencent" -> "TTS Tencent TTFB")
            const keyParts = event.key.split('.');
            let metricType = '';
            let service = '';
            let metricName = '';

            if (keyParts.length >= 3) {
                metricName = keyParts[0].toUpperCase();
                service = keyParts[2].charAt(0).toUpperCase() + keyParts[2].slice(1);
                metricType = keyParts[1].toUpperCase();
            } else {
                formattedMessage = `Metrics: ${event.key}`;
            }

            if (metricName && service && metricType) {
                formattedMessage = `${service} ${metricName} ${metricType}`;
            }

            // Add duration information
            formattedMessage += ` (${event.duration}ms)`;
            // Log the formatted metrics
            this.logEvent('METRICS', formattedMessage, {
                metricsKey: event.key,
                metricsDuration: event.duration,
            });
        },
    }
}
