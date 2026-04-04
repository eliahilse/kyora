import { pgTable, serial, text, integer, real, jsonb, timestamp, index, vector } from "drizzle-orm/pg-core"

export const events = pgTable("events", {
  id: serial("id").primaryKey(),
  type: text("type").notNull(),
  timestamp: timestamp("timestamp").defaultNow().notNull(),
  data: jsonb("data").notNull(),
  sessionId: text("session_id"),
  traceId: text("trace_id"),
}, (table) => ([
  index("idx_events_type_ts").on(table.type, table.timestamp),
]))

export const stateSnapshots = pgTable("state_snapshots", {
  id: serial("id").primaryKey(),
  key: text("key").notNull(),
  value: jsonb("value"),
  diff: jsonb("diff"),
  timestamp: timestamp("timestamp").defaultNow().notNull(),
  source: text("source"),
  traceId: text("trace_id"),
  sessionId: text("session_id"),
}, (table) => ([
  index("idx_state_key_ts").on(table.key, table.timestamp),
  index("idx_state_trace").on(table.traceId),
]))

export const functionCalls = pgTable("function_calls", {
  id: serial("id").primaryKey(),
  name: text("name").notNull(),
  args: jsonb("args"),
  returnValue: jsonb("return_value"),
  error: text("error"),
  durationMs: real("duration_ms"),
  timestamp: timestamp("timestamp").defaultNow().notNull(),
  caller: text("caller"),
  traceId: text("trace_id"),
  sessionId: text("session_id"),
}, (table) => ([
  index("idx_fn_name_ts").on(table.name, table.timestamp),
  index("idx_fn_trace").on(table.traceId),
]))

// nora: doc indexing

export const docSources = pgTable("doc_sources", {
  id: serial("id").primaryKey(),
  type: text("type").notNull(),
  reference: text("reference").notNull(),
  name: text("name").notNull(),
  status: text("status").notNull().default("pending"),
  chunksCount: integer("chunks_count").default(0),
  error: text("error"),
  createdAt: timestamp("created_at").defaultNow().notNull(),
  updatedAt: timestamp("updated_at").defaultNow().notNull(),
}, (table) => ([
  index("idx_doc_sources_type").on(table.type),
  index("idx_doc_sources_status").on(table.status),
]))

export const docChunks = pgTable("doc_chunks", {
  id: serial("id").primaryKey(),
  sourceId: integer("source_id").notNull(),
  content: text("content").notNull(),
  embedding: vector("embedding", { dimensions: 384 }),
  metadata: jsonb("metadata"),
  chunkIndex: integer("chunk_index").notNull(),
  createdAt: timestamp("created_at").defaultNow().notNull(),
}, (table) => ([
  index("idx_doc_chunks_source").on(table.sourceId),
]))
