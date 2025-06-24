# RustPBX API Reference

This document provides comprehensive documentation for RustPBX's RESTful API and WebSocket interfaces.

## Base URL

```
http://localhost:8080
```

## Authentication

Most endpoints are currently open for development. Production deployments should implement proper authentication.

---

## Call Management API

### List Active Calls

Get a list of all currently active calls.

**Endpoint:** `GET /call/lists`

**Response:**
```json
{
  "calls": [
    {
      "id": "call-123e4567-e89b-12d3-a456-426614174000",
      "call_type": "WebSocket",
      "created_at": "2024-01-15T10:30:00Z",
      "option": {
        "caller": "alice@example.com",
        "callee": "bob@example.com",
        "denoise": true,
        "vad": {
          "type": "silero",
          "enabled": true
        }
      }
    }
  ]
}
```

### Terminate Call

Forcefully terminate an active call.

**Endpoint:** `POST /call/kill/{call_id}`

**Parameters:**
- `call_id` (path): The unique identifier of the call to terminate

**Response:**
```json
true
```

---

## WebSocket Call Interfaces

### WebRTC Call Connection

Establish a WebRTC-based call connection for browser clients.

**Endpoint:** `GET /call/webrtc`

**WebSocket Upgrade:** Required

**Query Parameters:**
- `id` (optional): Custom call session ID
- `dump` (optional): Whether to dump events, defaults to true

### SIP Call Connection

Establish a SIP-based call connection.

**Endpoint:** `GET /call/sip`

**WebSocket Upgrade:** Required

**Query Parameters:**
- `id` (optional): Custom call session ID
- `dump` (optional): Whether to dump events, defaults to true

### General WebSocket Call Interface

Generic call interface supporting multiple protocols.

**Endpoint:** `GET /call`

**WebSocket Upgrade:** Required

**Query Parameters:**
- `id` (optional): Custom call session ID
- `dump` (optional): Whether to dump events, defaults to true

---

## WebSocket Commands

Once connected to a WebSocket call endpoint, you can send the following commands:

### 1. Invite Command

Initiate a call with specified configuration.

```json
{
  "command": "invite",
  "option": {
    "caller": "alice@example.com",
    "callee": "bob@example.com",
    "denoise": true,
    "vad": {
      "type": "silero",
      "samplerate": 16000,
      "speechPadding": 160,
      "silencePadding": 200,
      "ratio": 0.5,
      "voiceThreshold": 0.5,
      "maxBufferDurationSecs": 50
    },
    "asr": {
      "provider": "tencent",
      "language": "zh-CN",
      "model": "16k_zh_en",
      "appId": "your_app_id",
      "secretId": "your_secret_id",
      "secretKey": "your_secret_key"
    },
    "tts": {
      "provider": "tencent",
      "speaker": "301030",
      "speed": 1.0,
      "volume": 5,
      "emotion": "neutral",
      "samplerate": 16000
    },
    "sip": {
      "username": "alice",
      "password": "secret123",
      "realm": "example.com",
      "headers": {
        "X-Custom-Header": "value"
      }
    },
    "recorder": {
      "recorderFile": "/path/to/recording.wav",
      "samplerate": 16000,
      "ptime": "200ms"
    }
  }
}
```

### 2. Accept Command

Accept an incoming call.

```json
{
  "command": "accept",
  "option": {
    "denoise": true,
    "codec": "pcmu"
  }
}
```

### 3. Reject Command

Reject an incoming call.

```json
{
  "command": "reject",
  "reason": "Busy",
  "code": 486
}
```

### 4. Text-to-Speech (TTS) Command

Convert text to speech and play it during the call.

```json
{
  "command": "tts",
  "text": "Hello, how can I help you today?",
  "speaker": "301030",
  "playId": "greeting-001",
  "autoHangup": false,
  "streaming": true,
  "endOfStream": false
}
```

### 5. Play Audio Command

Play an audio file from a URL.

```json
{
  "command": "play",
  "url": "https://example.com/audio/greeting.wav",
  "autoHangup": false
}
```

### 6. Call Transfer (REFER) Command

Transfer the call to another destination.

```json
{
  "command": "refer",
  "target": "sip:support@example.com",
  "options": {
    "bypass": false,
    "timeout": 30,
    "moh": "https://example.com/hold-music.wav",
    "autoHangup": true
  }
}
```

### 7. Call Control Commands

#### Hangup
```json
{
  "command": "hangup",
  "reason": "Normal clearing",
  "initiator": "user"
}
```

#### Mute/Unmute
```json
{
  "command": "mute",
  "trackId": "audio-track-1"
}
```

```json
{
  "command": "unmute",
  "trackId": "audio-track-1"
}
```

#### Pause/Resume Media
```json
{
  "command": "pause"
}
```

```json
{
  "command": "resume"
}
```

#### Interrupt Current Audio
```json
{
  "command": "interrupt"
}
```

### 8. Add Conversation History

Add a conversation entry to the call history for LLM context.

