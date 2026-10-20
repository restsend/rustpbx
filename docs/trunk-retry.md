# Trunk retry conditions

Wholesale routing builds its candidate sequence once, before dialing. The
sequential dialer then uses the failed target's retry policy to decide whether
to advance. It does not rerun routing after each failure.

Configure a trunk's failure codes and ring time under **SIP Trunk →
General → Trunk failover**, or on the wholesale supplier trunk configuration
page. A trunk without a policy does not fail over.

## Candidate order

Wholesale retains every matching route item with a positive weight, across all
priorities; weight 0 excludes an item. With LCR enabled, buy rate orders
candidates across all priorities. Otherwise it orders by ascending priority,
then weighted rendezvous hashing of the SIP Call-ID and trunk ID within each
priority, so the first target follows the configured weights.

A list containing one target uses the existing
single-target path. Multiple targets produce a sequential dialplan. Each target
carries its own number rewrites, credentials, media settings, billing context
and its trunk's retry policy (an empty policy when the trunk has none). Tenant
admission happens once.

## Retry policy and ring time

- `status_codes` (retry policy): final SIP failure codes, from 300 through 699.
  The next target is tried only when the failed target's code is listed; an
  empty list disables failover from the trunk.
- `max_ring_time` (trunk setting, seconds): how long the trunk may ring. As the
  only target, it limits the whole call, as before. In a sequence of several
  targets, it limits that trunk's attempt: the INVITE is cancelled and the
  attempt fails with 408, so listing 408 tries the next target.

The call's own ring timer (the dialplan's `max_ring_time`, global or per route)
bounds the whole sequence. It runs around all attempts; when it fires, the
current target is cancelled and the call is rejected without trying further
targets.

Locally generated failures keep their mapped SIP code and are matched like any
other status; for example, a setup error is matched by listing 500. A target
with no policy object at all (queue agents, registered contacts, HTTP router
targets) keeps the legacy behaviour and advances after any failure;
routed trunk targets such as wholesale always carry a policy. Exhausted
targets, caller cancellation and an answered call also stop retries.

For example, a trunk with codes `[408, 503]` and a 20-second ring time tries the
next target after a 503, or after ringing 20 seconds. A 486 stops failover, and
so does the call's ring timer.

Parallel dialing does not use these retry checks.

## Storage

Database trunks store the configuration in the existing metadata JSON; no trunk
schema change is needed:

```json
{
  "sbc": {
    "max_ring_time": 20,
    "retry_policy": { "status_codes": [408, 503] }
  }
}
```

The console trunk page writes both into the metadata JSON. The wholesale trunk
API accepts `retry_policy` and `max_ring_time`, preserving unrelated metadata;
omit a field to keep its current value, send `{"retry_policy":{"status_codes":[]}}`
to disable retry, or `{"max_ring_time":0}` to clear the ring time. File-managed
trunks use `max_ring_time` and a nested `retry_policy` object.

Routing returns `RouteResult::Forward(Vec<RouteTarget>)` for both single and
multiple destinations. Each `RouteTarget` holds the routed `InviteOption`
(callee, rewritten caller, destination, credential and headers), its optional
`DialplanHints`, and an optional `RetryPolicy`. A target without a policy never
advances to the next one. An empty list has no destination and does not fall
back to direct dialing.

One target dials exactly as a plain forward did before: its caller and hints
apply to the whole dialplan, and an unchanged local target still resolves to
its registered contacts. Several targets become
`DialStrategy::Sequential(Vec<DialAttempt>)`, and each attempt applies its own
caller and hints before that trunk is dialed. Recording and the media path are
fixed for the call by the first target, since neither can change mid-call.
Single-leg routing (originate, transfer, RWI) uses the first target.

After a failure, the dialer tries the next target only when the failed target's
policy lists the final status. The HTTP router also accepts
per-target `retry_policy` objects; see the [HTTP routing API](api_integration_guide.md#11-http-router-dynamic-call-routing).
HTTP targets without a policy retain their legacy advance-on-failure behaviour.

## Attempt results

The call record describes the final attempt only. When a sequential plan has
more than one target, each target that fails is also traced in the call
record's `metadata.trace` as a `dial.attempt_failed` event. Its `detail` holds
the attempt index, target URI, SIP status and reason, and the attempt's trunk
(`trunk_id`, `trunk`) taken from its `OutboundTrunkContext`. Call record hooks
can use these events to account for every trunk that was tried, for example to
feed per-carrier circuit breakers; the router is never called again during
dialing.
