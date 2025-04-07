# RustPBX - secure software-defined PBX (SD-PBX)

RustPBX is a high-performance, secure software-defined PBX (Private Branch Exchange) system implemented in Rust, designed to support AI-powered communication pipelines.

## Overview

RustPBX enables real-time control of call states, transcription, and TTS through WebSocket interfaces. It integrates seamlessly with modern communication infrastructure while maintaining compatibility with traditional telephony systems.

### Key Features

- **WebSocket Control Interface**: Manage call states, real-time transcription, and TTS commands via WebSocket
- **AI Service Integration**: Direct integration with mainstream ASR/TTS service providers
- **Modern Web Connectivity**: Support for SIP.js and WebRTC over WSS, providing firewall-friendly and secure communications
- **Traditional SBC Capabilities**: Functions as a Session Border Controller with high-performance proxy features
- **Enterprise-Grade Features**: Includes CDR (Call Detail Records), authentication, and call forwarding functionality

## Requirements

- Rust 1.75 or later
- Cargo package manager

## Project Status

This project is currently in active development (WIP). The core functionality is being implemented and refined. We welcome contributions and feedback from the community.

## License

MIT License
