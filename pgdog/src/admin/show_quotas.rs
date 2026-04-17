//! SHOW QUOTAS admin command.

use crate::quota;

use super::prelude::*;

pub struct ShowQuotas;

#[async_trait]
impl Command for ShowQuotas {
    fn name(&self) -> String {
        "SHOW QUOTAS".into()
    }

    fn parse(_sql: &str) -> Result<Self, Error> {
        Ok(ShowQuotas {})
    }

    async fn execute(&self) -> Result<Vec<Message>, Error> {
        let rd = RowDescription::new(&[
            Field::text("database"),
            Field::bigint("current_size"),
            Field::bigint("max_size"),
            Field::bool("over_limit"),
        ]);

        let mut messages = vec![rd.message()?];

        let mut statuses = quota::all_quota_statuses();
        statuses.sort_by(|a, b| a.database.cmp(&b.database));

        for status in statuses {
            let mut row = DataRow::new();
            // Clamp u64::MAX (fail-closed sentinel) to -1 for display.
            let current = if status.current_size == u64::MAX {
                -1i64
            } else {
                status.current_size.min(i64::MAX as u64) as i64
            };
            row.add(status.database.as_str())
                .add(current)
                .add(status.max_size.min(i64::MAX as u64) as i64)
                .add(status.over_limit);
            messages.push(row.message()?);
        }

        Ok(messages)
    }
}
