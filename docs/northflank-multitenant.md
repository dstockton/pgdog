# PgDog on Northflank: Multi-Tenant Directus

Front a single Northflank Postgres addon with PgDog. Each Directus deployment gets its own database on the shared addon, enforced by a per-tenant size quota. Directus deployments only change DB host/port/user/password.

## Prereqs

- Northflank project exists.
- Postgres addon provisioned (shared — no overage billing).
- Capture addon connection details: host, port, superuser name, superuser password.

## 1. Create one database per tenant on the addon

```bash
# One-off from any host that can reach the addon.
export ADDON_HOST=<addon-host>
export ADDON_PORT=<addon-port>
export ADDON_SUPER=<superuser>
export ADDON_SUPER_PW=<superuser-password>

for T in tenant_a tenant_b tenant_c; do
  PGPASSWORD="$ADDON_SUPER_PW" psql -h "$ADDON_HOST" -p "$ADDON_PORT" \
    -U "$ADDON_SUPER" -d postgres -v ON_ERROR_STOP=1 <<SQL
CREATE ROLE ${T}_app LOGIN PASSWORD '<set-per-tenant>';
CREATE DATABASE ${T} OWNER ${T}_app;
REVOKE ALL ON DATABASE ${T} FROM PUBLIC;
SQL
done
```

Record each `<set-per-tenant>` password — you will paste them into `users.toml`.

## 2. Size the quota

Pick one number applied to every tenant. No global default exists; the `max_db_size` field is per-database.

```
max_db_size_bytes = floor( addon_storage_gb * 1024^3 * 0.80 / num_tenants )
```

Shell helper:

```bash
ADDON_GB=10        # total addon storage
TENANTS=8          # expected tenant count
python3 -c "print(int($ADDON_GB * 1024**3 * 0.80 / $TENANTS))"
# 1073741824   (example: 1 GiB)
```

Use that integer for every `max_db_size` below.

## 3. `pgdog.toml`

```toml
[general]
host = "0.0.0.0"
port = 6432
workers = 2
default_pool_size = 10
# Quota monitor cadence. 60s default; lower only if tenants push writes hard.
quota_poll_interval = 60000
openmetrics_port = 9930

# Replicate this block per tenant. Only `name` / `database_name` change.
[[databases]]
name            = "tenant_a"
host            = "<addon-host>"
port            = <addon-port>
role            = "primary"
database_name   = "tenant_a"
user            = "tenant_a_app"
password        = "<tenant_a password>"
max_db_size     = 1073741824   # 1 GiB — same for every tenant

[[databases]]
name            = "tenant_b"
host            = "<addon-host>"
port            = <addon-port>
role            = "primary"
database_name   = "tenant_b"
user            = "tenant_b_app"
password        = "<tenant_b password>"
max_db_size     = 1073741824
```

## 4. `users.toml`

One client-facing user per tenant. Directus uses these credentials, not the addon superuser.

```toml
[[users]]
name     = "tenant_a"
database = "tenant_a"
password = "<pgdog-facing password>"

[[users]]
name     = "tenant_b"
database = "tenant_b"
password = "<pgdog-facing password>"

[admin]
name     = "admin"
password = "<admin password>"
```

## 5. Deploy PgDog on Northflank

Create a **Combined Service** (build + deploy) or deploy a prebuilt image.

```bash
# Build & push (from this repo root, pushing to any registry Northflank can read).
docker build -t <registry>/pgdog:quota .
docker push <registry>/pgdog:quota
```

Northflank service settings:

- Image: `<registry>/pgdog:quota`
- Command: `/usr/local/bin/pgdog` (default `CMD`)
- Working dir: `/pgdog`
- Mount config files at `/pgdog/pgdog.toml` and `/pgdog/users.toml` (Northflank **Config Files** feature — paste the TOML, mount path = file path).
- Ports:
  - `6432` TCP — **Internal** only (Directus talks to it over the project network).
  - `9930` TCP — Internal, for metrics scraping.
- Env: `RUST_LOG=info`
- Resources: start at 1 vCPU / 512 MiB, scale with tenant count.
- Replicas: 1 is fine; scale horizontally only if CPU-bound. Quota state is per-replica; each replica polls independently.

## 6. Point each Directus deployment at PgDog

Change only these env vars on the Directus service:

```
DB_CLIENT=pg
DB_HOST=<pgdog-service>.<project>.svc.cluster.local   # Northflank internal DNS
DB_PORT=6432
DB_DATABASE=tenant_a          # matches `name` in pgdog.toml / users.toml
DB_USER=tenant_a
DB_PASSWORD=<pgdog-facing password>
```

Leave everything else (`KEY`, `SECRET`, storage, etc.) untouched per deployment.

## 7. Smoke test

```bash
# From inside the Northflank project (e.g. a debug pod or Directus shell).
PGPASSWORD=<pgdog-facing password> psql \
  -h <pgdog-service> -p 6432 -U tenant_a -d tenant_a -c 'SELECT 1;'

# Admin DB: quota status across all tenants.
PGPASSWORD=<admin password> psql \
  -h <pgdog-service> -p 6432 -U admin -d admin -c 'SHOW QUOTAS;'
```

Expected `SHOW QUOTAS` columns: `database | current_size | max_size | over_limit`.
`current_size = -1` means the monitor has not polled yet (fail-closed: writes blocked until first poll completes — happens within `quota_poll_interval`).

