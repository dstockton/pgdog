//! Unit tests for quota enforcement.
//!
//! These tests mutate global QUOTA_STATE and CONFIG, so they must run serially.
//! Each test saves and restores state to avoid interference.

use super::*;
use std::sync::Arc;

use crate::test_lock::GLOBAL_TEST_LOCK;

fn with_clean_state<F: FnOnce()>(f: F) {
    let _guard = GLOBAL_TEST_LOCK.lock();
    let prev_state = QUOTA_STATE.load().clone();
    let prev_overrides = QUOTA_OVERRIDES.lock().clone();
    f();
    QUOTA_STATE.store(prev_state);
    *QUOTA_OVERRIDES.lock() = prev_overrides;
}

fn with_config_and_clean_state<F: FnOnce()>(cfg: pgdog_config::ConfigAndUsers, f: F) {
    let _guard = GLOBAL_TEST_LOCK.lock();
    let prev_config = crate::config::config().clone();
    let prev_state = QUOTA_STATE.load().clone();
    let prev_overrides = QUOTA_OVERRIDES.lock().clone();

    let _ = crate::config::set(cfg);
    f();

    let _ = crate::config::set((*prev_config).clone());
    QUOTA_STATE.store(prev_state);
    *QUOTA_OVERRIDES.lock() = prev_overrides;
}

fn make_config_with_databases(
    databases: Vec<pgdog_config::Database>,
    users: Vec<pgdog_config::User>,
) -> pgdog_config::ConfigAndUsers {
    let mut cfg = pgdog_config::ConfigAndUsers::default();
    cfg.config.databases = databases;
    cfg.users.users = users;
    cfg
}

fn make_db(name: &str, host: &str, max_db_size: Option<u64>) -> pgdog_config::Database {
    pgdog_config::Database {
        name: name.to_string(),
        host: host.to_string(),
        port: 5432,
        max_db_size,
        ..Default::default()
    }
}

fn make_db_with_role(
    name: &str,
    host: &str,
    role: pgdog_config::Role,
    max_db_size: Option<u64>,
) -> pgdog_config::Database {
    pgdog_config::Database {
        name: name.to_string(),
        host: host.to_string(),
        port: 5432,
        role,
        max_db_size,
        ..Default::default()
    }
}

fn make_user(name: &str, password: &str) -> pgdog_config::User {
    pgdog_config::User {
        name: name.to_string(),
        password: Some(password.to_string()),
        ..Default::default()
    }
}

// ── Basic state management ───────────────────────────────────────────

#[test]
fn test_is_over_quota_unknown_db() {
    with_clean_state(|| {
        assert!(!is_over_quota("nonexistent_db"));
    });
}

#[test]
fn test_quota_status_returns_none_for_unknown() {
    with_clean_state(|| {
        assert!(quota_status("nonexistent_db").is_none());
    });
}

#[test]
fn test_quota_status_under_limit() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "test_db".to_string(),
            QuotaStatus {
                database: "test_db".to_string(),
                current_size: 500_000,
                max_size: 1_000_000,
                over_limit: false,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        let status = quota_status("test_db").unwrap();
        assert_eq!(status.current_size, 500_000);
        assert_eq!(status.max_size, 1_000_000);
        assert!(!status.over_limit);
        assert!(!is_over_quota("test_db"));
    });
}

#[test]
fn test_over_quota_detection() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "big_db".to_string(),
            QuotaStatus {
                database: "big_db".to_string(),
                current_size: 2_000_000,
                max_size: 1_000_000,
                over_limit: true,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        assert!(is_over_quota("big_db"));
        assert!(!is_over_quota("other_db"));
    });
}

#[test]
fn test_all_quota_statuses_values() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "db1".to_string(),
            QuotaStatus {
                database: "db1".to_string(),
                current_size: 100,
                max_size: 200,
                over_limit: false,
            },
        );
        state.insert(
            "db2".to_string(),
            QuotaStatus {
                database: "db2".to_string(),
                current_size: 300,
                max_size: 200,
                over_limit: true,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        let statuses = all_quota_statuses();
        assert_eq!(statuses.len(), 2);

        let db1 = statuses.iter().find(|s| s.database == "db1").unwrap();
        assert_eq!(db1.current_size, 100);
        assert_eq!(db1.max_size, 200);
        assert!(!db1.over_limit);

        let db2 = statuses.iter().find(|s| s.database == "db2").unwrap();
        assert_eq!(db2.current_size, 300);
        assert!(db2.over_limit);
    });
}

