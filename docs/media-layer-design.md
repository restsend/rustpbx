# Current Media Layer Design

This document describes the current media-layer design in this repository after the PeerEndpoint refactor on branch `unified-session`.

It focuses on:

- the PeerEndpoint abstraction for phone legs
- the MediaSource trait and composable pipeline
- recorder placement (source side)
- RTP timing placement (output side)
- playback on the same sender-polled output model
- the remaining legacy media pieces that are still outside this refactor

## Overview

The media layer currently has three shapes:

1. Anchored proxy-call bridge (PeerEndpoint model)
   - Core traits and sources in [`src/media/source.rs`](../src/media/source.rs)
   - Endpoint abstraction in [`src/media/endpoint.rs`](../src/media/endpoint.rs)
   - This is the current design direction
   - It is sender-polled and does not spawn a forwarding task

2. WebRTC <-> RTP transport bridge
   - Still implemented by the older task-based bridge in [`src/media/bridge.rs`](../src/media/bridge.rs)
   - This path is not migrated yet

3. Legacy track/media-stream layer
   - Still present for compatibility in [`src/media/mod.rs`](../src/media/mod.rs)
   - No longer the design center for anchored proxy bridging or playback

So the system is still hybrid:

- anchored bridge and playback use the new PeerEndpoint model
- transport bridge remains legacy

## Main Design Goal

- keep the sender-poll model
- keep media packet-oriented
- make source switching explicit
- keep outbound RTP continuity stable across source switching
- recording on the source (input) side, RTP timing on the output side
- clear PeerEndpoint abstraction for each phone leg
- composable pipeline via generic wrappers, boxed once at the PeerOutput boundary

## File Layout

- `src/media/source.rs` — core `MediaSource` trait, mapping config types, reusable sources and filters
- `src/media/endpoint.rs` — B2BUA phone-leg abstraction: `PeerOutput`, `PeerEndpoint`, `BidirectionalBridge`

## Core Concepts

### `MediaSource` (`source.rs`)

```rust
#[async_trait]
pub trait MediaSource: Send {
    async fn recv(&mut self) -> MediaResult<MediaSample>;
}
```

Pull-based media production. Any component producing or transforming packets implements this.

### Sources (`source.rs`)

- `TrackSource` — adapts `Arc<dyn MediaStreamTrack>` into `MediaSource`
- `FileSource` — playback from audio files, self-paced with 20ms ticker
- `IdleSource` — detached source, `recv()` waits forever

### Filters (`source.rs`)

- `TransformFilter<S: MediaSource>` — generic composable filter: PT filtering, DTMF remapping, transcoding. Wraps any upstream `MediaSource`.

### Taps (`endpoint.rs`, internal)

- `RecordingTap<S: MediaSource>` — generic tap: records each sample inline, passes through unmodified

### Composition

Pipeline stages are generic over their upstream source. Composition is zero-cost (monomorphized). Only one `Box<dyn MediaSource>` allocation happens when the final pipeline is installed on `PeerOutput`.

```
TrackSource → RecordingTap<TrackSource> → TransformFilter<RecordingTap<TrackSource>> → box → PeerOutput
```

Without recorder:
```
TrackSource → TransformFilter<TrackSource> → box → PeerOutput
```

### `PeerOutput` (`endpoint.rs`)

Output side of a phone endpoint.

It owns:
- the currently active `Box<dyn MediaSource>` (switchable)
- a `Notify` + `StdMutex<Option<...>>` for source replacement (latest always wins)
- one persistent outbound RTP continuity state (`OutputRtpState`)

It implements `MediaStreamTrack` so rustrtc's `RtpSender` can poll it.

Its job is:
- let `RtpSender` poll media via `recv()`
- switch to a new source when commanded (`set_source()`)
- keep outbound RTP sequence/timestamp continuity stable across source switches

Key method: `PeerOutput::attach(track_id, target_pc)` creates the output and installs it on the PeerConnection's audio transceiver.

Source switching uses one async Mutex (for the current source) and `Notify` to wake `recv()` when a replacement arrives — even if the current source is stuck (e.g. `IdleSource`).

### `PeerEndpoint` (`endpoint.rs`)

Represents one phone leg (caller or callee).

