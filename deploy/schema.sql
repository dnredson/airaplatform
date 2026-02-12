CREATE TABLE IF NOT EXISTS entities (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    doc JSONB NOT NULL,
    version BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS entities_type_idx ON entities (entity_type);