// ── Multi-tenant isolation ───────────────────────────────────────────

#[test]
fn test_multi_tenant_one_over_one_under() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "tenant_a".to_string(),
            QuotaStatus {
                database: "tenant_a".to_string(),
                current_size: 2_000_000,
                max_size: 1_000_000,
                over_limit: true,
            },
        );
        state.insert(
            "tenant_b".to_string(),
            QuotaStatus {
                database: "tenant_b".to_string(),
                current_size: 100_000,
                max_size: 1_000_000,
                over_limit: false,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        // tenant_a is blocked, tenant_b is not.
        assert!(is_over_quota("tenant_a"));
        assert!(!is_over_quota("tenant_b"));
    });
}

// ── Override management ──────────────────────────────────────────────

#[test]
fn test_quota_override_persists() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "resize_db".to_string(),
            QuotaStatus {
                database: "resize_db".to_string(),
                current_size: 800_000,
                max_size: 500_000,
                over_limit: true,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        assert!(is_over_quota("resize_db"));

        set_quota_override("resize_db", 1_000_000);

        assert!(!is_over_quota("resize_db"));
        let status = quota_status("resize_db").unwrap();
        assert_eq!(status.max_size, 1_000_000);
        assert!(!status.over_limit);

        assert_eq!(
            QUOTA_OVERRIDES.lock().get("resize_db").copied(),
            Some(1_000_000u64)
        );

        clear_quota_override("resize_db");
        assert!(QUOTA_OVERRIDES.lock().get("resize_db").is_none());
    });
}

#[test]
fn test_clear_override_snaps_state_back_to_config() {
    // When config has a max_db_size for the database, clear_quota_override
    // must update QUOTA_STATE in place so SHOW QUOTAS sees the revert
    // immediately (symmetric with set_quota_override).
    let db = make_db("clear_revert_db", "127.0.0.1", Some(500_000));
    let user = pgdog_config::User {
        name: "alice".into(),
        database: "clear_revert_db".into(),
        password: Some("secret".into()),
        ..Default::default()
    };
    let cfg = make_config_with_databases(vec![db], vec![user]);

    with_config_and_clean_state(cfg, || {
        let mut state = HashMap::new();
        state.insert(
            "clear_revert_db".to_string(),
            QuotaStatus {
                database: "clear_revert_db".to_string(),
                current_size: 300_000,
                max_size: 500_000,
                over_limit: false,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        // Override lowers the limit below current size.
        set_quota_override("clear_revert_db", 100_000);
        let after_set = quota_status("clear_revert_db").unwrap();
        assert_eq!(after_set.max_size, 100_000);
        assert!(after_set.over_limit);

        // Clearing must snap back to the config value immediately.
        clear_quota_override("clear_revert_db");
        let after_clear = quota_status("clear_revert_db").unwrap();
        assert_eq!(after_clear.max_size, 500_000);
        assert!(!after_clear.over_limit);
        assert!(QUOTA_OVERRIDES.lock().get("clear_revert_db").is_none());
    });
}

#[test]
fn test_clear_override_no_config_is_noop_on_state() {
    // If no config entry matches (or no max_db_size), clearing should
    // just drop the override — state is untouched, since we have no
    // config value to revert to. Documents the existing behavior.
    with_clean_state(|| {
        set_quota_override("no_config_db", 777);
        clear_quota_override("no_config_db");
        assert!(QUOTA_OVERRIDES.lock().get("no_config_db").is_none());
    });
}

#[test]
fn test_quota_override_getter() {
    // Public `quota_override` reads what was inserted and returns
    // None after clear.
    with_clean_state(|| {
        assert_eq!(quota_override("getter_db"), None);
        set_quota_override("getter_db", 123);
        assert_eq!(quota_override("getter_db"), Some(123));
        clear_quota_override("getter_db");
        assert_eq!(quota_override("getter_db"), None);
    });
}

#[test]
fn test_override_for_unknown_db_stores_but_doesnt_crash() {
    with_clean_state(|| {
        set_quota_override("phantom_db", 999);
        assert_eq!(
            QUOTA_OVERRIDES.lock().get("phantom_db").copied(),
            Some(999u64)
        );
        assert!(quota_status("phantom_db").is_none());
        clear_quota_override("phantom_db");
    });
}

#[test]
fn test_override_reduces_limit_triggers_over_quota() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "shrink_limit_db".to_string(),
            QuotaStatus {
                database: "shrink_limit_db".to_string(),
                current_size: 500_000,
                max_size: 1_000_000,
                over_limit: false,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        assert!(!is_over_quota("shrink_limit_db"));
        set_quota_override("shrink_limit_db", 100_000);
        assert!(is_over_quota("shrink_limit_db"));
    });
}

