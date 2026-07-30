import { describe, expect, test } from "bun:test"
import { extractRunCommands, isCiCovered } from "./ci"

const WORKFLOW = `name: CI
on: push
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: bun install --frozen-lockfile
      - run: bun run build
      - name: multi
        run: |
          # comment
          bun run check-types
          bun run test
      - run: echo "done"
`

describe("extractRunCommands", () => {
  test("collects single-line and block-scalar run commands", () => {
    expect(extractRunCommands(WORKFLOW)).toEqual([
      "bun install --frozen-lockfile",
      "bun run build",
      "bun run check-types",
      "bun run test",
      'echo "done"',
    ])
  })
})

describe("isCiCovered", () => {
  test("flags suites, builds, and checkers", () => {
    expect(isCiCovered("bun run test")).toBe(true)
    expect(isCiCovered("bun run build")).toBe(true)
    expect(isCiCovered("bun run check-types")).toBe(true)
    expect(isCiCovered("npx vitest run")).toBe(true)
    expect(isCiCovered("tsc --noEmit")).toBe(true)
  })

  test("ignores installs and misc commands", () => {
    expect(isCiCovered("bun install --frozen-lockfile")).toBe(false)
    expect(isCiCovered('echo "done"')).toBe(false)
    expect(isCiCovered("git fetch origin main")).toBe(false)
  })
})
