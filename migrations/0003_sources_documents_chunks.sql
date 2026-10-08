CREATE TABLE sources (
  id uuid PRIMARY KEY,
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  kind text NOT NULL CHECK (kind IN ('repo','traces','documents')),
  name text NOT NULL,
  web_url_template text,
  indexed_commit text,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, kind, name)
);
CREATE TABLE documents (
  id uuid PRIMARY KEY,
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  source_id uuid NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  external_id text NOT NULL,
  kind text NOT NULL CHECK (kind IN ('code','doc','session','note')),
  title text NOT NULL,
  uri text,
  metadata jsonb NOT NULL DEFAULT '{}',
  content_hash text NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (source_id, external_id)
);
CREATE INDEX documents_tenant_kind ON documents (tenant_id, kind);
CREATE TABLE chunks (
  id bigserial PRIMARY KEY,
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  document_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  ord int NOT NULL,
  text text NOT NULL,
  start_line int,
  end_line int,
  symbols text[] NOT NULL DEFAULT '{}',
  tsv tsvector NOT NULL,
  embedding vector(384) NOT NULL
);
CREATE INDEX chunks_document ON chunks (document_id);
CREATE INDEX chunks_tenant ON chunks (tenant_id);
CREATE INDEX chunks_tsv ON chunks USING gin (tsv);
CREATE INDEX chunks_embedding ON chunks USING hnsw (embedding vector_cosine_ops);