// ── Fail-closed behavior ─────────────────────────────────────────────

#[test]
fn test_fail_closed_pre_seed() {
    with_clean_state(|| {
        let mut initial_state = HashMap::new();
        initial_state.insert(
            "new_tenant".to_string(),
            QuotaStatus {
                database: "new_tenant".to_string(),
                current_size: u64::MAX,
                max_size: 1_000_000,
                over_limit: true,
            },
        );
        QUOTA_STATE.store(Arc::new(initial_state));

        assert!(is_over_quota("new_tenant"));
        let status = quota_status("new_tenant").unwrap();
        assert_eq!(status.current_size, u64::MAX);
        assert!(status.over_limit);
    });
}

#[test]
fn test_fail_closed_unblocks_after_successful_poll() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "tenant_a".to_string(),
            QuotaStatus {
                database: "tenant_a".to_string(),
                current_size: u64::MAX,
                max_size: 1_000_000,
                over_limit: true,
            },
        );
        QUOTA_STATE.store(Arc::new(state));
        assert!(is_over_quota("tenant_a"));

        // Simulate successful poll.
        let mut new_state = HashMap::new();
        new_state.insert(
            "tenant_a".to_string(),
            QuotaStatus {
                database: "tenant_a".to_string(),
                current_size: 500_000,
                max_size: 1_000_000,
                over_limit: false,
            },
        );
        QUOTA_STATE.store(Arc::new(new_state));

        assert!(!is_over_quota("tenant_a"));
    });
}

// ── Boundary conditions ──────────────────────────────────────────────

#[test]
fn test_exactly_at_limit_is_not_over() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "exact_db".to_string(),
            QuotaStatus {
                database: "exact_db".to_string(),
                current_size: 1_000_000,
                max_size: 1_000_000,
                over_limit: false,
            },
        );
        QUOTA_STATE.store(Arc::new(state));
        assert!(!is_over_quota("exact_db"));
    });
}

#[test]
fn test_one_byte_over_is_over() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "one_over".to_string(),
            QuotaStatus {
                database: "one_over".to_string(),
                current_size: 1_000_001,
                max_size: 1_000_000,
                over_limit: true,
            },
        );
        QUOTA_STATE.store(Arc::new(state));
        assert!(is_over_quota("one_over"));
    });
}

// ── Metrics output ───────────────────────────────────────────────────

#[test]
fn test_metrics_output_format() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "metrics_db".to_string(),
            QuotaStatus {
                database: "metrics_db".to_string(),
                current_size: 500,
                max_size: 1000,
                over_limit: false,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        let output = crate::stats::http_server::quota_metrics();
        assert!(output.contains("pgdog_db_size_bytes{database=\"metrics_db\"} 500"));
        assert!(output.contains("pgdog_db_size_limit_bytes{database=\"metrics_db\"} 1000"));
        assert!(output.contains("pgdog_db_over_limit{database=\"metrics_db\"} 0"));
        // Verify TYPE headers present.
        assert!(output.contains("# TYPE pgdog_db_size_bytes gauge"));
        assert!(output.contains("# TYPE pgdog_db_size_limit_bytes gauge"));
        assert!(output.contains("# TYPE pgdog_db_over_limit gauge"));
    });
}

