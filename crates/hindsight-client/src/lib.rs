use anyhow::Result;
use async_trait::async_trait;
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemoryUnit {
    pub unit_id: Uuid,
    pub bank_id: Uuid,
    pub content: String,
    pub fact_type: String,
    pub metadata: serde_json::Value,
}

#[async_trait]
pub trait HindsightMemory: Send + Sync {
    /// Retain a new memory unit (Experience or Fact)
    async fn retain(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        content: &str,
        fact_type: &str,
        embedding: Option<Vec<f32>>,
        metadata: serde_json::Value,
    ) -> Result<Uuid>;

    /// Recall relevant memory units using hybrid search
    async fn recall(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        query_embedding: Vec<f32>,
        limit: i32,
    ) -> Result<Vec<MemoryUnit>>;
}

pub struct PgHindsightClient {
    pool: PgPool,
}

impl PgHindsightClient {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl HindsightMemory for PgHindsightClient {
    async fn retain(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        content: &str,
        fact_type: &str,
        embedding: Option<Vec<f32>>,
        metadata: serde_json::Value,
    ) -> Result<Uuid> {
        let unit_id = Uuid::new_v4();
        let vector = embedding.map(Vector::from);

        sqlx::query(
            "INSERT INTO agent_memory_unit_t 
            (host_id, unit_id, bank_id, content, fact_type, embedding, metadata)
            VALUES ($1, $2, $3, $4, $5, $6, $7)"
        )
        .bind(host_id)
        .bind(unit_id)
        .bind(bank_id)
        .bind(content)
        .bind(fact_type)
        .bind(vector)
        .bind(metadata)
        .execute(&self.pool)
        .await?;

        Ok(unit_id)
    }

    async fn recall(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        query_embedding: Vec<f32>,
        limit: i32,
    ) -> Result<Vec<MemoryUnit>> {
        let vector = Vector::from(query_embedding);

        let rows = sqlx::query_as::<_, MemoryUnit>(
            "SELECT unit_id, bank_id, content, fact_type, metadata
            FROM agent_memory_unit_t
            WHERE host_id = $1 AND bank_id = $2
            ORDER BY embedding <=> $3
            LIMIT $4"
        )
        .bind(host_id)
        .bind(bank_id)
        .bind(vector)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}
