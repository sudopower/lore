ALTER TABLE chunks ADD COLUMN sql_subjects text[] NOT NULL DEFAULT '{}';
-- One entry per subject supports a newest-path index seek, not a sort of every matching chunk.
CREATE TABLE sql_subject_lookup (
    id bigserial PRIMARY KEY,
    chunk_id bigint NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source_id uuid NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('code', 'doc', 'session', 'note')),
    subject text COLLATE "C" NOT NULL,
    identity text COLLATE "C" NOT NULL,
    identity_hash bytea NOT NULL CHECK (octet_length(identity_hash) = 32),
    path text COLLATE "C" NOT NULL,
    ord int NOT NULL,
    changes_values boolean NOT NULL DEFAULT false
);
CREATE INDEX sql_subject_chunk ON sql_subject_lookup (chunk_id);
-- Hashing the identity keeps keys small even for long quoted names. Queries also compare the
-- full identity, so hash collisions cannot link distinct objects.
CREATE INDEX sql_subject_latest ON sql_subject_lookup
    (tenant_id, source_id, kind, identity_hash, path DESC, ord DESC, chunk_id DESC)
    WHERE octet_length(path) <= 2048;
-- Repo paths have no enforced length cap. Preserve overflow paths without putting their full
-- text in a B-tree key; queries sort only the matching identity's overflow rows.
CREATE INDEX sql_subject_overflow ON sql_subject_lookup
    (tenant_id, source_id, kind, identity_hash) WHERE octet_length(path) > 2048;
