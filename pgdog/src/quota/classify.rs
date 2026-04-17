//! Classify SQL statements as shrink operations.
//!
//! Shrink operations (DELETE, TRUNCATE, DROP, VACUUM) are allowed
//! even when a database is over its size quota, so tenants can
//! reduce their usage.

use crate::frontend::router::Ast;
use pgdog_plugin::pg_query::NodeEnum;

/// Returns true if the AST node is a data-modifying write that should
/// be blocked when over quota. Control statements (SET, SHOW, transaction
/// control, DISCARD, LISTEN, etc.) are NOT data-modifying.
fn is_data_write_node(node: &Option<NodeEnum>) -> bool {
    matches!(
        node,
        Some(NodeEnum::InsertStmt(_))
            | Some(NodeEnum::UpdateStmt(_))
            | Some(NodeEnum::CreateStmt(_))
            | Some(NodeEnum::IndexStmt(_))
            | Some(NodeEnum::CreateTableAsStmt(_))
            | Some(NodeEnum::CreateSchemaStmt(_))
            | Some(NodeEnum::CreateSeqStmt(_))
            | Some(NodeEnum::AlterTableStmt(_))
            | Some(NodeEnum::RenameStmt(_))
            | Some(NodeEnum::ViewStmt(_))
            | Some(NodeEnum::CreateFunctionStmt(_))
            | Some(NodeEnum::CreateTrigStmt(_))
            | Some(NodeEnum::CreateExtensionStmt(_))
            | Some(NodeEnum::CreateRoleStmt(_))
            | Some(NodeEnum::AlterRoleStmt(_))
            | Some(NodeEnum::GrantStmt(_))
            | Some(NodeEnum::RuleStmt(_))
            | Some(NodeEnum::CommentStmt(_))
            | Some(NodeEnum::ReindexStmt(_))
            | Some(NodeEnum::ClusterStmt(_))
    )
}

/// Returns true if the AST contains at least one data-modifying write.
/// Used to distinguish actual writes from control statements (SET, etc.)
/// that the router classifies as "write" by default.
pub fn is_data_write(ast: &Ast) -> bool {
    let stmts = &ast.parse_result().protobuf.stmts;
    stmts.iter().any(|raw_stmt| {
        raw_stmt
            .stmt
            .as_ref()
            .map(|s| is_data_write_node(&s.node))
            .unwrap_or(false)
    })
}

/// Returns true if the AST represents a COPY FROM (data import).
pub fn is_copy_from(ast: &Ast) -> bool {
    let stmts = &ast.parse_result().protobuf.stmts;
    stmts.iter().any(|raw_stmt| {
        raw_stmt.stmt.as_ref().map_or(false, |s| {
            matches!(&s.node, Some(NodeEnum::CopyStmt(copy)) if copy.is_from)
        })
    })
}

fn is_shrink_node(node: &Option<NodeEnum>) -> bool {
    matches!(
        node,
        Some(NodeEnum::DeleteStmt(_))
            | Some(NodeEnum::TruncateStmt(_))
            | Some(NodeEnum::DropStmt(_))
            | Some(NodeEnum::VacuumStmt(_))
    )
}

