# Media, Recording & CDR

## Media Proxy

Controls how RTP traffic is handled. Configurable at two levels:

### Server-level (`[proxy]`)

Sets the default for all calls. Trunk-level config overrides this when present.

```toml
[proxy]
# Modes: 
# - "auto": Bridge RTP only when necessary (e.g. WebRTC <-> UDP, or NAT detected)
# - "all":  Always bridge RTP (B2BUA style)
# - "nat":  Bridge only if private IP is detected
# - "none": Direct media (signaling only)
# - "bypass": SDP rewrite only, RTP direct (experimental)
media_proxy = "auto"

# Codec Negotiation
codecs = ["opus", "pcmu", "pcma", "g729"]

# RTP external/bind IP
# external_ip = "203.0.113.1"   # IP advertised in SDP c=/o= and ICE candidates
# auto_external_ip = "http://ifconfig.me"  # Auto-detect external IP
# bind_ip = "0.0.0.0"           # Local RTP socket bind address

# RTP port range
# rtp_start_port = 10000
# rtp_end_port   = 20000

# Latching (learn callee's actual RTP address from first received packet)
# enable_latching = true
# latching_probation_max_packets = 6
```

### Per-trunk override (`[proxy.trunks.<name>]`)

Overrides the server-level `media_proxy` for calls routed through this trunk.
Useful when some trunks need different media handling (e.g. overlay-network call
termination, or forcing anchor for NAT-prone endpoints).

```toml
[proxy.trunks.overlay_trunk]
dest = "sip:overlay.example:5060"

# Trunk-level media mode (overrides server's media_proxy):
# - "auto":           bridge only for app/queue flows
# - "none":           no media proxy (SDP passthrough, RTP direct)
# - "bypass":         SDP rewrite only, RTP direct
# - "force_transcode": always bridge through PBX (equivalent to server "all")
media_mode = "force_transcode"

# Per-trunk SDP IP override (overrides server's external_ip):
# Use when this trunk terminates on an overlay network (Tailscale/WireGuard)
# that needs a different advertised IP than the public NAT address.
external_ip = "100.64.10.1"
bind_ip = "100.64.10.2"

# Codecs (overrides server-level codecs)
codec = ["opus", "pcmu"]
```

### Latching

When `enable_latching = true` (default), the PBX learns a callee's actual RTP
address from the first received packet, even if the callee's SDP advertises a
private LAN IP. This makes `media_proxy = "auto"` or `"force_transcode"` work
correctly with NAT'd endpoints without needing explicit SDP rewriting.

**Server-level defaults:**
```toml
[proxy]
enable_latching = true
latching_probation_max_packets = 6
```

**Per-trunk override:**
```toml
[proxy.trunks.example]
enable_latching = false
```

### ICE-lite (`ice_lite`)

Answer/offer SDP on **plain-RTP legs** advertises `a=ice-lite` (RFC 8445 §2.4).
The PBX then runs as a *Controlled ICE-lite* agent: it starts no connectivity
checks and answers the remote full-ICE peer's checks against the candidates in
its SDP.

**When to use it:**

- Strict full-ICE endpoints that never fall back to plain RTP when the answer
  lacks ICE attributes — most notably **Microsoft Teams Direct Routing**, which
  requires the SBC side to answer as ICE-lite.
- PBX deployments behind NAT where the peer's full ICE agent should discover
  the correct return path via checks instead of relying on the SDP address.

**Behavior and safety:**

- Endpoints **without** ICE support ignore the ICE attributes entirely and keep
  using direct RTP + symmetric latching — enabling the flag does not break
  plain SIP phones.
- **WebRTC legs are unaffected**: browsers always run full ICE + DTLS, and the
  flag is forced off for them (and for SDES/Srtp legs) regardless of config.
- A non-ICE peer sending RTP before checks complete keeps media flowing; the
  PBX's ICE-lite answerer accepts RTP from the peer's signaling address and
  latches.

**Server-level default (`[media]`):**
```toml
[media]
ice_lite = false
```

**Per-trunk override** — wins over the global default and the per-extension
detection:
```toml
[proxy.trunks.teams]
dest = "sip:pstn.teams.microsoft.com:5061"
ice_lite = true   # false = explicitly off; unset = inherit global
```
Console: *Trunk → Media Options → ICE-Lite* (persisted as `metadata.sbc.ice_lite`).

**Per-extension (automatic):** extensions that registered with the RFC 5768
`;+sip.ice` Contact parameter on a non-WebSocket transport are answered as
ICE-lite automatically — no configuration needed. WebSocket/WebRTC endpoints
are excluded since they always negotiate full ICE.

