-- The workflow event consumer owns its recovery cursor. Keep this operational
-- state with the workflow runtime rather than in the Config Server schema.

SET search_path TO workflow_ops, pg_catalog;

CREATE TABLE IF NOT EXISTS workflow_ops.consumer_offsets (
    group_id text NOT NULL,
    topic_id integer NOT NULL,
    partition_id integer DEFAULT 0 NOT NULL,
    next_offset bigint DEFAULT 1 NOT NULL,
    CONSTRAINT consumer_offsets_pkey
        PRIMARY KEY (group_id, topic_id, partition_id)
);

COMMENT ON TABLE workflow_ops.consumer_offsets IS
  'Stores workflow event-consumer recovery offsets as operational state.';
COMMENT ON COLUMN workflow_ops.consumer_offsets.group_id IS
  'Identifier for the related consumer group.';
COMMENT ON COLUMN workflow_ops.consumer_offsets.topic_id IS
  'Identifier for the related topic.';
COMMENT ON COLUMN workflow_ops.consumer_offsets.partition_id IS
  'Identifier for the related partition.';
COMMENT ON COLUMN workflow_ops.consumer_offsets.next_offset IS
  'Next event offset to consume.';

GRANT SELECT, INSERT, UPDATE, DELETE ON workflow_ops.consumer_offsets
  TO operations_workflow_runtime;
