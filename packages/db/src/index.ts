import { PGlite } from "@electric-sql/pglite"
import { vector } from "@electric-sql/pglite/vector"
import { drizzle } from "drizzle-orm/pglite"
import * as schema from "./schema"

const SCHEMA_SQL = `
  CREATE EXTENSION IF NOT EXISTS vector;

  CREATE TABLE IF NOT EXISTS events (
    id SERIAL PRIMARY KEY, type TEXT NOT NULL,
    timestamp TIMESTAMP DEFAULT NOW() NOT NULL, data JSONB NOT NULL,
    session_id TEXT, trace_id TEXT
  );
  CREATE TABLE IF NOT EXISTS state_snapshots (
    id SERIAL PRIMARY KEY, key TEXT NOT NULL,
    value JSONB, diff JSONB,
    timestamp TIMESTAMP DEFAULT NOW() NOT NULL, source TEXT,
    trace_id TEXT, session_id TEXT
  );
  CREATE TABLE IF NOT EXISTS function_calls (
    id SERIAL PRIMARY KEY, name TEXT NOT NULL,
    args JSONB, return_value JSONB, error TEXT, duration_ms REAL,
    timestamp TIMESTAMP DEFAULT NOW() NOT NULL, caller TEXT,
    trace_id TEXT, session_id TEXT
  );
  CREATE TABLE IF NOT EXISTS doc_sources (
    id SERIAL PRIMARY KEY, type TEXT NOT NULL,
    reference TEXT NOT NULL, name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending', chunks_count INTEGER DEFAULT 0,
    error TEXT, created_at TIMESTAMP DEFAULT NOW() NOT NULL,
    updated_at TIMESTAMP DEFAULT NOW() NOT NULL
  );
  CREATE TABLE IF NOT EXISTS doc_chunks (
    id SERIAL PRIMARY KEY, source_id INTEGER NOT NULL,
    content TEXT NOT NULL, embedding VECTOR(384),
    metadata JSONB, chunk_index INTEGER NOT NULL,
    tsv TSVECTOR,
    created_at TIMESTAMP DEFAULT NOW() NOT NULL
  );
  CREATE TABLE IF NOT EXISTS api_symbols (
    id SERIAL PRIMARY KEY, source_id INTEGER NOT NULL,
    name TEXT NOT NULL, qualified TEXT NOT NULL,
    kind TEXT NOT NULL, signature TEXT,
    doc_chunk_id INTEGER,
    created_at TIMESTAMP DEFAULT NOW() NOT NULL
  );

  ALTER TABLE doc_chunks ADD COLUMN IF NOT EXISTS tsv TSVECTOR;
  ALTER TABLE state_snapshots ADD COLUMN IF NOT EXISTS trace_id TEXT;
  ALTER TABLE state_snapshots ADD COLUMN IF NOT EXISTS session_id TEXT;
  ALTER TABLE function_calls ADD COLUMN IF NOT EXISTS trace_id TEXT;
  ALTER TABLE function_calls ADD COLUMN IF NOT EXISTS session_id TEXT;

  CREATE INDEX IF NOT EXISTS idx_events_type_ts ON events (type, timestamp);
  CREATE INDEX IF NOT EXISTS idx_state_key_ts ON state_snapshots (key, timestamp);
  CREATE INDEX IF NOT EXISTS idx_state_trace ON state_snapshots (trace_id);
  CREATE INDEX IF NOT EXISTS idx_fn_name_ts ON function_calls (name, timestamp);
  CREATE INDEX IF NOT EXISTS idx_fn_trace ON function_calls (trace_id);
  CREATE INDEX IF NOT EXISTS idx_doc_sources_type ON doc_sources (type);
  CREATE INDEX IF NOT EXISTS idx_doc_sources_status ON doc_sources (status);
  CREATE INDEX IF NOT EXISTS idx_doc_chunks_source ON doc_chunks (source_id);
  CREATE INDEX IF NOT EXISTS idx_doc_chunks_tsv ON doc_chunks USING GIN (tsv);
  CREATE INDEX IF NOT EXISTS idx_api_symbols_name ON api_symbols (name);
  CREATE INDEX IF NOT EXISTS idx_api_symbols_qualified ON api_symbols (qualified);
  CREATE INDEX IF NOT EXISTS idx_api_symbols_source ON api_symbols (source_id);
`

async function initDb(client: PGlite) {
  await client.exec(SCHEMA_SQL)
  return drizzle(client, { schema })
}

export async function createDb(dataDir?: string) {
  const client = new PGlite(dataDir ?? ".kyora/data", {
    extensions: { vector },
  })
  return initDb(client)
}

export async function createMemoryDb() {
  const client = new PGlite({ extensions: { vector } })
  return initDb(client)
}

export type KyoraDb = Awaited<ReturnType<typeof createDb>>

export { schema }
export * from "./schema"
