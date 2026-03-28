# Current Media Layer Design

This document describes the current media-layer design after the peer/input/output refactor on branch `unified-session`.

It focuses on:

- `PeerInput` and `PeerOutput` as the two first-class poll-based abstractions
- `MediaBridge` as the thin bidirectional coordinator
- playback on the same output model
- what is still deferred

## Overview

The current media layer has three distinct shapes:

1. Anchored same-transport bridge
   - Core source types in [`src/media/source.rs`](../src/media/source.rs)
   - Peer abstractions in [`src/media/endpoint.rs`](../src/media/endpoint.rs)
   - Bridge coordinator in [`src/media/bridge.rs`](../src/media/bridge.rs)
   - This is the current design direction

2. Legacy media-stream / track layer
   - Still present in [`src/media/mod.rs`](../src/media/mod.rs)
   - Still used by older compatibility paths

3. Mixed WebRTC ↔ RTP transport bridge
   - Not yet reintroduced on top of the new peer abstraction
   - The session layer currently treats it as deferred work

So the system is still hybrid:

- anchored SIP↔SIP and WebRTC↔WebRTC media now use the peer/input/output model
- older media objects still exist for compatibility
- mixed transport bridging is intentionally not solved in this abstraction yet

## Main Design Goal

- keep the sender-poll model
- make peer direction explicit
- keep outbound switching at the peer output boundary
- keep per-direction media adaptation with the input adapted for the target output
- avoid generic stage pipelines as the design center

## File Layout

- `src/media/source.rs`
  - `PeerInputSource` trait
  - `MappedTrackInput` — per-direction PT admission, DTMF remap, transcode
  - `FileInput` — self-paced file playback
  - `IdleInput` — detached/silent source
  - per-direction mapping/transcode config types (`AudioMapping`, `DtmfMapping`, `TranscodeSpec`)

- `src/media/endpoint.rs`
  - `PeerInput` — inbound media abstraction (track or source)
  - `PeerOutput` — outbound media with switchable input and RTP continuity
  - `OutputRtpState` — outbound RTP timestamp/sequence rewriting
  - `receiver_track_for_pc()` — helper to extract receiver track from a PeerConnection

- `src/media/bridge.rs`
  - `DirectionConfig` — per-direction transform config
  - `MediaBridge` — bidirectional bridge coordinator

## Core Concepts

### `PeerInput`

`PeerInput` is the inbound media abstraction for one call leg.

It wraps either:
- an `Arc<dyn MediaStreamTrack>` (real inbound RTP from a peer connection)
- a `Box<dyn PeerInputSource>` (synthetic source like file playback or idle)

Key method:

```rust
pub fn adapted_for_output(
    &self,
    audio_mapping: Option<AudioMapping>,
    dtmf_mapping: Option<DtmfMapping>,
    transcode: Option<TranscodeSpec>,
) -> PeerInput
```

This builds a new `PeerInput` with per-direction adaptation (PT remapping, DTMF handling, transcoding) suitable for feeding into another peer's output.

Factory methods:
- `PeerInput::from_track(track)` — wrap an inbound media track
- `PeerInput::from_source(source)` — wrap a `PeerInputSource`
- `PeerInput::idle()` — silent/detached input
- `PeerInput::from_file(file_input)` — file playback input

### `PeerOutput`

`PeerOutput` is the outbound media abstraction for one call leg.

Responsibilities:

- stay attached to the target peer's sender/transceiver
- hold the current `PeerInput` as its active source
- allow replacement of that input without replacing the output object
- own outbound RTP continuity state

It implements `MediaStreamTrack`, so `rustrtc::RtpSender` polls it directly.

Switching API:

- `set_input(input: PeerInput)` — replace the active input
- `clear_input()` — switch to idle

Internally uses a `watch` channel to swap inputs safely while staying sender-polled.

Construction:

```rust
PeerOutput::attach(track_id, target_pc) -> Result<Arc<PeerOutput>>
```

This finds the audio transceiver on the target PeerConnection, builds a new RtpSender with the output as its track, and attaches it.

### `PeerInputSource`

`PeerInputSource` is the poll-based producer trait:

```rust
#[async_trait]
pub trait PeerInputSource: Send {
    async fn recv(&mut self) -> MediaResult<MediaSample>;
}
```

Current implementations:

- `MappedTrackInput` — reads from a peer's inbound track with PT filtering, DTMF remapping, and optional transcoding
- `FileInput` — self-paced file playback with encoder
- `IdleInput` — waits forever (detached state)

## Bridge Model

`MediaBridge` is a thin bidirectional coordinator.

It holds:

- caller `PeerInput` + `PeerOutput`
- callee `PeerInput` + `PeerOutput`
- one `DirectionConfig` for each direction

Its job is to wire:

- caller input → (adapted) → callee output
- callee input → (adapted) → caller output

Conceptually:

```text
caller.input → adapted_for_output(caller→callee config) → callee.output
callee.input → adapted_for_output(callee→caller config) → caller.output
```

Construction uses a builder pattern:

```rust
MediaBridge::builder(caller_input, caller_output, callee_input, callee_output)
    .caller_to_callee(DirectionConfig::from_profiles(caller_profile, callee_profile))
    .callee_to_caller(DirectionConfig::from_profiles(callee_profile, caller_profile))
    .build()
```

`bridge()` wires both directions. `unbridge()` sets both outputs to idle.

## Playback Model

Playback uses the same `PeerOutput` switching model as bridging.

The session layer builds a `FileInput` and installs it on the target leg output:

```text
FileInput → PeerInput::from_file() → PeerOutput → RtpSender
```

That means:

- playback and peer bridging use the same outbound switching boundary
- `PeerOutput` remains stable while the input changes

## RTP Continuity

Outbound RTP continuity is held in `PeerOutput` as `OutputRtpState`.

Current behavior:

- when `PeerOutput.recv()` gets an audio frame from the active input
- it rewrites the frame timestamp and sequence fields before returning it

This keeps continuity policy at the sender-facing boundary rather than inside any source implementation.

## Negotiation Assumptions

Same assumptions from [`src/media/negotiate.rs`](../src/media/negotiate.rs):

- the first answered audio codec is treated as the negotiated audio codec
- one DTMF entry is selected per leg
- direction config is derived from source-leg profile vs target-leg profile

## What Is Deferred

- mixed WebRTC ↔ RTP transport bridge on top of this peer abstraction
- recorder ownership redesign
- multi-consumer input fanout
- app/AVR peer kinds
- RWI media peer kind
- DTMF detection from RTP (IVR use case)
- migration/removal of all legacy track/media-stream compatibility code

## Current Mental Model

If you ignore the deferred pieces, the current media layer should be read like this:

1. Every SIP/WebRTC leg has a `PeerInput` and a `PeerOutput`
2. `PeerOutput` is always polled by the transport sender
3. `PeerInput` is adapted and set as the input on another peer's `PeerOutput`
4. `MediaBridge` just connects those two sides in each direction
5. File playback, idle, and peer media all use the same `PeerInput` → `PeerOutput` path

That is the current abstraction center of the media layer.