| Scenario | Global `[media] ice_lite` | Trunk `ice_lite` | Caller registered `;+sip.ice` | Session RTP legs |
|----------|---------------------------|------------------|-------------------------------|------------------|
| Plain carrier trunk | off (default) | — | — | no ICE attributes (legacy) |
| Teams Direct Routing trunk | off | `true` | — | `a=ice-lite` on this trunk's legs |
| ICE-capable extension (UDP/TCP) | off | — | yes | `a=ice-lite` answers |
| Trunk opt-out for detected caller | off | `false` | yes | off (trunk wins) |
| Global rollout | `true` | — | — | `a=ice-lite` everywhere (RTP legs) |

### Choosing the right combination (Bug 1 + 2 scenarios)

| Scenario | Server `media_proxy` | Trunk `media_mode` | Trunk `external_ip` | Result |
|----------|---------------------|--------------------|--------------------|--------|
| Normal SIP trunk (public) | `auto` (default) | — | — | Bypass for direct calls, anchor for app/queue |
| Overlay-network trunk | `auto` | `force_transcode` | `100.64.x.x` | Anchored through PBX, overlay IP in SDP |
| Outbound trunk (NAT'd callee) | `auto` | `force_transcode` | — | Anchored + latching handles NAT |
| Direct extension calls | `auto` | — | — | Bypass, SIP handles NAT naturally |

### Relay-only WebRTC legs (`relay_only`)

Per-dialplan switch (`media.relay_only`, default `false`). When enabled, the
call's WebRTC legs advertise — and only check — TURN **relay** candidates:
host and server-reflexive candidates are omitted from the SDP answer, so the
remote peer can only reach the PBX through the TURN allocation.

#### When to use it: the EIP NAT case

A PBX host behind a 1:1 NAT (e.g. a cloud elastic IP) exhibits a specific
failure mode with WebRTC caller legs:

- The pair "browser srflx ↔ PBX host (NAT'd)" **passes a single STUN binding
  check**, so the browser (ICE controlling) nominates it and the SIP call
  proceeds normally.
- The following **DTLS handshake datagrams never arrive** at the PBX — the
  NAT/firewall passes small STUN payloads, but drops the DTLS payload that
  follows on flows the PBX has not initiated outbound itself.
- Result: SIP `200 OK` answered, `PC state New → Failed` ~30 s later, zero
  media from the caller leg, empty recording. The same agent **answering** a
  PBX-initiated call works, because there the PBX (ICE controlling) sends the
  first packet and registers the NAT flow.

Setting `relay_only = true` moves DTLS onto the `browser → TURN → PBX` path,
which bypasses the NAT entirely.

```toml
# dialplan media config (route `media` section / API dialplans)
[media]
relay_only = true
# Requires ICE servers with a turn: URL (server [rtp] ice_servers or the
# dialplan ice_servers) — relay-only without a TURN server yields no usable
# candidates.
```

**Operational notes**

- TURN becomes the media path for relay-only calls: size the coturn box
  (bandwidth ≈ 2 × audio bitrate per call) and monitor it; if TURN is down,
  relay-only calls cannot establish media.
- Rollback is immediate: set `relay_only = false` and the legs return to
  standard RFC 5245 candidate handling.
- Calls failing this way are visible in CDRs: answered calls that end with
  zero media on a leg are flagged with error code `proxy.leg_media_incomplete`
  (call-record `metadata.error_code` / trace event `media_issue`).
- Related ICE option: `prefer_srflx_over_natted_host` (server side — controls
  check ordering when the PBX itself is the controlling agent).

**Diagnostics** (rustrtc ≥ 0.3.133): debug logs `ClientHello decoded`
(offered cipher suites), `Buffering out-of-order handshake message` /
`Handshake message reassembled`, and the
`Received DTLS packet but no receiver registered — dropped` counter localize
the failure to browser-side / reception race / network drop.

## Recording Policy

> **[recording] and [sipflow] are mutually exclusive for RTP capture.**
> The default configuration uses `[sipflow]` for both SIP signalling and RTP
> audio capture. Only configure `[recording]` when you specifically need the
> legacy live WAV recorder. See [08-sipflow.md](08-sipflow.md) for details.

Control when calls are recorded. Can be set at top-level `[recording]` or per-proxy `[proxy.recording]` (proxy-level overrides top-level).

`[recording]` controls the live WAV recorder. When enabled, the recorder always writes a local WAV first. Set `type = "http"` or `type = "s3"` only to upload that local WAV after the call completes.

Recording configuration has priority over SipFlow RTP recording. If a top-level `[recording]` or per-proxy `[proxy.recording]` section is configured, it owns the recording decision:

- `enabled = true`: use the live WAV recorder and optional `[recording]` upload.
- `enabled = false`: do not record RTP media.
- SipFlow SIP message capture still works when `[sipflow]` is enabled.
- SipFlow RTP capture and `[sipflow.upload]` recording export are disabled for that call.

Only omit the recording section entirely when you want SipFlow to capture RTP audio and/or `[sipflow.upload]` to act as the recording source.

```toml
# Top-level recording config (applies to all proxies unless overridden)
[recording]
enabled = false

# Recording upload mode: "local" (default), "http", or "s3".
type = "local"

# Record these directions
directions = ["inbound", "outbound", "internal"]

# Auto-start recording on answer
auto_start = true

# Storage path for raw audio files
path = "./recordings"

# Optional local filename template. Supported tokens:
# {session_id}, {caller}, {callee}, {direction}, {timestamp}
filename_pattern = "{session_id}"

# Fine-grained filters
caller_allow = ["1001", "1002"]
caller_deny = ["anonymous"]
callee_allow = []
callee_deny = ["911"]

# Recording quality
samplerate = 16000      # Resampled 16-bit PCM WAV output rate (Hz); optional
ptime = 20              # File-recorder flush interval (ms); default 500
stereo_swap = false     # true: callee left, caller right (stereo only)

# Hybrid mode: force legacy WAV recorder even when [sipflow] is active
# When true, SipFlow captures signalling only; [recording] handles media
# force_file = true

# SIP signalling JSONL sidecar (written next to the WAV file). Default true.
# Without a [sipflow] backend, every recorded call also writes a lightweight
# {session_id}.jsonl (same schema as SipFlow export_jsonl) capturing the SIP
# messages of the call. It is uploaded together with the WAV when
# type = "http"|"s3", and powers the console "SIP flow" view/download.
# Set signaling = false to record WAV only, with no JSONL sidecar or upload.
# signaling = true

# Or configure per-proxy
[proxy.recording]
enabled = true
directions = ["inbound"]
auto_start = true
```

File recording settings also apply to on-demand recording. Per-call overrides take precedence over the server recording policy. An explicit `samplerate` (8000–192000 Hz) selects resampled 16-bit PCM WAV; omitting it preserves the codec-based WAV output. `ptime` must be positive and controls file flushing, not RTP packetization. These settings do not change SipFlow packet capture.

### HTTP Recording Upload
```toml
[recording]
enabled = true
auto_start = true
type = "http"
path = "./recordings"
url = "https://archive.example.com/recording"
# headers = { "Authorization" = "Bearer token" }
```

`type = "http"` is a generic, configurable multipart uploader. The defaults
(`POST`, multipart field `recording`, file name = local file name, MIME
`audio/wav`, plus `call_id` / `track_id` form fields) preserve the historical
wire format. Any third-party upload API can be described declaratively:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | String | required | Endpoint. May contain `{key}` (replaced with the file name, raw concatenation) |
| `method` | Option\<String\> | `POST` | HTTP method |
| `headers` | Option\<Map\> | None | Extra headers (values support placeholders) |
| `file_field` | Option\<String\> | `recording` | Multipart field carrying the binary payload |
| `body_field` | Option\<String\> | None | Send the payload as this text field instead of a binary file part (mutually exclusive with `file_field`) |
| `file_name` | Option\<String\> | local file name | Multipart file name template |
| `content_type` | Option\<String\> | `audio/wav` | MIME type of the binary payload |
| `fields` | Option\<Map\> | `{call_id, track_id}` | Extra text form fields; values support `{call_id}` / `{track_id}` / `{filename}` / `{key}` |
| `response_url_path` | Option\<String\> | None | Dot-path into a JSON response holding the uploaded URL (e.g. `data.url`). When unset, an `http`-looking body is used, otherwise the request URL |
| `response_success` | Option\<Table\> | None | Business success rule `{ path = "code", equals = 0 }`; when unset any 2xx succeeds |
| `connect_timeout_ms` | Option\<u64\> | 3000 | TCP connect timeout |
| `request_timeout_ms` | Option\<u64\> | 10000 | Total request timeout |

Example matching a key-based resource API:

```toml
[recording]
enabled = true
type = "http"
url = "https://upload.example.com/resources/{key}"
file_field = "filecontent"
file_name = "{key}"
content_type = "application/octet-stream"
fields = { call_id = "{call_id}", track_id = "{track_id}" }
response_url_path = "data.url"
response_success = { path = "code", equals = 0 }
connect_timeout_ms = 3000
request_timeout_ms = 10000
```

Response handling: a 2xx response is success unless `response_success` is set.
The stored URL is extracted from `response_url_path` when configured, else a
response body starting with `http`, else the request URL (with `{key}`
substituted). Non-2xx responses and failed `response_success` checks write the
usual `.upload_failed.*` marker and are retried by the retry worker.

### S3 Recording Upload
```toml
[recording]
enabled = true
auto_start = true
type = "s3"
path = "./recordings"
vendor = "minio" # aws, gcp, azure, aliyun, tencent, minio, digitalocean
bucket = "recordings"
region = "us-east-1"
access_key = "MINIO_ACCESS_KEY"
secret_key = "MINIO_SECRET_KEY"
endpoint = "http://minio:9000"
root = "recordings"
```

For Aliyun OSS, set `vendor = "aliyun"` and provide the complete virtual-hosted
endpoint, including the bucket name. Leave `bucket` and `region` unset;
the client uses the endpoint unchanged:

```toml
vendor = "aliyun"
endpoint = "https://my-bucket.oss-cn-beijing.aliyuncs.com"
```

This applies to both `[recording]` and `[sipflow.upload]`. AccessKey credentials
are still required. The `root` setting is an object-key prefix inside the
bucket. No bucket or region is inferred, and regional endpoints are not
rewritten. Other vendors retain their existing addressing behavior.

When `[recording] type = "http"` or `type = "s3"` is used, the CDR may be written before the media upload finishes. The database `recording_url` is updated after the upload succeeds. The local CDR JSON keeps the local recorder path in `recordingUrl` and the recorder metadata in `recorder[]`.

### On-Demand Segmented Recording

Every media leg carries a recording capture tap, so any call can start/stop
recording **mid-call** — even when `[recording] enabled = false` and no
recording policy matched the call. This powers stage-based recording such as
"record only the IVR stage" or "record once an agent answers" without arming
whole-call recording.

Triggers:

- IVR nodes `record_start` / `record_stop` (see the IVR step protocol docs)
- RWI `record.start` / `record.stop` (also inline on `call.originate`)
- Console / CTI HTTP APIs (`POST /calls/active/{session_id}/commands`,
  `POST /cc/calls/{call_id}/record`)
- In-dialog SIP INFO (`application/vnd.rustpbx+json`,
  `{"action": "record.start", "params": {...}}`)

Auto-generated segment file names follow `{path}/{root_session_id}_{seq}_{label}.wav`:

- `seq` — 1-based per-call recording counter; bumped automatically when a file
  with the candidate name already exists (segments started on different legs
  of the same logical call — e.g. around transfers — share the root session
  id).
- `label` — resolved at start time: explicit `label` parameter →
  `resolved_agent_id` / `agent_id` session extension (set when a CC agent
  answers) → `ivr` session extension (set while the call is inside an IVR) →
  `segment_type`.

Example: an inbound call records one slice inside the main IVR and another
once the agent answers → `abc123_1_main-ivr.wav`, `abc123_2_1001.wav`.

Each completed segment is listed in the CDR
(`metadata.recording_segments`, one entry with `seq` / `label` /
`segment_type` / `segment_id` / start/end times per slice). On upload, one
`recording_metadata_available` RWI/webhook event is emitted **per segment**
(`filename` / `download_url` / `file_size` describe that segment; `extra`
carries `seq` / `label` / `segment_type` next to the call-level metadata).
each segment notifies independently — there is no call-level summary event; `record_end` has been removed. Every event carries a first-class `source` field (`ivr` / `agent` / `consult` / `ringing` / `voicemail` / `full` / `external`). Upload works the same as
whole-call recordings (`type = "local"|"http"|"s3"`, `.upload_failed.*`
markers + retry worker).

Contact-center agent segments: cc.toml accepts

```toml
[recording]
record_on_agent_connect = true
```

With this on, the CC addon starts a `segment_type = "agent"` segment
automatically when an agent answers (labeled with the agent id) and closes it
when the agent leg leaves while the customer call continues (CSAT survey,
return-to-IVR). Sessions already recording (policy full-call recording or a
previous segment) are left untouched.

### SIP Signaling JSONL Sidecar

When `[recording] enabled = true` is used **without** a `[sipflow]` backend (or with `force_file = true`), each recorded call additionally writes a SIP signalling sidecar next to the WAV file:

- Path: `{path}/{session_id}.jsonl` (or `{path}/{session_id}_{call_id}.jsonl` for extra legs).
- Format: one JSON object per line, **byte-identical schema to SipFlow `export_jsonl`**:

```json
{"timestamp":1753000000123456,"seq":0,"leg":null,"msg_type":"Sip","src_addr":"192.168.1.10:5060","dst_addr":"","payload":"INVITE sip:1001@pbx SIP/2.0\r\n..."}
```

| Field | Type | Description |
|-------|------|-------------|
| `timestamp` | u64 | Unix microseconds |
| `seq` | u64 | Capture sequence number |
| `leg` | null / i32 | Media leg (null for SIP messages) |
| `msg_type` | string | Always `"Sip"` |
| `src_addr` / `dst_addr` | string | Transport address (one side filled per direction) |
| `payload` | string | Full SIP message text |

- Upload: with `type = "http"` or `type = "s3"` the JSONL is uploaded together with the WAV (same storage path); local files are deleted only after a successful upload. On failure a sibling `.upload_failed.{filename}` marker is written.
- Console: the CDR detail page's "SIP flow" view/download falls back to this file when no SipFlow backend is configured.
- Disable: set `signaling = false` in `[recording]` to skip the sidecar (and its upload) entirely — only the WAV is recorded/uploaded.

## CDR (Call Detail Records)

### Database CDR (always on)

Every call is automatically persisted to the `rustpbx_call_records` table in your configured database. This is the primary CDR mechanism and requires no extra configuration — the Web Console "Call Records" page reads from this table.

As long as `database_url` is set (which is always required), call records will be written.

### Optional CDR sinks (`[callrecord]`)

The `[callrecord]` section is **optional**. It adds a secondary raw-CDR sink on top of the always-on database persistence. Omit this section entirely if you only need database CDRs (the common case).

`max_concurrent` controls how many post-call CDR save/upload/hook tasks may run at once. The default is `64`; values below `1` are clamped to `1`.

Common pipeline options (independent of the storage type):

| Key | Default | Description |
|---|---|---|
| `channel_capacity` | 2048 | Bounded queue length between call producers and the CDR manager. Producers drop (and count `cdr_records_dropped_total`) when full. |
| `batch_size` | 64 | Max records per manager batch. |
| `track_queue_latency` | false | Record the `cdr_queue_latency_seconds` histogram (queueing wait only — save/push time excluded). |

The pipeline exports Prometheus metrics (`cdr_records_enqueued_total`,
`cdr_records_pushed_total`, `cdr_records_push_failed_total`,
`cdr_records_dropped_total`, `cdr_queue_size`, `cdr_queue_current`,
`cdr_queue_latency_seconds`) — see [observability.md](../observability.md#call-record-cdr-pipeline).

### Database
Writes CDR JSON to a separate database table (default: `call_records`).

```toml
[callrecord]
type = "database"
# database_url = "sqlite://cdr.sqlite3"     # Optional: separate database
# table_name = "call_records"               # Optional: custom table name
max_concurrent = 64
```

### Optional storage types

All examples below are **optional** add-ons. The database CDR (above) is always active regardless.

### Local Filesystem
```toml
[callrecord]
type = "local"
root = "./cdr_archive"
max_concurrent = 64
```

### S3 Compatible Object Storage
Uploads CDR JSON to AWS S3, MinIO, DigitalOcean Spaces, etc.

```toml
[callrecord]
type = "s3"
vendor = "minio" # aws, gcp, azure, digitalocean, etc.
bucket = "my-recordings"
region = "us-east-1"
# S3 Credentials
access_key = "MINIO_ACCESS_KEY"
secret_key = "MINIO_SECRET_KEY"
endpoint = "http://minio:9000" # needed for non-AWS
root = "/daily-records"
max_concurrent = 64

# Deprecated and ignored. Recording media upload is configured by [recording].
with_media = true
keep_media_copy = false
```

### HTTP Webhook
Send CDR JSON to an endpoint.
```toml
[callrecord]
type = "http"
url = "http://my-crm/cdr-hook"
max_concurrent = 64

# Deprecated and ignored. Recording media upload is configured by [recording].
with_media = true
keep_media_copy = false
```

HTTP CDR delivery uses `multipart/form-data` with the CDR JSON as a text field. The default field name is `calllog.json`; override it with `body_field`, or send the JSON as a binary part with `file_field` plus `file_name`/`content_type`. The same generic HTTP scheme options as `[recording] type = "http"` are accepted (`method`, `headers`, `fields`, `response_url_path`, `response_success`, `connect_timeout_ms`, `request_timeout_ms`). Recording media is delivered separately by `[recording] type = "http"`.

```toml
[callrecord]
type = "http"
url = "http://my-crm/cdr-hook"
body_field = "calllog.json"   # default
# method = "POST"
# headers = { Authorization = "Bearer token" }
# response_success = { path = "code", equals = 0 }
```

