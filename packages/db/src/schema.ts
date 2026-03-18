import { pgTable, serial, text, integer, real, jsonb, timestamp, index } from "drizzle-orm/pg-core"

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
  value: jsonb("value").notNull(),
  diff: jsonb("diff"),
  timestamp: timestamp("timestamp").defaultNow().notNull(),
  source: text("source"),
}, (table) => ([
  index("idx_state_key_ts").on(table.key, table.timestamp),
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
}, (table) => ([
  index("idx_fn_name_ts").on(table.name, table.timestamp),
]))
