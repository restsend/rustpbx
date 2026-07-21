# RustPBX Configuration Overview

## Configuration Sources
RustPBX loads configuration in the following order of precedence:
1. **Command Line arguments**: e.g. `--conf my_config.toml`
2. **Main Config File**: `rustpbx.toml` (or custom path)
3. **Partial Config Files**: Loaded via glob patterns defined in main config (e.g. `trunks_files`, `routes_files`).
4. **Generated Configs**: Automatically loaded from `generated_dir` if present (managed by UI/API).
5. **Database Configs**: When `generated_db = true`, generated configs (trunks, routes, queues, IVR, ACL, CC, SBC) are stored in the `config_entries` database table instead of files.

## Storage Modes

### File System Mode (default)
By default, the system assumes a `config` folder exists next to the binary:

```toml
[proxy]
# Root for generated configs (default: ./config)
generated_dir = "./config"
# Explicit overrides for file patterns
routes_files = ["config/routes/*.toml"]
trunks_files = ["config/trunks/*.toml"]
acl_files = ["config/acl/*.toml"]
```

### Database Mode
Set `generated_db = true` to store all generated configs in the application database:

```toml
[proxy]
generated_db = true
```

In this mode, the system reads from and writes to the `config_entries` table instead of the filesystem. This includes:
- SIP trunks, routes, queues, and ACL rules
- IVR project definitions (when published)
- CC ACD config, skill groups, and agents
- SBC JSON-RPC config

File-based `routes_files`, `trunks_files`, etc. are ignored in DB mode — all config is managed through the database. The `generated_dir` path is also unused.

## Reload Behavior
Changes to `rustpbx.toml` usually require a restart. However, **Trunks**, **Queues**, **Routes**, and **ACLs** can be reloaded at runtime without dropping active calls via the Admin Console or API. In DB mode, the same runtime reload applies — configs are loaded from the database.

## Addon System
Addons (like Wholesale, Queue, Transcript) are enabled in the `[proxy]` section.

```toml
[proxy]
addons = ["wholesale", "queue"]
```

## Configuration Reference

| # | File | Topics |
|---|---|---|
| 01 | [Platform & Networking](01-platform.md) | HTTP/HTTPS, ports, TLS, external IP |
| 02 | [Proxy Core](02-proxy-core.md) | SIP stack, realms, locators, dialplan |
| 03 | [Auth & Users](03-auth-users.md) | User accounts, passwords, RBAC |
| 04 | [Routing](04-routing.md) | Route rules, pattern matching |
| 05 | [Trunks & Queues](05-trunks-queues.md) | SIP trunks, call queues |
| 06 | [Media & Recording](06-media-recording.md) | RTP proxy, codecs, call recording, CDR |
| 07 | [Addons, Console & Storage](07-addons-admin-storage.md) | Web console, AMI, blob storage, cluster, licenses, RWI |
| 08 | [SipFlow](08-sipflow.md) | SIP flow capture, RTP recording, storage backends, WAV export, REST API |
