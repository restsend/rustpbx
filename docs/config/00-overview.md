# RustPBX Configuration Overview

## Configuration Sources
RustPBX loads configuration in the following order of precedence:
1. **Command Line arguments**: e.g. `--conf my_config.toml`
2. **Main Config File**: `rustpbx.toml` (or custom path)
3. **Partial Config Files**: Loaded via glob patterns defined in main config (e.g. `trunks_files`, `routes_files`).
4. **Generated Configs**: Automatically loaded from `generated_dir` if present (managed by UI/API).

## Directory Structure
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

## Reload Behavior
Changes to `rustpbx.toml` usually require a restart. However, **Trunks**, **Queues**, **Routes**, and **ACLs** can be reloaded at runtime without dropping active calls via the Admin Console or API.

## Addon System
Addons (like Wholesale, Queue, Transcript) are enabled in the `[proxy]` section but configured in their own namespaces.

```toml
[proxy]
addons = ["wholesale", "queue"]

# Addon-specific configuration map
[addons.wholesale]
license = "key-123"
```
