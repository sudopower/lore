CREATE TABLE spans (
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  trace_id text NOT NULL,
  span_id text NOT NULL,
  parent_span_id text,
  session_id text NOT NULL,
  name text NOT NULL,
  kind text NOT NULL,
  start_time timestamptz NOT NULL,
  end_time timestamptz NOT NULL,
  status_code text NOT NULL,           -- 'UNSET' | 'OK' | 'ERROR'
  status_message text,
  resource jsonb NOT NULL,
  attributes jsonb NOT NULL,
  events jsonb NOT NULL DEFAULT '[]',
  received_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, trace_id, span_id)
);
CREATE INDEX spans_session ON spans (tenant_id, session_id, start_time);
CREATE TABLE session_queue (
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  session_id text NOT NULL,
  last_span_at timestamptz NOT NULL,
  PRIMARY KEY (tenant_id, session_id)
);
