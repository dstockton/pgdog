//! Per-database size quota enforcement.
//!
//! Monitors database sizes and blocks write queries when a database
//! exceeds its configured `max_db_size` limit. Shrink operations
//! (DELETE, TRUNCATE, VACUUM, and `DropStmt` variants such as
//! DROP TABLE / INDEX / SCHEMA) remain allowed so tenants can reduce
//! usage. Admin-level statements like DROP DATABASE (`DropdbStmt`)
//! are not exempt — see `classify::is_shrink_operation`.
//!
//! Runtime overrides are exposed via the admin `SET QUOTA <db> <bytes>`
//! and `RESET QUOTA <db>` commands (see `admin::quota_override`). They
//! persist across monitor poll cycles but are lost on restart; change
//! `max_db_size` in `pgdog.toml` + `RELOAD` for persistent updates.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use tokio_postgres::NoTls;
use tracing::{debug, error, info, warn};

use crate::config::config;
use pgdog_config::TlsVerifyMode;

mod classify;
#[cfg(test)]
mod tests;

pub use classify::{is_copy_from, is_data_write, is_shrink_operation};

/// Per-database quota status.
#[derive(Debug, Clone)]
pub struct QuotaStatus {
    pub database: String,
    pub current_size: u64,
    pub max_size: u64,
    pub over_limit: bool,
}

/// Global quota state, updated by the background monitor.
static QUOTA_STATE: Lazy<ArcSwap<HashMap<String, QuotaStatus>>> =
    Lazy::new(|| ArcSwap::from_pointee(HashMap::new()));

/// Admin overrides that persist across monitor poll cycles.
/// Guarded by Mutex to prevent lost updates from concurrent set/clear.
static QUOTA_OVERRIDES: Lazy<parking_lot::Mutex<HashMap<String, u64>>> =
    Lazy::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Read current quota state for a database.
pub fn quota_status(database: &str) -> Option<QuotaStatus> {
    QUOTA_STATE.load().get(database).cloned()
}

/// Check if a database is over its quota limit.
pub fn is_over_quota(database: &str) -> bool {
    QUOTA_STATE
        .load()
        .get(database)
        .map(|s| s.over_limit)
        .unwrap_or(false)
}

/// Get all quota statuses.
pub fn all_quota_statuses() -> Vec<QuotaStatus> {
    QUOTA_STATE.load().values().cloned().collect()
}

/// Override quota limit at runtime (persists across poll cycles).
///
/// The `QUOTA_OVERRIDES` mutex is held across the `QUOTA_STATE` store
/// to serialize with `monitor_loop`, which also acquires the same
/// mutex at its terminal store. Without this, an override landing
/// mid-cycle could be silently overwritten by the monitor's
/// from-scratch state rebuild for up to one poll interval.
pub fn set_quota_override(database: &str, max_size: u64) {
    let mut overrides = QUOTA_OVERRIDES.lock();
    overrides.insert(database.to_string(), max_size);

    // Update current state immediately so the change is visible.
    let mut state = (**QUOTA_STATE.load()).clone();
    if let Some(status) = state.get_mut(database) {
        status.max_size = max_size;
        status.over_limit = status.current_size > max_size;
        QUOTA_STATE.store(Arc::new(state));
    }
}

/// Clear a runtime override, reverting to config value. If the
/// database has an entry in `QUOTA_STATE`, its `max_size` /
/// `over_limit` are updated immediately so `SHOW QUOTAS` reflects
/// the revert without waiting for the next monitor poll. Symmetric
/// with `set_quota_override`; holds `QUOTA_OVERRIDES` across the
/// state mutation so `monitor_loop` can't race-overwrite the revert.
pub fn clear_quota_override(database: &str) {
    let mut overrides = QUOTA_OVERRIDES.lock();
    overrides.remove(database);

    // Look up the config value to snap QUOTA_STATE back in place.
    let cfg = config();
    let config_max = cfg
        .config
        .databases
        .iter()
        .find(|db| db.name == database)
        .and_then(|db| db.max_db_size);

    if let Some(max_size) = config_max {
        let mut state = (**QUOTA_STATE.load()).clone();
        if let Some(status) = state.get_mut(database) {
            status.max_size = max_size;
            status.over_limit = status.current_size > max_size;
            QUOTA_STATE.store(Arc::new(state));
        }
    }
}

/// Read the current runtime override for a database, if any.
/// Returns `None` when no override has been set (config value applies).
pub fn quota_override(database: &str) -> Option<u64> {
    QUOTA_OVERRIDES.lock().get(database).copied()
}

/// Collected quota configs from the database configuration.
#[derive(Debug, Clone)]
struct QuotaTarget {
    pool_name: String,
    pg_database_name: String,
    host: String,
    port: u16,
    user: String,
    password: String,
    max_size: u64,
}

