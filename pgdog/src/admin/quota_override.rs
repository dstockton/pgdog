//! Runtime quota override admin commands.
//!
//! `SET QUOTA <database> <bytes>` overrides a per-database size quota
//! at runtime without touching the config file. The override survives
//! monitor poll cycles until cleared with `RESET QUOTA <database>` or
//! until PgDog is restarted.
//!
//! Overrides are only meaningful for databases that already have a
//! `max_db_size` configured in `pgdog.toml` — the quota monitor only
//! polls those databases. Applying an override to a database without
//! a configured quota returns an error instead of silently doing
//! nothing.

use super::prelude::*;
use crate::quota;

pub struct SetQuota {
    database: String,
    max_size: u64,
}

pub struct ResetQuota {
    database: String,
}

/// Resolve `requested` against the configured database list using
/// case-insensitive comparison. Returns the case-preserved config
/// name so override lookups in `quota::monitor_loop` hit.
///
/// Errors:
/// - `UnknownDatabase` if no configured database matches.
/// - `QuotaNotConfigured` if matching databases exist but none would
///   be polled by the monitor — i.e. neither `max_db_size` nor an
///   inherited non-zero `default_max_db_size` applies, or every
///   matching entry sets `max_db_size = 0` to opt out. Silently
///   accepting the override would mislead operators.
/// - `AmbiguousDatabase` if two or more configured databases share
///   the requested name case-insensitively but use distinct case-
///   preserved names. Override would have to target one specific
///   HashMap key; picking silently would surprise operators.
///   Multiple entries with the *same* case-preserved name (shards,
///   primary+replica pairs) are fine — they resolve to one key.
fn resolve_quota_database(requested: &str) -> Result<String, Error> {
    let cfg = crate::config::config();
    let general_default = cfg.config.general.default_max_db_size;

    // Collect distinct case-preserved names that match case-insensitively.
    let mut distinct_names: Vec<String> = Vec::new();
    let mut has_quota = false;
    for db in &cfg.config.databases {
        if !db.name.eq_ignore_ascii_case(requested) {
            continue;
        }
        if !distinct_names.iter().any(|n| n == &db.name) {
            distinct_names.push(db.name.clone());
        }
        // Same effective-limit logic as quota::collect_targets:
        // per-db wins (Some(0) explicitly disables), otherwise inherit
        // general.default_max_db_size when it's set and non-zero.
        let effective = match db.max_db_size {
            Some(0) => None,
            Some(s) => Some(s),
            None => general_default.filter(|&s| s > 0),
        };
        if effective.is_some() {
            has_quota = true;
        }
    }

    match distinct_names.len() {
        0 => Err(Error::UnknownDatabase(requested.to_string())),
        1 => {
            let name = distinct_names.into_iter().next().unwrap();
            if !has_quota {
                return Err(Error::QuotaNotConfigured(name));
            }
            Ok(name)
        }
        _ => Err(Error::AmbiguousDatabase(
            requested.to_string(),
            distinct_names.join(", "),
        )),
    }
}

#[async_trait]
impl Command for SetQuota {
    fn name(&self) -> String {
        "SET QUOTA".into()
    }

    fn parse(sql: &str) -> Result<Self, Error> {
        let parts: Vec<&str> = sql.split_whitespace().collect();
        match parts[..] {
            ["set", "quota", database, bytes] => Ok(Self {
                database: database.to_string(),
                max_size: bytes.parse()?,
            }),
            _ => Err(Error::Syntax),
        }
    }

    async fn execute(&self) -> Result<Vec<Message>, Error> {
        let actual = resolve_quota_database(&self.database)?;
        quota::set_quota_override(&actual, self.max_size);
        Ok(vec![])
    }
}

#[async_trait]
impl Command for ResetQuota {
    fn name(&self) -> String {
        "RESET QUOTA".into()
    }

    fn parse(sql: &str) -> Result<Self, Error> {
        let parts: Vec<&str> = sql.split_whitespace().collect();
        match parts[..] {
            ["reset", "quota", database] => Ok(Self {
                database: database.to_string(),
            }),
            _ => Err(Error::Syntax),
        }
    }

    async fn execute(&self) -> Result<Vec<Message>, Error> {
        let actual = resolve_quota_database(&self.database)?;
        quota::clear_quota_override(&actual);
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_set_quota() {
        // Matches the parser.rs pre-processing: trimmed, ';' removed, lowercased.
        let cmd = SetQuota::parse("set quota tenant_a 2147483648").unwrap();
        assert_eq!(cmd.database, "tenant_a");
        assert_eq!(cmd.max_size, 2_147_483_648);
    }

    #[test]
    fn parses_set_quota_zero() {
        // Zero is a legitimate lockdown value — every size > 0 is over.
        let cmd = SetQuota::parse("set quota tenant_a 0").unwrap();
        assert_eq!(cmd.max_size, 0);
    }

    #[test]
    fn parses_reset_quota() {
        let cmd = ResetQuota::parse("reset quota tenant_a").unwrap();
        assert_eq!(cmd.database, "tenant_a");
    }

    #[test]
    fn set_quota_rejects_missing_value() {
        assert!(matches!(
            SetQuota::parse("set quota tenant_a"),
            Err(Error::Syntax)
        ));
    }

    #[test]
    fn set_quota_rejects_extra_tokens() {
        assert!(matches!(
            SetQuota::parse("set quota tenant_a 100 200"),
            Err(Error::Syntax)
        ));
    }

    #[test]
    fn set_quota_rejects_non_numeric_bytes() {
        assert!(matches!(
            SetQuota::parse("set quota tenant_a notanumber"),
            Err(Error::ParseInt(_))
        ));
    }

    #[test]
    fn set_quota_rejects_negative_bytes() {
        // u64 parse rejects leading '-'; ensures no silent wrap.
        assert!(matches!(
            SetQuota::parse("set quota tenant_a -1"),
            Err(Error::ParseInt(_))
        ));
    }

    #[test]
    fn reset_quota_rejects_missing_database() {
        assert!(matches!(
            ResetQuota::parse("reset quota"),
            Err(Error::Syntax)
        ));
    }

    #[test]
    fn reset_quota_rejects_extra_tokens() {
        assert!(matches!(
            ResetQuota::parse("reset quota tenant_a extra"),
            Err(Error::Syntax)
        ));
    }
}
