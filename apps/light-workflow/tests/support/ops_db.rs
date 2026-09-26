use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use std::env;
use std::path::PathBuf;
use uuid::Uuid;

fn pool_from_env(name: &str, missing_message: &str) -> PgPool {
    let url = env::var(name).unwrap_or_else(|_| panic!("{missing_message}"));
    PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET search_path TO workflow_ops")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy(&url)
        .unwrap_or_else(|error| panic!("{name} is not a valid PostgreSQL URL: {error}"))
}

pub fn runtime_pool() -> PgPool {
    pool_from_env("DATABASE_URL", "DATABASE_URL is required")
}

pub fn admin_pool() -> PgPool {
    pool_from_env("ADMIN_DATABASE_URL", "ADMIN_DATABASE_URL is required")
}

pub fn random_host_id() -> Uuid {
    Uuid::new_v4()
}

/// `workflow_ops.wf_definition_t` has no foreign keys, including no FK to a host table.
/// A fresh host UUID is sufficient and does not require an admin-side host fixture.
pub async fn insert_workflow_definition(
    pool: &PgPool,
    host_id: Uuid,
) -> Result<(Uuid, String), sqlx::Error> {
    let wf_def_id = Uuid::new_v4();
    let name = format!("ops-smoke-{wf_def_id}");
    sqlx::query(
        "INSERT INTO wf_definition_t \
         (host_id, wf_def_id, namespace, name, version, definition, lifecycle_status) \
         VALUES ($1, $2, 'ops-smoke', $3, '1.0.0', $4, 'DRAFT')",
    )
    .bind(host_id)
    .bind(wf_def_id)
    .bind(&name)
    .bind("name: ops-smoke\nversion: 1.0.0\n")
    .execute(pool)
    .await?;
    Ok((wf_def_id, name))
}

pub fn last_workflow_migration_id() -> String {
    let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/operational-store/release/bundle/migration-order.tsv");
    let contents = std::fs::read_to_string(&bundle)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", bundle.display()));
    contents
        .lines()
        .filter_map(|line| {
            let columns: Vec<_> = line.split('\t').collect();
            (columns.len() >= 4 && columns[1] == "workflow-store" && columns[2] == "workflow_ops")
                .then(|| columns[3].to_string())
        })
        .last()
        .expect("bundle must contain a Workflow migration")
}

pub async fn definition_name(pool: &PgPool, wf_def_id: Uuid) -> Result<String, sqlx::Error> {
    let row: PgRow = sqlx::query("SELECT name FROM wf_definition_t WHERE wf_def_id = $1")
        .bind(wf_def_id)
        .fetch_one(pool)
        .await?;
    row.try_get("name")
}