It owns:
- `receiver_track` — the peer's incoming RTP (single reader)
- `recorder` — optional recording binding (source side, pre-transform)
- `output` — `Arc<PeerOutput>` for sending RTP to this peer

Key method: `bridge_to(target_output, audio, dtmf, transcode)` builds a recording+transform pipeline from this endpoint's input and installs it as the source on the target output:

```
this.receiver_track → RecordingTap(recorder) → TransformFilter(remap/transcode) → target.output
```

### `DirectionConfig` (`endpoint.rs`)

Groups per-direction transform config (audio mapping, DTMF mapping, transcode spec).

`DirectionConfig::from_profiles(source, target)` builds the config from `NegotiatedLegProfile`s.

### `BidirectionalBridge` (`endpoint.rs`)

Holds two `PeerEndpoint`s and per-direction `DirectionConfig`s.

Built via builder pattern:
```rust
BidirectionalBridge::builder(caller, callee)
    .caller_to_callee(DirectionConfig::from_profiles(&caller_profile, &callee_profile))
    .callee_to_caller(DirectionConfig::from_profiles(&callee_profile, &caller_profile))
    .build()
```

- `bridge()` — wires both directions
- `unbridge()` — clears both outputs to idle

## Pipeline Examples

### Anchored proxy bridge (caller → callee):
```
caller.receiver_track → RecordingTap → TransformFilter → callee.PeerOutput → RtpSender
callee.receiver_track → RecordingTap → TransformFilter → caller.PeerOutput → RtpSender
```

### Playback (e.g. hold music to caller):
```
FileSource → caller.PeerOutput → RtpSender
```
RTP timing continues across source switch (peer → file → peer).

### IVR (future, no callee):
```
caller.receiver_track → RecordingTap → DtmfDetector (future)
FileSource → caller.PeerOutput → RtpSender
```

## RTP Continuity

Outbound RTP continuity is owned by `PeerOutput` (`OutputRtpState`).

For each outgoing audio sample, the output:
- rewrites sequence number
- rewrites RTP timestamp onto a continuous outbound timeline

Source switching does not reset the outbound sender timeline. `OutputRtpState` is the timestamp authority — sources set `sequence_number = Some(...)` so rustrtc treats frames as app-controlled and doesn't apply its own rewriting.

## Recording

Recording is source-side on the `PeerEndpoint` input.

When `PeerEndpoint::bridge_to()` builds the pipeline, if a recorder is attached, a `RecordingTap` wraps the `TrackSource` and records each packet before passing it to the `TransformFilter`.

Recording captures:
- pre-transcode
- pre-output-remap
- original incoming media for that leg

The recorder is shared (`Arc<StdMutex<Option<Recorder>>>`) between caller (leg A) and callee (leg B) for stereo WAV output.

## Concurrency Model

The current anchored bridge path does not spawn a forwarding worker.

`PeerOutput.recv()`:
- holds one async Mutex on the current source
- `select!`s between:
  - current source producing a sample
  - `Notify` signaling a replacement is pending

If a replacement arrives:
- the new source is swapped in from `StdMutex<Option<...>>`
- the old source is dropped
- latest source always wins (no channel backpressure)

## Negotiation Assumptions

Same as before — from [`src/media/negotiate.rs`](../src/media/negotiate.rs):
- the first answered audio codec is treated as the negotiated audio codec
- one DTMF entry is selected per leg

## What Is Still Legacy

- [`src/media/bridge.rs`](../src/media/bridge.rs) — task-based WebRTC <-> RTP bridge
- legacy `Track` / `MediaStream` compatibility in [`src/media/mod.rs`](../src/media/mod.rs)
- broader `MediaPeer` redesign

## Types Removed in This Refactor

- `DirectedLink` — was just a box around `Box<dyn MediaSource>`, now held directly by `PeerOutput`
- `LegOutput` — absorbed into `PeerOutput`
- `SenderTrackAdapter` — renamed to `PeerOutput`
- `PeerSourceConfig` — replaced by `DirectionConfig` + builder pattern
- `AdapterRtpState` — renamed to `OutputRtpState`
- `PeerSource` — merged into generic `TransformFilter<TrackSource>`
- `TransformSource` — merged into generic `TransformFilter<S>`
- `RecordingSource` — replaced by generic `RecordingTap<S>`
- `MediaSink` — removed (premature, no implementations)
