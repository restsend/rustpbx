# RustPBX

RustPBX is a high-performance, secure PBX (Private Branch Exchange) system implemented in Rust, designed to support AI-powered communication pipelines. This project is currently a work in progress (WIP).

## Overview

RustPBX aims to revolutionize voice communication systems by integrating modern AI capabilities with traditional PBX functionality, all while leveraging Rust's safety and performance benefits.

### Key Features

- **AI Pipeline Integration**: Support for speech recognition, transcription, and natural language processing
- **High Performance**: Built with Rust for optimal speed and resource efficiency
- **Enhanced Security**: Memory-safe design to minimize security vulnerabilities
- **Media Processing**: Robust audio processing with support for various codecs
- **Extensible Architecture**: Modular design for easy integration of new capabilities

## Media Processing System

The core of RustPBX includes a robust media processing system for handling WebRTC/RTP audio streams, with support for:

- **Audio Codecs**
  - G.711 μ-law (PCMU)
  - G.711 A-law (PCMA)
  - G.722

- **Voice Activity Detection (VAD)**
  - WebRTC VAD implementation
  - Voice Activity Detector implementation
  - Configurable VAD type selection

- **Noise Reduction**
  - RNNoise-based noise suppression
  - Frame-based processing
  - Support for different sample rates and channels

- **Speech Recognition**
  - ASR integration
  - Real-time transcription
  - Support for word-level and segment-level results

- **Media Stream Management**
  - Track-based architecture
  - Event-driven design
  - Support for multiple concurrent tracks
  - Recording capability

## Requirements

- Rust 1.75 or later
- Cargo package manager

## Project Status

This project is currently in active development (WIP). The core functionality is being implemented and refined. We welcome contributions and feedback from the community.

## License

MIT License
