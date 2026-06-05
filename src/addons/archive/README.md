# Archive Addon

This addon provides functionality to archive old call records to compressed CSV files and remove them from the database.

## Configuration

Enable the addon in your main `config.toml` (or `rustpbx.toml`):

```toml
[proxy]
addons = ["archive"]
```

Then add the following settings to `archive.toml` in the same directory as your main config file:

```toml
enabled = true
archive_time = "03:00"       # Time to run the archive job (HH:MM)
timezone = "Asia/Shanghai"   # Timezone for the schedule
retention_days = 30          # Keep data for 30 days
archive_after_days = 0       # 0 archives yesterday's records
# archive_dir = "/var/lib/rustpbx/archive"
```

For compatibility, the addon also accepts the same settings under `[archive]` in the main config
when `archive.toml` is not present. The console UI saves archive settings to `archive.toml`.

## Features

- **Scheduled Archiving**: Automatically runs daily at the configured time.
- **Data Retention**: Exports call records older than `retention_days` to CSV.
- **Compression**: Compresses CSV files using Gzip (`.gz`).
- **Storage**: Saves archives to `archive/` directory in the workspace root.
- **Cleanup**: Removes archived records from the database.
- **Management UI**: View and delete archive files via the Admin Console.

## API

- `GET /api/archive/list`: List all archive files.
- `POST /api/archive/delete`: Delete an archive file. Payload: `{"filename": "..."}`.