#[test]
fn test_metrics_over_limit_shows_1() {
    with_clean_state(|| {
        let mut state = HashMap::new();
        state.insert(
            "hot_db".to_string(),
            QuotaStatus {
                database: "hot_db".to_string(),
                current_size: 2000,
                max_size: 1000,
                over_limit: true,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        let output = crate::stats::http_server::quota_metrics();
        assert!(output.contains("pgdog_db_over_limit{database=\"hot_db\"} 1"));
    });
}

#[test]
fn test_metrics_empty_when_no_quotas() {
    with_clean_state(|| {
        QUOTA_STATE.store(Arc::new(HashMap::new()));
        let output = crate::stats::http_server::quota_metrics();
        assert!(output.is_empty());
    });
}

// ── Config parsing ───────────────────────────────────────────────────

#[test]
fn test_config_max_db_size_field() {
    let toml_str = r#"
        name = "tenant_db"
        host = "localhost"
        port = 5432
        max_db_size = 1073741824
    "#;
    let db: pgdog_config::Database = toml::from_str(toml_str).unwrap();
    assert_eq!(db.max_db_size, Some(1_073_741_824));
}

#[test]
fn test_config_max_db_size_optional() {
    let toml_str = r#"
        name = "no_quota_db"
        host = "localhost"
        port = 5432
    "#;
    let db: pgdog_config::Database = toml::from_str(toml_str).unwrap();
    assert_eq!(db.max_db_size, None);
}

#[test]
fn test_config_quota_poll_interval_default() {
    let general = pgdog_config::General::default();
    assert_eq!(general.quota_poll_interval, 60_000);
}

// ── collect_targets ──────────────────────────────────────────────────

#[test]
fn test_collect_targets_basic() {
    let cfg = make_config_with_databases(
        vec![make_db("tenant1", "pg1.example.com", Some(1_000_000_000))],
        vec![make_user("app", "secret")],
    );
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pool_name, "tenant1");
        assert_eq!(targets[0].host, "pg1.example.com");
        assert_eq!(targets[0].max_size, 1_000_000_000);
        assert_eq!(targets[0].user, "app");
        assert_eq!(targets[0].password, "secret");
    });
}

#[test]
fn test_collect_targets_skips_no_quota() {
    let cfg = make_config_with_databases(
        vec![
            make_db("with_quota", "pg1", Some(1_000_000)),
            make_db("no_quota", "pg2", None),
        ],
        vec![make_user("app", "pw")],
    );
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pool_name, "with_quota");
    });
}

#[test]
fn test_collect_targets_skips_zero_quota() {
    let cfg = make_config_with_databases(
        vec![make_db("zero", "pg1", Some(0))],
        vec![make_user("app", "pw")],
    );
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert!(targets.is_empty(), "zero max_db_size should be skipped");
    });
}

#[test]
fn test_collect_targets_prefers_primary() {
    let cfg = make_config_with_databases(
        vec![
            make_db_with_role(
                "mydb",
                "replica1",
                pgdog_config::Role::Replica,
                Some(1_000_000),
            ),
            make_db_with_role(
                "mydb",
                "primary1",
                pgdog_config::Role::Primary,
                Some(1_000_000),
            ),
        ],
        vec![make_user("app", "pw")],
    );
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].host, "primary1");
    });
}

#[test]
fn test_collect_targets_database_name_fallback() {
    let mut db = make_db("pool_name", "pg1", Some(1_000_000));
    db.database_name = Some("actual_pg_db".to_string());
    let cfg = make_config_with_databases(vec![db], vec![make_user("app", "pw")]);
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets[0].pool_name, "pool_name");
        assert_eq!(targets[0].pg_database_name, "actual_pg_db");
    });
}

#[test]
fn test_collect_targets_database_name_defaults_to_pool_name() {
    let cfg = make_config_with_databases(
        vec![make_db("mypool", "pg1", Some(1_000_000))],
        vec![make_user("app", "pw")],
    );
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets[0].pg_database_name, "mypool");
    });
}

#[test]
fn test_collect_targets_user_from_database_config() {
    let mut db = make_db("mydb", "pg1", Some(1_000_000));
    db.user = Some("monitor_user".to_string());
    db.password = Some("monitor_pw".to_string());
    let cfg = make_config_with_databases(vec![db], vec![make_user("app", "app_pw")]);
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets[0].user, "monitor_user");
        assert_eq!(targets[0].password, "monitor_pw");
    });
}

#[test]
fn test_collect_targets_sorted_user_fallback() {
    // With no db.user, collect_targets picks the first user alphabetically.
    let cfg = make_config_with_databases(
        vec![make_db("mydb", "pg1", Some(1_000_000))],
        vec![make_user("zoe", "zoe_pw"), make_user("alice", "alice_pw")],
    );
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets[0].user, "alice"); // sorted, deterministic
        assert_eq!(targets[0].password, "alice_pw");
    });
}