## 8. Verify enforcement

```bash
# Write that should succeed.
psql -h <pgdog-service> -p 6432 -U tenant_a -d tenant_a \
  -c "CREATE TABLE IF NOT EXISTS t(id int); INSERT INTO t VALUES (1);"

# After a tenant crosses max_db_size, writes return:
#   ERROR: database quota exceeded: database '<tenant>' has exceeded its size quota (...)
# Reads are always allowed.
```

Enforcement has two tiers:

- **Proxy-level** (via PgDog AST): INSERT / UPDATE / CREATE / COPY FROM are blocked; DELETE / TRUNCATE / VACUUM / DROP TABLE|INDEX|SCHEMA are exempt. DROP DATABASE is not exempt.
- **Server-level** (via `ALTER DATABASE <db> SET default_transaction_read_only = on`): once the monitor detects over-quota, *all* writes on new connections are blocked by Postgres itself, including the shrink operations exempted at the proxy layer. This is the backstop for queries that bypass AST classification (extended protocol / prepared statements).

Practical consequence: once the monitor flips a tenant to read-only, that tenant cannot DELETE or TRUNCATE their way back under the limit on a new connection. Recovery requires one of:

1. `SET QUOTA <db> <larger-bytes>` on the admin DB. The monitor will see size ≤ limit on its next poll and toggle `default_transaction_read_only = off`. Pool rotation (automatic on `server_lifetime` expiry, or forced via `RECONNECT`) causes new client connections to pick up the setting.
2. Operator runs `ALTER DATABASE <db> RESET default_transaction_read_only` directly against the backend and then `VACUUM FULL` / `DROP TABLE` to actually free pages. Note that `pg_database_size()` reflects filesystem high-water mark — it only shrinks after operations that release pages (`DROP TABLE`, `VACUUM FULL`, or database recreate), not after `TRUNCATE` or `DELETE + VACUUM`.

## 9. Operate

Persistent quota change: edit `max_db_size` on the `[[databases]]` entry in `pgdog.toml`, redeploy the config file, and issue `RELOAD` on the admin DB. The monitor re-reads config each poll cycle, so the new limit applies within one `quota_poll_interval`.

Runtime override (no config edit, lost on restart):

```bash
# Temporarily bump tenant_a to 2 GiB.
psql -h <pgdog-service> -p 6432 -U admin -d admin \
  -c "SET QUOTA tenant_a 2147483648;"

# Revert to the configured max_db_size.
psql -h <pgdog-service> -p 6432 -U admin -d admin \
  -c "RESET QUOTA tenant_a;"

# Lock writes immediately (any size > 0 is over limit).
psql -h <pgdog-service> -p 6432 -U admin -d admin \
  -c "SET QUOTA tenant_a 0;"

# Apply config-file changes without restarting PgDog.
psql -h <pgdog-service> -p 6432 -U admin -d admin -c "RELOAD;"

# Check current state.
psql -h <pgdog-service> -p 6432 -U admin -d admin -c "SHOW QUOTAS;"

# Metrics (scrape into Northflank's Prometheus or any OTEL collector).
curl http://<pgdog-service>:9930/metrics | grep pgdog_db_
#   pgdog_db_size_bytes{database="tenant_a"} ...
#   pgdog_db_size_limit_bytes{database="tenant_a"} ...
#   pgdog_db_over_limit{database="tenant_a"} 0|1
```

`SET QUOTA` requires that the target database already has a non-zero `max_db_size` in `pgdog.toml` — databases without a configured quota are not polled by the monitor, so an override on them has no effect. The command rejects those up front. Override changes take effect on the next monitor poll cycle (up to `quota_poll_interval` ms); they do not retroactively unblock server-level read-only flips until the monitor next reconciles.

## Adding a tenant

1. Create DB + role on the addon (step 1).
2. Append `[[databases]]` block to `pgdog.toml` with the same `max_db_size`.
3. Append `[[users]]` to `users.toml`.
4. Redeploy the PgDog service (config is loaded at start; a rolling restart re-reads both files).
5. Deploy a new Directus service using the creds from step 3.

## Removing a tenant

1. Stop the Directus service.
2. Remove the matching `[[databases]]` and `[[users]]` entries; redeploy PgDog.
3. `DROP DATABASE <tenant>; DROP ROLE <tenant>_app;` on the addon.

## Notes / gotchas

- Quota enforcement at the proxy needs the simple query protocol AST; prepared-statement writes rely on the server-level `ALTER DATABASE SET default_transaction_read_only` fallback — both are wired up, no action needed.
- The monitor re-reads config every poll cycle, so `max_db_size` changes via config redeploy apply without a restart once the file is updated on disk. A rolling restart still makes the change take effect faster.
- `pg_database_size()` includes indexes and bloat. It reflects filesystem pages Postgres has claimed, not live row data — `DELETE` and `TRUNCATE` leave the high-water mark in place until `VACUUM FULL` or `DROP TABLE` releases pages. When a tenant sits near its quota, prefer shrinking via `DROP TABLE` / `VACUUM FULL` *before* the monitor trips; once the server-side read-only flag is set, even shrink operations are blocked at the Postgres layer until either `SET QUOTA` bumps the limit or an operator manually resets `default_transaction_read_only` on the backend.
- Keep `default_pool_size` × tenants well below the addon's `max_connections`. Leave headroom for admin sessions.
