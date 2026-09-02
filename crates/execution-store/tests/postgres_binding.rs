use execution_store::ExpectedBinding;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
async fn exact_binding_is_ready_and_wrong_scope_fails_closed() {
    let Ok(database_url) = std::env::var("EXECUTION_STORE_TEST_DATABASE_URL") else {
        return;
    };
    let binding_id = Uuid::parse_str(
        &std::env::var("EXECUTION_STORE_TEST_BINDING_ID").expect("binding ID accompanies URL"),
    )
    .expect("valid binding ID");
    let host_id = Uuid::parse_str(
        &std::env::var("EXECUTION_STORE_TEST_HOST_ID").expect("Host ID accompanies URL"),
    )
    .expect("valid Host ID");
    let binding_digest =
        std::env::var("EXECUTION_STORE_TEST_BINDING_DIGEST").expect("digest accompanies URL");
    let environment =
        std::env::var("EXECUTION_STORE_TEST_ENVIRONMENT").expect("environment accompanies URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect execution store");

    execution_store::validate(
        &pool,
        &ExpectedBinding {
            binding_id,
            binding_digest: &binding_digest,
            host_id,
            environment: &environment,
            server_host: "postgres",
            port: 5432,
            tls_mode: "DISABLE",
            expected_database: "operations",
            minimum_schema_generation: 1,
        },
    )
    .await
    .expect("exact execution binding is ready");

    for wrong in [
        ExpectedBinding {
            binding_id: Uuid::now_v7(),
            binding_digest: &binding_digest,
            host_id,
            environment: &environment,
            server_host: "postgres",
            port: 5432,
            tls_mode: "DISABLE",
            expected_database: "operations",
            minimum_schema_generation: 1,
        },
        ExpectedBinding {
            binding_id,
            binding_digest: "sha256:wrong-binding",
            host_id,
            environment: &environment,
            server_host: "postgres",
            port: 5432,
            tls_mode: "DISABLE",
            expected_database: "operations",
            minimum_schema_generation: 1,
        },
        ExpectedBinding {
            binding_id,
            binding_digest: &binding_digest,
            host_id: Uuid::now_v7(),
            environment: &environment,
            server_host: "postgres",
            port: 5432,
            tls_mode: "DISABLE",
            expected_database: "operations",
            minimum_schema_generation: 1,
        },
        ExpectedBinding {
            binding_id,
            binding_digest: &binding_digest,
            host_id,
            environment: "wrong-environment",
            server_host: "postgres",
            port: 5432,
            tls_mode: "DISABLE",
            expected_database: "operations",
            minimum_schema_generation: 1,
        },
    ] {
        assert!(matches!(
            execution_store::validate(&pool, &wrong).await,
            Err(_)
        ));
    }
}