/// Connection timeout for monitor queries.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the background quota monitor task.
pub fn spawn_monitor() -> Option<tokio::task::JoinHandle<()>> {
    let targets = collect_targets();
    if targets.is_empty() {
        info!("quota monitor: no databases with max_db_size configured, not starting");
        return None;
    }

    let interval_ms = config().config.general.quota_poll_interval;
    info!(
        "quota monitor: tracking {} database(s), polling every {}ms",
        targets.len(),
        interval_ms
    );

    // Fail-closed: pre-seed all configured databases as over-limit.
    // Writes are blocked until the first successful poll confirms
    // the actual size is within the limit. This prevents a window
    // where enforcement is disabled during startup or if the backend
    // is unreachable.
    let mut initial_state = HashMap::new();
    for target in &targets {
        initial_state.insert(
            target.pool_name.clone(),
            QuotaStatus {
                database: target.pool_name.clone(),
                current_size: u64::MAX,
                max_size: target.max_size,
                over_limit: true,
            },
        );
    }
    QUOTA_STATE.store(Arc::new(initial_state));

    Some(tokio::spawn(async move {
        monitor_loop(Duration::from_millis(interval_ms)).await;
    }))
}

/// Collect quota targets from current config (re-read each cycle).
fn collect_targets() -> Vec<QuotaTarget> {
    let cfg = config();
    let mut targets = Vec::new();

    // Build a sorted list of users for deterministic fallback.
    let mut user_list: Vec<(String, String)> = cfg
        .users
        .users
        .iter()
        .map(|u| (u.name.clone(), u.password.clone().unwrap_or_default()))
        .collect();
    user_list.sort_by(|a, b| a.0.cmp(&b.0));
    let users: HashMap<String, String> = user_list.iter().cloned().collect();
    let first_user = user_list.first().map(|(n, _)| n.clone());

    // Group databases by name — only check the primary for size.
    let mut seen = std::collections::HashSet::new();

    for db in &cfg.config.databases {
        let max_size = match db.max_db_size {
            Some(s) if s > 0 => s,
            _ => continue,
        };

        if !seen.insert(db.name.clone()) {
            if db.role == pgdog_config::Role::Primary {
                targets.retain(|t: &QuotaTarget| t.pool_name != db.name);
            } else {
                continue;
            }
        }

        let pg_database_name = db.database_name.clone().unwrap_or_else(|| db.name.clone());
        let user = db
            .user
            .clone()
            .unwrap_or_else(|| first_user.clone().unwrap_or_else(|| "postgres".to_string()));
        let password = db
            .password
            .clone()
            .unwrap_or_else(|| users.get(&user).cloned().unwrap_or_default());

        targets.push(QuotaTarget {
            pool_name: db.name.clone(),
            pg_database_name,
            host: db.host.clone(),
            port: db.port,
            user,
            password,
            max_size,
        });
    }

    targets
}

/// Main monitoring loop. Re-reads config each cycle to pick up
/// config reloads (new databases, changed limits, rotated credentials).
async fn monitor_loop(interval: Duration) {
    loop {
        let targets = collect_targets();
        // Snapshot overrides under lock, release immediately.
        let overrides: HashMap<String, u64> = QUOTA_OVERRIDES.lock().clone();
        let mut new_state = HashMap::new();

        for target in &targets {
            // Apply admin override if one exists.
            let effective_max = overrides
                .get(&target.pool_name)
                .copied()
                .unwrap_or(target.max_size);

            let was_over = QUOTA_STATE
                .load()
                .get(&target.pool_name)
                .map(|s| s.over_limit)
                .unwrap_or(true); // fail-closed: assume over if unknown

            match tokio::time::timeout(CHECK_TIMEOUT, check_size(target)).await {
                Ok(Ok(size)) => {
                    let over_limit = size > effective_max;

                    // Server-side enforcement: toggle default_transaction_read_only
                    // on the actual Postgres database. This blocks writes at the
                    // server level, regardless of protocol (simple or extended).
                    // Only ALTER when state changes to avoid redundant DDL.
                    if over_limit != was_over {
                        let read_only = if over_limit { "on" } else { "off" };
                        let target_clone = target.clone();
                        match tokio::time::timeout(
                            CHECK_TIMEOUT,
                            set_read_only(&target_clone, read_only),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                info!(
                                    "quota monitor: set default_transaction_read_only={} on '{}'",
                                    read_only, target.pool_name
                                );
                            }
                            Ok(Err(e)) => {
                                error!(
                                    "quota monitor: failed to set read_only on '{}': {}",
                                    target.pool_name, e
                                );
                            }
                            Err(_) => {
                                error!(
                                    "quota monitor: timeout setting read_only on '{}'",
                                    target.pool_name
                                );
                            }
                        }
                    }

                    if over_limit {
                        warn!(
                            "quota monitor: database '{}' over limit ({} > {} bytes)",
                            target.pool_name, size, effective_max
                        );
                    } else {
                        debug!(
                            "quota monitor: database '{}' size={} limit={}",
                            target.pool_name, size, effective_max
                        );
                    }
                    new_state.insert(
                        target.pool_name.clone(),
                        QuotaStatus {
                            database: target.pool_name.clone(),
                            current_size: size,
                            max_size: effective_max,
                            over_limit,
                        },
                    );
                }
                Ok(Err(e)) => {
                    error!(
                        "quota monitor: failed to check size for '{}': {}",
                        target.pool_name, e
                    );
                    // Preserve previous state on error.
                    if let Some(prev) = QUOTA_STATE.load().get(&target.pool_name) {
                        new_state.insert(target.pool_name.clone(), prev.clone());
                    }
                }
                Err(_) => {
                    error!(
                        "quota monitor: timeout checking size for '{}' ({}s)",
                        target.pool_name,
                        CHECK_TIMEOUT.as_secs()
                    );
                    if let Some(prev) = QUOTA_STATE.load().get(&target.pool_name) {
                        new_state.insert(target.pool_name.clone(), prev.clone());
                    }
                }
            }
        }

        // Re-apply overrides under the QUOTA_OVERRIDES lock before
        // committing new_state. Any SET QUOTA / RESET QUOTA that
        // landed while we were doing network I/O is visible here
        // because `set_quota_override` / `clear_quota_override` hold
        // the same mutex across their own QUOTA_STATE store. Without
        // this step, a late override would be silently reverted for
        // the remainder of the cycle when we commit below.
        {
            let overrides_now = QUOTA_OVERRIDES.lock();
            for (db, &effective_max) in overrides_now.iter() {
                if let Some(status) = new_state.get_mut(db) {
                    status.max_size = effective_max;
                    status.over_limit = status.current_size > effective_max;
                }
            }
            QUOTA_STATE.store(Arc::new(new_state));
        }
        tokio::time::sleep(interval).await;
    }
}

