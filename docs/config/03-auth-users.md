# Authentication & Users

RustPBX supports multiple backend types for user authentication and retrieval. You can chain multiple backends in `proxy.user_backends`.

## 1. Memory Backend (Static)
Best for small, static deployments or testing.

```toml
[[proxy.user_backends]]
type = "memory"

[[proxy.user_backends.users]]
username = "1001"
password = "secret-password"
realm = "example.com"  # Optional
display_name = "Alice"
enabled = true
# Allow calls from this user without Registration (IP-based auth usually handled elsewhere)
allow_guest_calls = false 
```

## 2. Database Backend
Loads users from the SQL database (`database_url`).

```toml
[[proxy.user_backends]]
type = "database"
# Optional overrides for table schema
table_name = "users"
id_column = "id"
username_column = "username"
password_column = "password"
realm_column = "realm"
enabled_column = "is_active"
```

## 3. HTTP Backend (Remote)
Offloads auth to an external web service.

```toml
[[proxy.user_backends]]
type = "http"
url = "http://auth-service/verify"
method = "POST"
# headers = { "X-Api-Key" = "secret" }
```
*Note: The HTTP service must return a JSON structure matching the accepted User model.*

## 4. Plain Text Backend
Loads users from a simple text file.

```toml
[[proxy.user_backends]]
type = "plain"
path = "./users.txt"
```

## 5. Extension Backend
Used for short-lived, dynamic extensions (often internal).

```toml
[[proxy.user_backends]]
type = "extension"
ttl = 3600 # Cache time in seconds
```

---

## Locator (Registrar Storage)
Configures where "Where is User X?" data is stored.

### Memory (Default)
Fast, but lost on restart.
```toml
[proxy.locator]
type = "memory"
```

### Database
Persistent registrations.
```toml
[proxy.locator]
type = "database"
url = "sqlite://rustpbx.sqlite3" # Can share main DB
```

### HTTP (Remote)
Query external registry.
```toml
[proxy.locator]
type = "http"
url = "http://registry-service/lookup"
```
