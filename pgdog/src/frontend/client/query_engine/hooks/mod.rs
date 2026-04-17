//! Query hooks.
use super::*;
pub mod schema;

use crate::quota;
use tracing::debug;

#[derive(Debug)]
pub struct QueryEngineHooks;

impl Default for QueryEngineHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryEngineHooks {
    pub(super) fn new() -> Self {
        Self {}
    }

    pub(super) fn before_execution(
        &mut self,
        context: &mut QueryEngineContext<'_>,
    ) -> Result<(), Error> {
        self.check_quota(context)
    }

    pub(super) fn after_connected(
        &mut self,
        _context: &mut QueryEngineContext<'_>,
        _backend: &Connection,
    ) -> Result<(), Error> {
        Ok(())
    }

    pub(super) fn after_execution(
        &mut self,
        _context: &mut QueryEngineContext<'_>,
    ) -> Result<(), Error> {
        Ok(())
    }

    pub(super) fn on_server_message(
        &mut self,
        _context: &mut QueryEngineContext<'_>,
        _message: &Message,
    ) -> Result<(), Error> {
        Ok(())
    }

    pub(super) fn on_engine_error(
        &mut self,
        _context: &mut QueryEngineContext<'_>,
        _error: &ErrorResponse,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// Check if the current query should be blocked due to quota enforcement.
    fn check_quota(&self, context: &QueryEngineContext<'_>) -> Result<(), Error> {
        // Get the database name from client params.
        let user = match context.params.get_required("user") {
            Ok(u) => u,
            Err(_) => return Ok(()),
        };
        let database = context.params.get_default("database", user);

        // Check if this database is over quota.
        if !quota::is_over_quota(database) {
            return Ok(());
        }

        // Use AST to determine if the query is a data-modifying write.
        // Route-level is_write() is unreliable — in single-primary configs
        // (no replicas), all queries route to primary as "writes".
        if let Some(ref ast) = context.client_request.ast {
            // Shrink operations (DELETE, TRUNCATE, DROP, VACUUM) always pass.
            if quota::is_shrink_operation(ast) {
                debug!(
                    "quota: allowing shrink operation on over-quota database '{}'",
                    database
                );
                return Ok(());
            }

            // Only block actual data-modifying writes and COPY FROM.
            // Everything else (SELECT, SET, SHOW, BEGIN, COMMIT, etc.) passes.
            if !quota::is_data_write(ast) && !quota::is_copy_from(ast) {
                return Ok(());
            }
        } else {
            // AST unavailable — extended query protocol (Parse/Bind/Execute)
            // or query parser disabled. We cannot reliably classify the
            // statement type, so we allow it through. This is the correct
            // default because:
            // - Single-primary configs route ALL queries (including SELECTs)
            //   as "writes", so route-level check is useless
            // - Blocking here would break reads, deletes, and session control
            // - The monitor still enforces the quota on the next poll cycle
            //   if actual data was written
            debug!(
                "quota: allowing query on over-quota '{}' (AST unavailable)",
                database
            );
            return Ok(());
        }

        // Block the write.
        let status = quota::quota_status(database);
        let msg = match status {
            Some(s) => format!(
                "database '{}' has exceeded its size quota ({} bytes used, {} bytes limit)",
                database, s.current_size, s.max_size
            ),
            None => format!("database '{}' has exceeded its size quota", database),
        };

        debug!("quota: blocking write on database '{}'", database);
        Err(Error::QuotaExceeded(msg))
    }
}
