ALTER TABLE chunks ADD COLUMN sql_subjects text[] NOT NULL DEFAULT '{}';
-- One entry per subject supports a newest-path index seek, not a sort of every matching chunk.
-- Both limits keep the B-tree key below PostgreSQL's per-entry limit, even at worst case.
CREATE TABLE sql_subject_lookup (
    chunk_id bigint NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source_id uuid NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('code', 'doc', 'session', 'note')),
    subject text COLLATE "C" NOT NULL CHECK (octet_length(subject) <= 256),
    identity text COLLATE "C" NOT NULL CHECK (octet_length(identity) <= 256),
    path text COLLATE "C" NOT NULL CHECK (octet_length(path) <= 2048),
    ord int NOT NULL,
    PRIMARY KEY (chunk_id, identity)
);
CREATE INDEX sql_subject_latest ON sql_subject_lookup
    (tenant_id, source_id, kind, identity, path DESC, ord DESC, chunk_id DESC);
