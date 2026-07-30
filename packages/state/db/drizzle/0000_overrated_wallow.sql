CREATE TABLE "doc_chunks" (
	"id" serial PRIMARY KEY NOT NULL,
	"source_id" integer NOT NULL,
	"content" text NOT NULL,
	"embedding" vector(384),
	"metadata" jsonb,
	"chunk_index" integer NOT NULL,
	"created_at" timestamp DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "doc_sources" (
	"id" serial PRIMARY KEY NOT NULL,
	"type" text NOT NULL,
	"reference" text NOT NULL,
	"name" text NOT NULL,
	"status" text DEFAULT 'pending' NOT NULL,
	"chunks_count" integer DEFAULT 0,
	"error" text,
	"created_at" timestamp DEFAULT now() NOT NULL,
	"updated_at" timestamp DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "events" (
	"id" serial PRIMARY KEY NOT NULL,
	"type" text NOT NULL,
	"timestamp" timestamp DEFAULT now() NOT NULL,
	"data" jsonb NOT NULL,
	"session_id" text,
	"trace_id" text
);
--> statement-breakpoint
CREATE TABLE "function_calls" (
	"id" serial PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"args" jsonb,
	"return_value" jsonb,
	"error" text,
	"duration_ms" real,
	"timestamp" timestamp DEFAULT now() NOT NULL,
	"caller" text
);
--> statement-breakpoint
CREATE TABLE "state_snapshots" (
	"id" serial PRIMARY KEY NOT NULL,
	"key" text NOT NULL,
	"value" jsonb NOT NULL,
	"diff" jsonb,
	"timestamp" timestamp DEFAULT now() NOT NULL,
	"source" text
);
--> statement-breakpoint
CREATE INDEX "idx_doc_chunks_source" ON "doc_chunks" USING btree ("source_id");--> statement-breakpoint
CREATE INDEX "idx_doc_sources_type" ON "doc_sources" USING btree ("type");--> statement-breakpoint
CREATE INDEX "idx_doc_sources_status" ON "doc_sources" USING btree ("status");--> statement-breakpoint
CREATE INDEX "idx_events_type_ts" ON "events" USING btree ("type","timestamp");--> statement-breakpoint
CREATE INDEX "idx_fn_name_ts" ON "function_calls" USING btree ("name","timestamp");--> statement-breakpoint
CREATE INDEX "idx_state_key_ts" ON "state_snapshots" USING btree ("key","timestamp");