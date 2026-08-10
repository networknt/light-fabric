use knowledge_core::{EmbeddingMigrationLedger, EmbeddingMigrationState};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut migration = EmbeddingMigrationLedger {
        state: EmbeddingMigrationState::Preflighted,
        snapshot_watermark: 100,
        final_watermark: None,
        estimated_chunks: 3,
        catchup_chunks: 0,
        completed_chunks: 0,
        accepted_cost_ceiling_micros: 1_000,
        consumed_cost_micros: 0,
    };
    migration.start_backfill()?;
    migration.record_batch(2, 400)?;
    migration.record_batch(1, 200)?;
    migration.begin_catchup(1)?;
    migration.record_batch(1, 100)?;
    migration.begin_catchup(0)?;
    migration.final_fence(104, 104)?;
    assert_eq!(migration.state, EmbeddingMigrationState::Ready);
    println!(
        "{{\"status\":\"PASS\",\"phase\":\"3\",\"reusedCanonicalChunks\":4,\"finalWatermark\":104}}"
    );
    Ok(())
}