/// Returns true if the AST represents a shrink operation that
/// should be allowed even when the database is over quota.
///
/// For multi-statement queries, ALL statements must be shrink
/// operations. A mixed batch like `DELETE FROM t; INSERT INTO t ...`
/// is blocked to prevent quota bypass via multi-statement injection.
pub fn is_shrink_operation(ast: &Ast) -> bool {
    let stmts = &ast.parse_result().protobuf.stmts;

    if stmts.is_empty() {
        return false;
    }

    stmts.iter().all(|raw_stmt| {
        raw_stmt
            .stmt
            .as_ref()
            .map(|s| is_shrink_node(&s.node))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::router::Ast;

    fn parse_ast(sql: &str) -> Ast {
        let result = pgdog_plugin::pg_query::parse(sql).unwrap();
        Ast::from_parse_result(result)
    }

    #[test]
    fn delete_is_shrink() {
        let ast = parse_ast("DELETE FROM users WHERE id = 1");
        assert!(is_shrink_operation(&ast));
    }

    #[test]
    fn truncate_is_shrink() {
        let ast = parse_ast("TRUNCATE TABLE users");
        assert!(is_shrink_operation(&ast));
    }

    #[test]
    fn drop_table_is_shrink() {
        let ast = parse_ast("DROP TABLE users");
        assert!(is_shrink_operation(&ast));
    }

    #[test]
    fn vacuum_is_shrink() {
        let ast = parse_ast("VACUUM users");
        assert!(is_shrink_operation(&ast));
    }

    #[test]
    fn insert_is_not_shrink() {
        let ast = parse_ast("INSERT INTO users (name) VALUES ('test')");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn update_is_not_shrink() {
        let ast = parse_ast("UPDATE users SET name = 'test' WHERE id = 1");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn select_is_not_shrink() {
        let ast = parse_ast("SELECT * FROM users");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn create_table_is_not_shrink() {
        let ast = parse_ast("CREATE TABLE test (id int)");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn copy_from_is_not_shrink() {
        let ast = parse_ast("COPY users FROM '/tmp/data.csv'");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn drop_index_is_shrink() {
        let ast = parse_ast("DROP INDEX idx_users_name");
        assert!(is_shrink_operation(&ast));
    }

    #[test]
    fn multi_stmt_all_shrink_is_shrink() {
        let ast = parse_ast("DELETE FROM a; TRUNCATE b");
        assert!(is_shrink_operation(&ast));
    }

    #[test]
    fn multi_stmt_mixed_is_not_shrink() {
        let ast = parse_ast("DELETE FROM a WHERE false; INSERT INTO a VALUES (1)");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn cte_with_delete_is_not_shrink() {
        // CTEs wrapping writes are SELECT at the root level.
        let ast = parse_ast("WITH d AS (DELETE FROM a RETURNING *) SELECT * FROM d");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn create_index_is_not_shrink() {
        let ast = parse_ast("CREATE INDEX idx ON users (name)");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn alter_table_is_not_shrink() {
        let ast = parse_ast("ALTER TABLE users ADD COLUMN email text");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn drop_database_is_not_shrink() {
        // DROP DATABASE is DropdbStmt, not DropStmt — it's an admin
        // command that shouldn't be allowed through quota enforcement
        // (and would fail anyway since you can't drop the current DB).
        let ast = parse_ast("DROP DATABASE mydb");
        assert!(!is_shrink_operation(&ast));
    }

    #[test]
    fn vacuum_full_is_shrink() {
        let ast = parse_ast("VACUUM FULL users");
        assert!(is_shrink_operation(&ast));
    }

    #[test]
    fn delete_with_subquery_is_shrink() {
        let ast = parse_ast("DELETE FROM users WHERE id IN (SELECT id FROM old_users)");
        assert!(is_shrink_operation(&ast));
    }

    // ── is_data_write tests ──────────────────────────────────────────

    #[test]
    fn insert_is_data_write() {
        let ast = parse_ast("INSERT INTO t (x) VALUES (1)");
        assert!(is_data_write(&ast));
    }

    #[test]
    fn update_is_data_write() {
        let ast = parse_ast("UPDATE t SET x = 1");
        assert!(is_data_write(&ast));
    }

    #[test]
    fn create_table_is_data_write() {
        let ast = parse_ast("CREATE TABLE t (id int)");
        assert!(is_data_write(&ast));
    }

    #[test]
    fn create_index_is_data_write() {
        let ast = parse_ast("CREATE INDEX idx ON t (id)");
        assert!(is_data_write(&ast));
    }

    #[test]
    fn alter_table_is_data_write() {
        let ast = parse_ast("ALTER TABLE t ADD COLUMN x text");
        assert!(is_data_write(&ast));
    }

    #[test]
    fn select_is_not_data_write() {
        let ast = parse_ast("SELECT * FROM t");
        assert!(!is_data_write(&ast));
    }

    #[test]
    fn set_is_not_data_write() {
        let ast = parse_ast("SET statement_timeout = 5000");
        assert!(!is_data_write(&ast));
    }

    #[test]
    fn begin_is_not_data_write() {
        let ast = parse_ast("BEGIN");
        assert!(!is_data_write(&ast));
    }

    #[test]
    fn commit_is_not_data_write() {
        let ast = parse_ast("COMMIT");
        assert!(!is_data_write(&ast));
    }

    #[test]
    fn show_is_not_data_write() {
        let ast = parse_ast("SHOW server_version");
        assert!(!is_data_write(&ast));
    }

    #[test]
    fn delete_is_not_data_write() {
        // DELETE is a shrink op, not a data write.
        let ast = parse_ast("DELETE FROM t WHERE id = 1");
        assert!(!is_data_write(&ast));
    }

    #[test]
    fn truncate_is_not_data_write() {
        let ast = parse_ast("TRUNCATE t");
        assert!(!is_data_write(&ast));
    }

    // ── is_copy_from tests ───────────────────────────────────────────

    #[test]
    fn copy_from_is_copy() {
        let ast = parse_ast("COPY t FROM '/tmp/data.csv'");
        assert!(is_copy_from(&ast));
    }

    #[test]
    fn copy_to_is_not_copy_from() {
        let ast = parse_ast("COPY t TO '/tmp/data.csv'");
        assert!(!is_copy_from(&ast));
    }

    #[test]
    fn select_is_not_copy_from() {
        let ast = parse_ast("SELECT 1");
        assert!(!is_copy_from(&ast));
    }
}
