# Call Frequency Limit Plan

## Goals
- Provide per-caller/per-callee throttling with vendor-specific windows (e.g., 3 calls / 24h).
- Centralize static policy definitions in the database so ops can audit and deploy with migrations.
- Enforce dynamic counters via shared PostgreSQL tables for rolling windows and concurrency guards.
- Surface configuration, debugging, and override controls in the console.

## Data Model
- **Policy definitions**
  - Extend `rustpbx_routes.metadata` and `rustpbx_sip_trunks.metadata` with a `policy` object that captures both origin (trunk) country and destination country constraints:
    ```json
    {
      "called_prefix": "4109",
      "trunk_country": "CN",
      "allowed_destination_countries": ["CN"],
      "time_window": {"start": "08:00", "end": "22:00", "timezone": "Asia/Shanghai"},
      "deny_regions": ["bjing", "shai"],
      "allow_landline": true,
      "frequency_limit": {"count": 3, "window_hours": 24},
      "daily_limit": {"count": 3},
      "concurrency": {"max_total": 3, "max_per_account": {"1": 3}},
      "tags": ["qx1", "sichuan"]
    }
    ```
  - Store JSON schema version to allow future migrations.
- **Runtime counters**
  - Dedicated tables keep all frequency state so every node shares data via PostgreSQL:
    - `call_frequency_counters(policy_id, scope, scope_value, window_start, window_end, attempt_count, last_attempt_at)`.
    - `call_concurrency_locks(trunk_id, country, active_calls, updated_at)`.
  - `scope` allows `caller`, `callee`, or `pair` rows. Windows are discretized (e.g., per hour/day) to avoid unbounded growth.
  - Use partial indexes on `(policy_id, scope, scope_value, window_end)` for quick lookups.
  - Nightly job purges expired windows once `window_end < now()`.
- **Audit trail**
  - Persist rejection entries to `call_record.metadata.frequency` so console can show reason, timestamp, policy id, and current count.

## Runtime Flow
1. Normalize numbers and run `phonedata.lookup` → attach `{country, province, city, line_type, carrier}`.
2. Fetch applicable policies from route + trunk metadata (cache in memory with TTL) and determine trunk origin country.
3. Policy guard checks:
  - Time window, region bans, line-type permission, called prefix, origin/destination country alignment.
  - Within a DB transaction, `SELECT ... FOR UPDATE` the relevant counter row, roll the window forward if needed, increment `attempt_count`, and persist new totals before routing.
  - If limit reached, reject with `SIP 603` and record reason.
4. On successful call setup, insert/update `call_concurrency_locks` for the trunk (and optional country bucket); delete or decrement the row on hangup/failure.
5. Background job aggregates old windows into analytics tables and deletes expired `call_frequency_counters` rows to keep the dataset bounded.

## Console Requirements
- **Policy Editor (Routing & SIP Trunk pages)**
  - Form to toggle frequency control per policy, set `count`, `window`, `daily limit`, allowed regions, prefixes, concurrency caps, trunk origin country, allowed destination countries, and per-country concurrency overrides.
  - Validation for time ranges and numeric limits.
  - JSON preview/raw editor for advanced fields.
- **Diagnostics View**
  - Table of blocked numbers backed by `call_frequency_counters`: columns `{number, policy, scope, count, window_end, reason}` plus derived `window_remaining`.
  - Actions: "Lift limit" (issue REST call that truncates the specific DB row), "Add exception" (store in allowlist table).
  - Filters by trunk, route, region, date.
- **Real-time Debug Panel**
  - Streaming feed (WebSocket) of policy evaluations with pass/fail, so ops can verify configuration during rollout.
  - Downloadable logs for compliance.
- **Test Tool**
  - Simple form to simulate a call: enter caller/callee/time/origin country → backend runs policy guard in dry-run mode and returns evaluation trace including country checks.

## Monitoring & Alerts
- Emit metrics sourced from DB writes:
  - `policy.limit.hit` with labels `{trunk, route, policy_id, country}` triggered when a counter crosses its threshold.
  - `policy.limit.released` when manual reset deletes or resets a DB counter row.
- Grafana dashboard showing hit rate, current concurrency utilization, and blocked regions.
- Alert when hit rate spikes or concurrency saturates for more than N minutes.

- Confirm PostgreSQL sizing and partitioning strategy since frequency counters now live exclusively in DB; consider monthly partitions if traffic is high.
- Define batching/archival process for `call_frequency_counters` so that historical data can move to cold storage without impacting hot queries.
- Decide retention period for audit records to avoid bloating `call_record` table.

## Development TODOs
1. **Backend Core**
  - [ ] Define `PolicySpec` struct + serde schema shared by routes and trunks, including trunk country + allowed destination info.
  - [ ] Implement `PolicyGuard` that loads specs, evaluates static checks, and delegates to a DB-backed `FrequencyLimiter`.
  - [ ] Create a persistence layer (trait + Postgres implementation + in-memory mock) that increments counters inside transactions and enforces concurrency rows.
  - [ ] Add rejection logging and metrics emission.
2. **Migrations & Data Access**
  - [ ] Add JSON schema versioning to `rustpbx_routes.metadata`/`rustpbx_sip_trunks.metadata` plus optional indexed `trunk_country` column for reporting.
   - [ ] Provide admin SQL seeds for sample carriers (qx1, 112.126.28.95, 103.213.96.43).
  - [ ] Create migrations for `call_frequency_counters`, `call_concurrency_locks`, and helper views (including partitioning or retention policies).
3. **Console UI**
  - [ ] Routing page: new tab for "Frequency & Geo Policies" with form + validation + preview including country targeting widgets.
  - [ ] SIP trunk detail: surface effective policy (inherits from route, overrides at trunk) and highlight the trunk's home country.
   - [ ] Diagnostics page listing blocked numbers with actions (lift limit, extend ban duration, add exception).
   - [ ] Debug/test tool modal to dry-run policies.
4. **Tooling & Ops**
  - [ ] CLI/REST endpoints to list current counters, reset by number/policy/country (DELETE rows), and export CSV straight from the DB views.
   - [ ] Grafana/Prometheus dashboards for `policy.limit.*` metrics.
   - [ ] Runbook documentation for common tasks (unlock number, adjust limit, inspect logs).
5. **Testing**
  - [ ] Unit tests for `PolicySpec` parsing, guard logic, DB limiter (including country-only cases) using transaction rollbacks.
  - [ ] Integration tests covering vendor scenarios (prefixes, region bans, concurrency, cross-country bans) that assert counter rows match expectations.
   - [ ] Console Cypress tests for creating/editing policies and viewing diagnostics.
