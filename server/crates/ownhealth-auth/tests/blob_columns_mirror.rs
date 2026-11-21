//! Every column that names a blob is known to profile delete: a new `*_blob` or `blob_sha256` column
//! that is in neither `BLOB_COLUMNS` nor `HOUSEHOLD_BLOB_COLUMNS` would leave a deleted person's files
//! on disk, so this test fails until the migration that adds it registers it.

use std::collections::BTreeSet;

use ownhealth_auth::household::{BLOB_COLUMNS, HOUSEHOLD_BLOB_COLUMNS};
use ownhealth_core::testing::TestDb;

#[tokio::test]
async fn every_blob_column_in_the_schema_is_registered_for_profile_delete() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let schema: Vec<(String, String)> = sqlx::query_as(
        "select c.table_name::text, c.column_name::text from information_schema.columns c \
         join information_schema.tables t on t.table_schema = c.table_schema and t.table_name = c.table_name \
         where c.table_schema = current_schema() and t.table_type = 'BASE TABLE' and c.data_type = 'bytea' \
         and (c.column_name like '%\\_blob' or c.column_name = 'blob_sha256') order by 1, 2",
    )
    .fetch_all(&db.pool)
    .await?;
    let schema: BTreeSet<(String, String)> = schema.into_iter().collect();
    let registered: BTreeSet<(String, String)> = BLOB_COLUMNS
        .iter()
        .flat_map(|(t, cols, _)| cols.iter().map(move |c| (t.to_string(), c.to_string())))
        .chain(
            HOUSEHOLD_BLOB_COLUMNS
                .iter()
                .map(|(t, c)| (t.to_string(), c.to_string())),
        )
        .collect();
    let missing: Vec<_> = schema.difference(&registered).collect();
    assert!(
        missing.is_empty(),
        "blob columns profile delete does not know: {missing:?}; add each to BLOB_COLUMNS (with the rows a \
         profile owns) or to HOUSEHOLD_BLOB_COLUMNS"
    );
    // and nothing registered is a typo the schema does not have
    let unknown: Vec<_> = registered.difference(&schema).collect();
    assert!(
        unknown.is_empty(),
        "registered blob columns the schema does not have: {unknown:?}"
    );
    Ok(())
}
