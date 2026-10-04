//! Shared scratch lifecycle only. Each suite validates its own explicit URL and
//! provisions its roles/schema; no ambient URL or application fallback.
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

pub(super) struct ScratchDatabase {
    pub pool: PgPool,
    pub admin: PgPool,
    name: String,
}
// Unquoted ASCII identifiers: [a-z_][a-z0-9_]*, at most 30 bytes.
// PostgreSQL's 63-byte limit includes the underscore and 32 hex UUID bytes.
fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 30
        && prefix
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
impl ScratchDatabase {
    pub async fn create(url: &url::Url, prefix: &str, connections: u32) -> Self {
        assert!(
            valid_prefix(prefix),
            "unsafe or overlong scratch database prefix"
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(url.as_str())
            .await
            .unwrap();
        let name = format!("{prefix}_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut child = url.clone();
        child.set_path(&format!("/{name}"));
        let pool = PgPoolOptions::new()
            .max_connections(connections)
            .after_connect(|c, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(child.as_str())
            .await
            .unwrap();
        Self { pool, admin, name }
    }
    pub async fn migrate(&self, role: Option<&str>) {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/workflow-store/migrations/workflow-postgres");
        let mut files: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        files.sort();
        let mut connection = self.pool.acquire().await.unwrap();
        if let Some(role) = role {
            assert_eq!(role, "operations_workflow_migrator");
            sqlx::raw_sql("SET ROLE operations_workflow_migrator")
                .execute(&mut *connection)
                .await
                .unwrap();
        }
        for file in files {
            sqlx::raw_sql(&std::fs::read_to_string(&file).unwrap())
                .execute(&mut *connection)
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        }
        if role.is_some() {
            sqlx::raw_sql("RESET ROLE")
                .execute(&mut *connection)
                .await
                .unwrap();
        }
    }
    pub async fn close(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

#[test]
fn scratch_identifier_prefix_contract() {
    for prefix in [
        "p01_timer",
        "workflow_retry_case",
        "_fixture2",
        &"a".repeat(30),
    ] {
        assert!(valid_prefix(prefix), "{prefix}");
        assert!(format!("{prefix}_{}", Uuid::new_v4().simple()).len() <= 63);
    }
    for prefix in [
        "",
        "1timer",
        "Upper",
        "a-b",
        "a;b",
        "a\"",
        "a b",
        "é",
        &"a".repeat(31),
    ] {
        assert!(!valid_prefix(prefix), "{prefix}");
    }
}
