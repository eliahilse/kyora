import { describe, expect, test } from "bun:test"
import { parseCommentableLines } from "./diff"

const DIFF = `diff --git a/src/a.ts b/src/a.ts
index 111..222 100644
--- a/src/a.ts
+++ b/src/a.ts
@@ -8,4 +8,5 @@ function existing() {
 context line
-removed line
+added line
+another added line
 trailing context
diff --git a/src/deleted.ts b/src/deleted.ts
deleted file mode 100644
--- a/src/deleted.ts
+++ /dev/null
@@ -1,2 +0,0 @@
-gone
-gone too
diff --git a/src/b.ts b/src/b.ts
--- a/src/b.ts
+++ b/src/b.ts
@@ -1,2 +1,3 @@
 first
+inserted
 last
`

describe("parseCommentableLines", () => {
  test("tracks new-side line numbers through hunks", () => {
    const lines = parseCommentableLines(DIFF)
    // context @8, added @9, added @10, context @11 — removed line has no new-side number
    expect([...lines.get("src/a.ts")!].sort((a, b) => a - b)).toEqual([8, 9, 10, 11])
    // exact set: no phantom anchor past the last line (trailing "" from split)
    expect([...lines.get("src/b.ts")!].sort((a, b) => a - b)).toEqual([1, 2, 3])
  })

  test("ignores deleted files", () => {
    const lines = parseCommentableLines(DIFF)
    expect(lines.has("src/deleted.ts")).toBe(false)
  })
})
