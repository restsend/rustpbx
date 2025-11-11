function mainApp() {
    return {
        // Configuration state
        config: {
            callType: 'webrtc', // Default to WebRTC, can be 'webrtc' or 'sip'
            sip: {
                caller: '',
                callee: '',
                username: '',
                password: ''
            },
            denoise: {
                enabled: false,
            },
            vad: {
                type: 'silero',
                enabled: false,
                voiceThreshold: 0.5,
                ratio: 0.35,
                speechPadding: 250,
                silencePadding: 100,
                silenceTimeout: 5000 // 5 seconds timeout for silence
            },
            recording: {
                enabled: false,
                samplerate: 16000,
            },
            asr: {
                provider: 'tencent', // 'tencent' or 'voiceapi'
                appId: '',
                secretId: '',
                secretKey: '',
                model: '',
                language: 'zh-cn',
                endpoint: '',
                inactivityTimeout: 35000 // 35 seconds timeout for ASR inactivity
            },
            tts: {
                provider: 'tencent', // 'tencent' or 'voiceapi'
                appId: '',
                secretId: '',
                secretKey: '',
                endpoint: '',
                speaker: 'female', // '601003'
                speed: 1.0,
                volume: 5,
                greeting: "Hello, how can I help you today?"
            },
            llm: {
                useProxy: true,
                baseurl: '',
                apiKey: '',
                model: 'qwen-turbo',
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

        // DTMF related properties
        showDtmfKeypad: false,
        lastDtmfTone: null,
        dtmfSender: null,

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
            // Watch for config changes and save to localStorage
            this.$watch('config', () => {
                this.saveConfigToLocalStorage();
            }, { deep: true });
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
                'DTMF': 'bg-orange-100 text-orange-800',
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
            const wsUrl = `${protocol}//${window.location.host}/call/${this.config.callType}`;

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

            // For WebRTC calls, set the remote SDP
            if (this.config.callType === 'webrtc' && this.peerConnection) {
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
                });
            } else {
                // For SIP calls, we don't need to set remote SDP
                this.rtcStatus = 'connected';
                this.startAsrInactivityMonitor();

                // Play greeting message when call is established
                if (this.config.tts.greeting && this.config.tts.greeting.trim() !== '') {
                    this.addLogEntry('info', 'Playing greeting message');
                    this.sendTtsRequest(this.config.tts.greeting);
                }
            }
        },
        handleRinging(event) {
            this.addLogEntry('info', `Call is ringing`);
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

            // Generate a unique playId for this LLM request
            const playId = `llm-${Date.now()}-${Math.floor(Math.random() * 1000)}`;

            // Buffer for collecting text until punctuation
            let ttsBuffer = '';
            let firstSegmentUsage = undefined;
            let startTime = new Date();
            // Function to check for punctuation and send TTS if needed
            const processTtsSegment = (content) => {
                ttsBuffer += content;

                if (this.config.tts.provider == 'tencent_streaming') {
                    if (firstSegmentUsage === undefined) {
                        firstSegmentUsage = `TTFS: ${(new Date() - startTime)} ms`
                    }
                    this.sendTtsRequest(content, autoHangup, playId, firstSegmentUsage);
                    firstSegmentUsage = '';
                    return;
                }
                // Check if the buffer contains any punctuation
                // Matching for periods, question marks, exclamation marks, colons, semicolons
                const punctuationRegex = /([,，。.!?！？;；、\n])\s/g;
                let match;
                let lastIndex = 0;

                while ((match = punctuationRegex.exec(ttsBuffer)) !== null) {
                    // Extract the segment up to and including the punctuation
                    const segment = ttsBuffer.substring(lastIndex, match.index + 1);
                    if (segment.trim().length > 0) {
                        // Send this segment to TTS with the same playId
                        if (firstSegmentUsage === undefined) {
                            firstSegmentUsage = `TTFS: ${(new Date() - startTime)} ms`
                        }
                        this.sendTtsRequest(segment, autoHangup, playId, firstSegmentUsage);
                        firstSegmentUsage = ''
                    }
                    lastIndex = match.index + 1;
                }

                // Keep the remainder in the buffer
                if (lastIndex > 0) {
                    ttsBuffer = ttsBuffer.substring(lastIndex);
                }
            };

            // Prepare the request payload
            const payload = {
                model: this.config.llm.useProxy ? 'qwen3-14b' : this.config.llm.model,
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
                            if (this.config.tts.provider == 'tencent_streaming') {
                                this.sendTtsRequest("", autoHangup, playId, undefined, true);
                            }
                            // When stream is complete, send any remaining text in the buffer
                            if (ttsBuffer.trim().length > 0 && this.config.tts.provider != 'tencent_streaming') {
                                this.sendTtsRequest(ttsBuffer, autoHangup, playId);
                            }
                            this.logEvent('LLM', `${duration} ms`, { llmResponse: fullLlmResponse });
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

                                    // Check for tool calls (especially hangup)
                                    if (jsonData.choices && jsonData.choices[0].delta && jsonData.choices[0].delta.tool_calls) {
                                        const toolCalls = jsonData.choices[0].delta.tool_calls;
                                        toolCalls.forEach(toolCall => {
                                            if (toolCall.function && toolCall.function.name === 'hangup') {
                                                autoHangup = true;
                                            }
                                        });
                                    }

                                    // Extract the content as before
                                    if (jsonData.choices && jsonData.choices[0].delta && jsonData.choices[0].delta.content) {
                                        const content = jsonData.choices[0].delta.content;
                                        fullLlmResponse += content;

                                        // Process the content for TTS streaming
                                        processTtsSegment(content);

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
                    if (fullLlmResponse && this.config.tts.provider != 'tencent_streaming') {
                        this.sendTtsRequest(fullLlmResponse, autoHangup, playId, '');
                    }
                });
        },
        doHangup() {
            this.addLogEntry('info', 'Hanging up the call');
            // Send hangup command to WebSocket
            if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                this.ws.send(JSON.stringify({ command: 'hangup' }));
            }
            this.endCall();
        },
        // Send TTS request to the WebSocket
        sendTtsRequest(text, autoHangup, playId, firstSegmentUsage = undefined, endOfStream = false) {
            if ((!text || text.trim() === '') && !endOfStream) {
                this.addLogEntry('warning', 'Cannot send empty text to TTS');
                return;
            }
            if (firstSegmentUsage === undefined) {
                firstSegmentUsage = ''
            }

            if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                const ttsCommand = {
                    command: 'tts',
                    text: text,
                    autoHangup,
                    playId,
                    endOfStream
                };

                this.ws.send(JSON.stringify(ttsCommand));
                this.addLogEntry('TTS', `${text.substring(0, 50)}${text.length > 50 ? '...' : ''}${playId ? ` (playId: ${playId} ${firstSegmentUsage})` : ''}`);
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

        // Toggle DTMF keypad visibility
        toggleDtmfKeypad() {
            this.showDtmfKeypad = !this.showDtmfKeypad;
        },

        // Send DTMF tone
        sendDtmfTone(tone) {
            if (!this.peerConnection || !this.callActive) {
                this.addLogEntry('error', 'Cannot send DTMF tone: No active call');
                return;
            }

            if (!this.dtmfSender) {
                // Create DTMF sender if it doesn't exist
                const senders = this.peerConnection.getSenders();
                const audioSender = senders.find(sender => sender.track && sender.track.kind === 'audio');

                if (audioSender) {
                    this.dtmfSender = audioSender.dtmf;
                    if (!this.dtmfSender) {
                        this.addLogEntry('error', 'DTMF sending not supported by the browser');
                        return;
                    }
                } else {
                    this.addLogEntry('error', 'No audio sender available for DTMF');
                    return;
                }
            }

            // Send the DTMF tone
            this.dtmfSender.insertDTMF(tone);
            this.lastDtmfTone = tone;

            // Log to debug console with special DTMF type
            this.logEvent('DTMF', `Sent DTMF tone: ${tone}`);
        },

        async prepareCall() {
            if (this.config.callType === 'webrtc') {
                // For WebRTC calls, set up peer connection
                await this.setupPeerConnection();
            } else {
                // For SIP calls, we don't need to set up peer connection
                // Just proceed with the call
                this.addLogEntry('info', 'Setting up SIP call');
            }

            this.callActive = true;
            this.addLogEntry('info', `Starting ${this.config.callType.toUpperCase()} call...`);

            if (this.config.callType === 'webrtc') {
                // Create and send offer for WebRTC call
                this.createOffer();
            } else {
                // For SIP calls, just send the invite with SIP details
                this.sendInvite();
            }
        },

        // End a call
        endCall() {
            this.stopAsrInactivityMonitor();

            if (this.peerConnection) {
                this.peerConnection.close();
                this.peerConnection = null;
                this.dtmfSender = null; // Reset DTMF sender
                this.showDtmfKeypad = false; // Hide keypad if open
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
            let iceServers = {
                urls: ['stun:stun.l.google.com:19302']
            }

            try {
                iceServers = await fetch('/iceservers').then(res => res.json())
            } catch { }

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
                const sender = this.peerConnection.addTrack(track, mediaStream);

                // Set up DTMF sender for audio track
                if (track.kind === 'audio' && sender.dtmf) {
                    this.dtmfSender = sender.dtmf;
                    this.addLogEntry('info', 'DTMF sender initialized');
                }
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

                // Set up DTMF sender for the audio track
                const senders = this.peerConnection.getSenders();
                const audioSender = senders.find(sender => sender.track && sender.track.kind === 'audio');
                if (audioSender && audioSender.dtmf) {
                    this.dtmfSender = audioSender.dtmf;
                    this.addLogEntry('info', 'DTMF sender initialized');
                }
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
                    silencePadding: parseInt(this.config.vad.silencePadding),
                    silenceTimeout: parseInt(this.config.vad.silenceTimeout),
                } : undefined;
                let denoise = this.config.denoise.enabled ? true : undefined;

                // Build ASR configuration
                let asrConfig = {
                    provider: this.config.asr.provider
                };

                // Add provider-specific ASR configuration
                if (this.config.asr.provider === 'tencent') {
                    if (this.config.asr.appId) asrConfig.appId = this.config.asr.appId;
                    if (this.config.asr.secretId) asrConfig.secretId = this.config.asr.secretId;
                    if (this.config.asr.secretKey) asrConfig.secretKey = this.config.asr.secretKey;
                    if (this.config.asr.model) asrConfig.modelType = this.config.asr.model;
                    if (this.config.asr.language) asrConfig.language = this.config.asr.language;
                } else if (this.config.asr.provider === 'voiceapi') {
                    if (this.config.asr.endpoint) asrConfig.endpoint = this.config.asr.endpoint;
                    if (this.config.asr.model) asrConfig.modelType = this.config.asr.model;
                    if (this.config.asr.language) asrConfig.language = this.config.asr.language;
                }

                // Build TTS configuration
                let speaker;
                switch (this.config.tts.provider) {
                    case 'tencent':
                        if (this.config.tts.speaker == 'male') {
                            speaker = '601004';
                        } else {
                            speaker = '601003';
                        }
                        break;
                    case 'aliyun':
                        if (this.config.tts.speaker == 'male') {
                            speaker = 'longanyun';
                        } else {
                            speaker = 'longyumi_v2';
                        }
                        break;
                }
                let ttsConfig = {
                    provider: this.config.tts.provider,
                    speaker
                };
                // Add provider-specific TTS configuration
                if (this.config.tts.provider === 'tencent' || this.config.tts.provider === 'tencent_streaming') {
                    if (this.config.tts.appId) ttsConfig.appId = this.config.tts.appId;
                    if (this.config.tts.secretId) ttsConfig.secretId = this.config.tts.secretId;
                    if (this.config.tts.secretKey) ttsConfig.secretKey = this.config.tts.secretKey;
                    if (this.config.tts.speed) ttsConfig.speed = this.config.tts.speed;
                    if (this.config.tts.volume) ttsConfig.volume = this.config.tts.volume;
                } else if (this.config.tts.provider === 'voiceapi') {
                    if (this.config.tts.endpoint) ttsConfig.endpoint = this.config.tts.endpoint;
                }

                const invite = {
                    command: 'invite',
                    option: {
                        asr: asrConfig,
                        recorder,
                        tts: ttsConfig,
                        vad,
                        denoise
                    },
                };

                // Add different parameters based on call type
                if (this.config.callType === 'webrtc') {
                    // For WebRTC calls, add the offer SDP
                    invite.option.offer = this.peerConnection.localDescription.sdp;
                } else if (this.config.callType === 'sip') {
                    // For SIP calls, add the caller and callee information
                    invite.callType = 'sip';
                    invite.option.caller = this.config.sip.caller;
                    invite.option.callee = this.config.sip.callee;

                    // Add SIP credentials if provided
                    if (this.config.sip.username || this.config.sip.password) {
                        invite.option.sip = {
                            username: this.config.sip.username,
                            password: this.config.sip.password
                        };
                    }

                    // Validate SIP parameters
                    if (!invite.option.caller || !invite.option.callee) {
                        this.addLogEntry('error', 'SIP call requires both caller and callee to be specified');
                        return;
                    }
                }

                this.ws.send(JSON.stringify(invite));
                this.addLogEntry('info', `Sent ${this.config.callType.toUpperCase()} invite`);
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