```json
{
  "command": "history",
  "speaker": "user",
  "text": "I need help with my account"
}
```

### 9. WebRTC ICE Candidates

Send ICE candidates for WebRTC connection establishment.

```json
{
  "command": "candidate",
  "candidates": [
    "candidate:1 1 UDP 2113667326 192.168.1.100 54400 typ host",
    "candidate:2 1 UDP 1677729535 203.0.113.2 54401 typ srflx raddr 192.168.1.100 rport 54400"
  ]
}
```

---

## LLM Proxy API

RustPBX includes a built-in LLM proxy for AI language model integration.

### Chat Completions

**Endpoint:** `POST /llm/v1/chat/completions`

**Headers:**
```
Content-Type: application/json
Authorization: Bearer <token> (optional)
```

**Request Body:**
```json
{
  "model": "qwen-turbo",
  "messages": [
    {
      "role": "system",
      "content": "You are a helpful assistant."
    },
    {
      "role": "user",
      "content": "Hello, how are you?"
    }
  ],
  "stream": true,
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "hangup",
        "description": "End the call with the user",
        "parameters": {
          "type": "object",
          "properties": {},
          "required": []
        }
      }
    }
  ]
}
```

**Response:** Streaming or non-streaming chat completion response compatible with OpenAI API format.

---

## ICE Servers API

Get STUN/TURN server configuration for WebRTC connections.

**Endpoint:** `GET /iceservers`

**Response:**
```json
[
  {
    "urls": ["stun:stun.l.google.com:19302"]
  },
  {
    "urls": ["turn:turn.example.com:3478"],
    "username": "user123",
    "credential": "pass456"
  }
]
```

---

## Configuration Schema

### CallOption Object

Complete configuration options for call setup:

```json
{
  "denoise": true,
  "offer": "SDP offer string for WebRTC",
  "callee": "destination@example.com",
  "caller": "source@example.com",
  "handshakeTimeout": "30s",
  "enableIpv6": false,
  "codec": "pcmu",
  "recorder": {
    "recorderFile": "/tmp/recordings/call.wav",
    "samplerate": 16000,
    "ptime": "200ms"
  },
  "vad": {
    "type": "silero",
    "samplerate": 16000,
    "speechPadding": 160,
    "silencePadding": 200,
    "ratio": 0.5,
    "voiceThreshold": 0.5,
    "maxBufferDurationSecs": 50
  },
  "asr": {
    "provider": "tencent",
    "language": "zh-CN",
    "model": "16k_zh_en",
    "appId": "your_app_id",
    "secretId": "your_secret_id",
    "secretKey": "your_secret_key",
    "modelType": "realtime",
    "bufferSize": 1024,
    "samplerate": 16000,
    "endpoint": "https://asr.tencentcloudapi.com/"
  },
  "tts": {
    "provider": "tencent",
    "speaker": "301030",
    "speed": 1.0,
    "volume": 5,
    "emotion": "neutral",
    "samplerate": 16000,
    "codec": "pcm",
    "subtitle": false,
    "appId": "your_app_id",
    "secretId": "your_secret_id",
    "secretKey": "your_secret_key"
  },
  "sip": {
    "username": "alice",
    "password": "secret123",
    "realm": "example.com",
    "headers": {
      "X-Custom-Header": "value"
    }
  },
  "eou": {
    "type": "tencent",
    "endpoint": "https://asr.tencentcloudapi.com/",
    "secretKey": "your_secret_key",
    "secretId": "your_secret_id",
    "timeout": 5000
  },
  "extra": {
    "customKey": "customValue"
  }
}
```

### VAD Options Details

Voice Activity Detection (VAD) configuration:

- `type`: VAD engine type ("webrtc", "silero", or others)
- `samplerate`: Sample rate (default: 16000)
- `speechPadding`: Padding before speech detection (default: 160)
- `silencePadding`: Padding after silence detection (default: 200)
- `ratio`: Threshold ratio for detection (default: 0.5)
- `voiceThreshold`: Voice threshold (default: 0.5)
- `maxBufferDurationSecs`: Maximum buffer duration (default: 50)

### ASR Options Details

Automatic Speech Recognition (ASR) configuration:

- `provider`: Provider ("tencent" or "voiceapi")
- `language`: Language code (e.g., "zh-CN", "en-US")
- `model`: Model name
- `appId`: Application ID (for Tencent Cloud)
- `secretId`: Secret ID
- `secretKey`: Secret Key
- `endpoint`: API endpoint URL

### TTS Options Details

Text-to-Speech (TTS) configuration:

- `provider`: Provider ("tencent" or "voiceapi")
- `speaker`: Speaker ID
- `speed`: Speech speed (default: 1.0)
- `volume`: Volume (0-10, default: 5)
- `emotion`: Emotion ("neutral", "happy", "sad", etc.)
- `samplerate`: Sample rate (default: 16000)
- `codec`: Audio codec (default: "pcm")

---

## Typical Usage Scenarios

