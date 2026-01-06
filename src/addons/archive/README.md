# Archive Addon

This addon provides functionality to archive old call records to compressed CSV files and remove them from the database.

## Configuration

Add the following section to your `config.toml` (or `rustpbx.toml`):

```toml
[archive]
enabled = true
archive_time = "03:00"       # Time to run the archive job (HH:MM)
timezone = "Asia/Shanghai"   # Timezone for the schedule
retention_days = 30          # Keep data for 30 days
```

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