/// Check a single database's size using a dedicated connection.
/// Uses TLS when PgDog's `tls_verify` is set to anything other than `disabled`.
async fn check_size(target: &QuotaTarget) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    // Use Config builder to avoid password in error Display.
    let mut pg_config = tokio_postgres::Config::new();
    pg_config
        .host(&target.host)
        .port(target.port)
        .user(&target.user)
        .password(&target.password)
        .dbname(&target.pg_database_name);

    let cfg = config();
    let tls_mode = cfg.config.general.tls_verify;

    let conn_handle = match tls_mode {
        TlsVerifyMode::Disabled => {
            let (client, connection) = pg_config.connect(NoTls).await?;
            let handle = tokio::spawn(async move {
                if let Err(e) = connection.await {
                    debug!("quota monitor connection closed: {}", e);
                }
            });
            run_size_query(client, handle).await
        }
        _ => {
            // Build a rustls ClientConfig matching PgDog's TLS settings.
            let client_config = crate::net::tls::client_config_for_verify_mode(
                tls_mode,
                cfg.config.general.tls_server_ca_certificate.as_ref(),
            )
            .map_err(|e| format!("quota monitor TLS setup failed: {}", e))?;

            let tls = tokio_postgres_rustls::MakeRustlsConnect::new((*client_config).clone());
            let (client, connection) = pg_config.connect(tls).await?;
            let handle = tokio::spawn(async move {
                if let Err(e) = connection.await {
                    debug!("quota monitor connection closed: {}", e);
                }
            });
            run_size_query(client, handle).await
        }
    };

    conn_handle
}

async fn run_size_query(
    client: tokio_postgres::Client,
    conn_handle: tokio::task::JoinHandle<()>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let result = client
        .query_one("SELECT pg_database_size(current_database())", &[])
        .await;

    drop(client);
    conn_handle.abort();

    let row = result?;
    let size: i64 = row.get(0);
    Ok(size.max(0) as u64)
}

/// Set default_transaction_read_only on a database via ALTER DATABASE.
/// This provides server-side write blocking that works regardless of
/// the client protocol (simple or extended query).
async fn set_read_only(
    target: &QuotaTarget,
    value: &'static str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut pg_config = tokio_postgres::Config::new();
    pg_config
        .host(&target.host)
        .port(target.port)
        .user(&target.user)
        .password(&target.password)
        .dbname(&target.pg_database_name);

    let cfg = config();
    let tls_mode = cfg.config.general.tls_verify;

    // Connect — reuse the same TLS logic as check_size.
    let (client, conn_handle) = match tls_mode {
        TlsVerifyMode::Disabled => {
            let (c, connection) = pg_config.connect(NoTls).await?;
            let h = tokio::spawn(async move {
                let _ = connection.await;
            });
            (c, h)
        }
        _ => {
            let client_config = crate::net::tls::client_config_for_verify_mode(
                tls_mode,
                cfg.config.general.tls_server_ca_certificate.as_ref(),
            )
            .map_err(|e| format!("TLS setup: {}", e))?;

            let tls = tokio_postgres_rustls::MakeRustlsConnect::new((*client_config).clone());
            let (c, connection) = pg_config.connect(tls).await?;
            let h = tokio::spawn(async move {
                let _ = connection.await;
            });
            (c, h)
        }
    };

    // ALTER DATABASE requires the database name, not current_database().
    // Use the pg_database_name from target config.
    let sql = format!(
        "ALTER DATABASE {} SET default_transaction_read_only = {}",
        quote_ident(&target.pg_database_name),
        value
    );
    client.execute(&sql, &[]).await?;

    drop(client);
    conn_handle.abort();

    Ok(())
}

/// Quote a SQL identifier to prevent injection.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
