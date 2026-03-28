# Current Media Layer Design

This document describes the current media-layer design after the peer/input/output refactor on branch `unified-session`.

It focuses on:

- the bidirectional `MediaPeer` abstraction for SIP/WebRTC legs
- `PeerInput` and `PeerOutput` as the two first-class poll-based sides
- `OutputProvider` as the outbound media producer abstraction
- the current anchored bridge shape
- playback on the same output model
- what is still deferred

## Overview

The current media layer has three distinct shapes:

1. Anchored same-transport bridge
   - Core provider types in [`src/media/source.rs`](../src/media/source.rs)
   - Peer abstraction in [`src/media/endpoint.rs`](../src/media/endpoint.rs)
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
- model each media participant as one bidirectional peer
- keep outbound switching at the peer output boundary
- keep per-direction media adaptation with the provider built from peer input
- avoid generic stage pipelines as the design center

## File Layout

- `src/media/source.rs`
  - `OutputProvider`
  - `PeerInputProvider`
  - `FileOutputProvider`
  - `IdleOutputProvider`
  - per-direction mapping/transcode config types

- `src/media/endpoint.rs`
  - `PeerInput`
  - `PeerOutput`
  - media-layer `MediaPeer`
  - output RTP continuity state

- `src/media/bridge.rs`
  - `DirectionConfig`
  - `MediaBridge`

## Core Concepts

### `MediaPeer`

`MediaPeer` is the top-level media object for one call leg.

It owns:

- one `PeerInput`
- one `PeerOutput`

Meaning:

- `PeerInput` is media coming from the remote side into the system
- `PeerOutput` is media going from the system to the remote side

This is the target vocabulary for SIP/WebRTC peers in the current media layer.

### `PeerInput`

`PeerInput` is the inbound side of a media peer.

Current shape:

- wraps one inbound `Arc<dyn MediaStreamTrack>`
- is single-consumer in intent
- can build a direction-specific `OutputProvider` for some other peer’s output

Key method:

```rust
provider_for_output(audio_mapping, dtmf_mapping, transcode) -> Box<dyn OutputProvider>
```

That method is where per-direction adaptation is currently attached.

### `PeerOutput`

`PeerOutput` is the outbound side of a media peer.

Responsibilities:

- stay attached to the target peer’s sender/transceiver
- hold the current `OutputProvider`
- allow replacement of that provider without replacing the output object
- own outbound RTP continuity state

It implements `MediaStreamTrack`, so `rustrtc::RtpSender` polls it directly.

Current switching API:

- `install_provider(...)`
- `install_file_provider(...)`
- `clear_provider()`

Internally it uses `watch` to swap providers safely while staying sender-polled.

### `OutputProvider`

`OutputProvider` is the poll-based producer that feeds `PeerOutput`.

```rust
#[async_trait]
pub trait OutputProvider: Send {
    async fn recv(&mut self) -> MediaResult<MediaSample>;
}
```

Current provider kinds:

- `PeerInputProvider`
  - built from another peer’s inbound media
  - owns PT admission, DTMF remap/drop, and optional transcoding for one direction

- `FileOutputProvider`
  - playback from file
  - self-paced

- `IdleOutputProvider`
  - detached output state
  - produces no media

So the design-center abstraction is now:

- peer input
- peer output
- outbound provider

not generic pipeline stages.

## Bridge Model

`MediaBridge` is now a thin coordinator.

It owns:

- caller `MediaPeer`
- callee `MediaPeer`
- one `DirectionConfig` for each direction

Its job is only to wire:

- caller `input()` into callee `output()`
- callee `input()` into caller `output()`

by building direction-specific `PeerInputProvider`s.

Conceptually:

```text
caller.input -> PeerInputProvider(caller->callee mapping) -> callee.output
callee.input -> PeerInputProvider(callee->caller mapping) -> caller.output
```

No generic stage chain is required in the bridge layer.

## Playback Model

Playback uses the same `PeerOutput` switching model as bridging.

`SipSession` builds a `FileOutputProvider` and installs it on the target leg output:

```text
FileOutputProvider -> PeerOutput -> RtpSender
```

That means:

- playback and peer bridging use the same outbound switching boundary
- `PeerOutput` remains stable while the provider changes

## RTP Continuity

Outbound RTP continuity is currently held in `PeerOutput` as `OutputRtpState`.

Current behavior:

- when `PeerOutput.recv()` gets an audio frame from the active provider
- it rewrites the frame timestamp and sequence fields before returning it

This keeps continuity policy at the sender-facing boundary rather than inside:

- `MediaBridge`
- `PeerInputProvider`
- `FileOutputProvider`

This document does not claim that final wire ownership is fully solved yet relative to `rustrtc`; it only documents the current abstraction boundary in this repository.

## Recorder Status

Recorder behavior is not considered finished in this design.

Current code still allows `PeerInput` to carry recorder metadata and wrap a provider with recording behavior, but recorder ownership is not yet the finalized abstraction boundary.

So recorder should be treated as:

- present in code
- not the design center
- still subject to later cleanup

## Negotiation Assumptions

Same assumptions as before from [`src/media/negotiate.rs`](../src/media/negotiate.rs):

- the first answered audio codec is treated as the negotiated audio codec
- one DTMF entry is selected per leg
- direction config is derived from source-leg profile vs target-leg profile

## What Is Deferred

- mixed WebRTC ↔ RTP transport bridge on top of this peer abstraction
- recorder ownership redesign
- multi-consumer input fanout
- app/AVR peer kinds
- RWI media peer kind
- migration/removal of all legacy track/media-stream compatibility code

## Current Mental Model

If you ignore the deferred pieces, the current media layer should be read like this:

1. Every SIP/WebRTC leg is a `MediaPeer`
2. A `MediaPeer` has:
   - a `PeerInput`
   - a `PeerOutput`
3. `PeerOutput` is always polled by the transport sender
4. `PeerInput` is used to build an `OutputProvider` for some other peer’s `PeerOutput`
5. `MediaBridge` just connects those two sides in each direction

That is the current abstraction center of the media layer.