### 1. AI Customer Service Bot

**Scenario:** Create an intelligent customer service system that can handle incoming calls, transcribe speech, process with LLM, and respond with synthetic voice.

**Implementation:**

1. **Setup WebSocket Connection:**
```javascript
const ws = new WebSocket('ws://localhost:8080/call/webrtc');
```

2. **Configure AI Services:**
```json
{
  "command": "invite",
  "option": {
    "denoise": true,
    "vad": {
      "type": "silero",
      "enabled": true
    },
    "asr": {
      "provider": "tencent",
      "language": "zh-CN"
    },
    "tts": {
      "provider": "tencent",
      "speaker": "301030"
    }
  }
}
```

3. **Handle Incoming Speech:**
```javascript
ws.onmessage = async (event) => {
  const data = JSON.parse(event.data);
  if (data.event === 'transcription_final') {
    // Process with LLM
    const response = await fetch('/llm/v1/chat/completions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        model: 'qwen-turbo',
        messages: [
          { role: 'system', content: 'You are a customer service assistant.' },
          { role: 'user', content: data.text }
        ]
      })
    });
    
    // Convert response to speech
    ws.send(JSON.stringify({
      command: 'tts',
      text: response.content,
      autoHangup: false
    }));
  }
};
```

### 2. Voice-Controlled Application

**Scenario:** Build a voice assistant that can execute commands based on speech input.

**Implementation:**

1. **Setup Voice Activity Detection:**
```json
{
  "command": "accept",
  "option": {
    "vad": {
      "type": "webrtc",
      "enabled": true,
      "voiceThreshold": 0.7
    },
    "asr": {
      "provider": "voiceapi",
      "language": "en-US"
    }
  }
}
```

2. **Process Commands:**
```javascript
ws.onmessage = (event) => {
  const data = JSON.parse(event.data);
  if (data.event === 'transcription_final') {
    const command = parseVoiceCommand(data.text);
    executeCommand(command);
    
    ws.send(JSON.stringify({
      command: 'tts',
      text: `Executing ${command}`,
      autoHangup: false
    }));
  }
};
```

### 3. WebRTC Contact Center

**Scenario:** Deploy a browser-based contact center with call routing and AI assistance.

**Implementation:**

1. **Agent Dashboard Connection:**
```javascript
const agentWs = new WebSocket('ws://localhost:8080/call/webrtc?id=agent-001');
```

2. **Incoming Call Handling:**
```json
{
  "command": "accept",
  "option": {
    "recorder": {
      "enabled": true,
      "samplerate": 16000
    },
    "asr": {
      "provider": "tencent",
      "language": "auto"
    }
  }
}
```

3. **AI-Assisted Responses:**
```javascript
// Real-time transcription for agent assistance
ws.onmessage = (event) => {
  const data = JSON.parse(event.data);
  if (data.event === 'transcription_delta') {
    updateRealtimeTranscript(data.text);
    // Get AI suggestions for agent
    getSuggestedResponses(data.text);
  }
};
```

### 4. SIP Phone Integration

**Scenario:** Connect traditional SIP phones with AI voice processing.

**Implementation:**

1. **SIP Registration:**
```json
{
  "command": "invite",
  "option": {
    "sip": {
      "username": "phone001",
      "password": "secure123",
      "realm": "pbx.company.com"
    },
    "caller": "sip:phone001@pbx.company.com",
    "callee": "sip:ivr@pbx.company.com"
  }
}
```

2. **Interactive Voice Response (IVR):**
```json
{
  "command": "tts",
  "text": "Welcome to Company Inc. Please say how I can help you today.",
  "speaker": "101001",
  "autoHangup": false
}
```

3. **Call Transfer Based on Intent:**
```javascript
// After processing user intent with LLM
if (intent === 'technical_support') {
  ws.send(JSON.stringify({
    command: 'refer',
    target: 'sip:tech-support@company.com',
    options: {
      timeout: 30,
      moh: 'https://company.com/hold-music.wav'
    }
  }));
}
```

---

## WebSocket Events

The server sends various events through WebSocket connections:

### Call Events
- `call_established`: Call connection successful
- `call_terminated`: Call ended
- `media_connected`: Media stream established

### Audio Processing Events
- `transcription_delta`: Partial speech recognition result
- `transcription_final`: Final speech recognition result
- `vad_speech_start`: Voice activity detected
- `vad_speech_end`: Voice activity ended
- `audio_playback_started`: TTS/audio playback started
- `audio_playback_finished`: TTS/audio playback completed

### Error Events
- `error`: General error message
- `transcription_error`: ASR processing error
- `synthesis_error`: TTS processing error
- `media_error`: Media processing error

---

## Error Codes

### HTTP Status Codes
- `200`: Success
- `400`: Bad Request - Invalid parameters
- `401`: Unauthorized - Authentication required
- `404`: Not Found - Resource not found
- `429`: Too Many Requests - Rate limit exceeded
- `500`: Internal Server Error - Server error