#[test]
fn test_collect_targets_uses_general_default_when_db_unset() {
    // db has no max_db_size; general.default_max_db_size kicks in.
    let mut cfg = make_config_with_databases(
        vec![make_db("inherits", "pg1", None)],
        vec![make_user("app", "pw")],
    );
    cfg.config.general.default_max_db_size = Some(2_000_000);
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pool_name, "inherits");
        assert_eq!(targets[0].max_size, 2_000_000);
    });
}

#[test]
fn test_collect_targets_per_db_overrides_general_default() {
    // Per-db value wins over the global default.
    let mut cfg = make_config_with_databases(
        vec![make_db("explicit", "pg1", Some(500))],
        vec![make_user("app", "pw")],
    );
    cfg.config.general.default_max_db_size = Some(9_999_999);
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].max_size, 500);
    });
}

#[test]
fn test_collect_targets_per_db_zero_disables_even_with_general_default() {
    // Some(0) at the db level explicitly disables enforcement, regardless
    // of any general default. This lets operators carve out exemptions.
    let mut cfg = make_config_with_databases(
        vec![
            make_db("opt_out", "pg1", Some(0)),
            make_db("inherits", "pg2", None),
        ],
        vec![make_user("app", "pw")],
    );
    cfg.config.general.default_max_db_size = Some(1_000_000);
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pool_name, "inherits");
        assert_eq!(targets[0].max_size, 1_000_000);
    });
}

#[test]
fn test_collect_targets_general_default_zero_is_disabled() {
    // Explicit zero on the general default behaves like None.
    let mut cfg = make_config_with_databases(
        vec![make_db("orphan", "pg1", None)],
        vec![make_user("app", "pw")],
    );
    cfg.config.general.default_max_db_size = Some(0);
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert!(targets.is_empty());
    });
}

#[test]
fn test_clear_override_snaps_to_general_default() {
    // db has no per-db limit but inherits the general default; after
    // clearing a runtime override, max_size should revert to the default.
    let db = make_db("inheriting_db", "127.0.0.1", None);
    let user = pgdog_config::User {
        name: "alice".into(),
        database: "inheriting_db".into(),
        password: Some("secret".into()),
        ..Default::default()
    };
    let mut cfg = make_config_with_databases(vec![db], vec![user]);
    cfg.config.general.default_max_db_size = Some(750_000);

    with_config_and_clean_state(cfg, || {
        let mut state = HashMap::new();
        state.insert(
            "inheriting_db".to_string(),
            QuotaStatus {
                database: "inheriting_db".to_string(),
                current_size: 400_000,
                max_size: 750_000,
                over_limit: false,
            },
        );
        QUOTA_STATE.store(Arc::new(state));

        set_quota_override("inheriting_db", 100_000);
        assert_eq!(quota_status("inheriting_db").unwrap().max_size, 100_000);

        clear_quota_override("inheriting_db");
        let after = quota_status("inheriting_db").unwrap();
        assert_eq!(after.max_size, 750_000);
        assert!(!after.over_limit);
    });
}

#[test]
fn test_config_default_max_db_size_optional() {
    let general = pgdog_config::General::default();
    assert_eq!(general.default_max_db_size, None);
}

#[test]
fn test_config_default_max_db_size_parses() {
    let toml_str = r#"
        default_max_db_size = 5368709120
    "#;
    let general: pgdog_config::General = toml::from_str(toml_str).unwrap();
    assert_eq!(general.default_max_db_size, Some(5_368_709_120));
}

#[test]
fn test_collect_targets_multiple_databases() {
    let cfg = make_config_with_databases(
        vec![
            make_db("t1", "pg1", Some(100)),
            make_db("t2", "pg2", Some(200)),
            make_db("t3", "pg3", Some(300)),
        ],
        vec![make_user("app", "pw")],
    );
    with_config_and_clean_state(cfg, || {
        let targets = collect_targets();
        assert_eq!(targets.len(), 3);
        let names: Vec<&str> = targets.iter().map(|t| t.pool_name.as_str()).collect();
        assert!(names.contains(&"t1"));
        assert!(names.contains(&"t2"));
        assert!(names.contains(&"t3"));
    });
}
