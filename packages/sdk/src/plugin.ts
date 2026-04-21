import { plugin } from "bun"

const WATCH_RE = /\/\/\s*@kyora\.watch(?:\s+(\w+))?/
const TRACE_RE = /\/\/\s*@kyora\.trace(?:\s+(\w+))?/

plugin({
  name: "kyora",
  setup(build) {
    build.onLoad({ filter: /\.(ts|tsx|js|jsx)$/ }, async (args) => {
      const passthrough = async () => ({ contents: await Bun.file(args.path).text(), loader: args.loader })
      if (args.path.includes("node_modules")) return passthrough()
      if (args.path.includes("@kyora")) return passthrough()

      const source = await Bun.file(args.path).text()
      if (!source.includes("@kyora.")) return { contents: source, loader: args.loader }

      const lines = source.split("\n")
      let needsWatch = false
      let needsTrace = false
      const insertions: Array<{ after: number; code: string }> = []
      const findDeclarationEnd = (start: number) => {
        let braceDepth = 0
        let sawBrace = false
        for (let j = start; j < lines.length; j++) {
          const text = lines[j]!
          for (const ch of text) {
            if (ch === "{") {
              braceDepth++
              sawBrace = true
            } else if (ch === "}") {
              braceDepth--
            }
          }
          if (sawBrace && braceDepth <= 0) return j
          if (!sawBrace && text.includes(";")) return j
        }
        return start
      }

      for (let i = 0; i < lines.length; i++) {
        const line = lines[i]!

        const watchMatch = line.match(WATCH_RE)
        if (watchMatch && i + 1 < lines.length) {
          const nextLine = lines[i + 1]!
          const varMatch = nextLine.match(/(?:const|let|var)\s+(\w+)/)
          const target = watchMatch[1] ?? varMatch?.[1]
          if (target && varMatch) {
            needsWatch = true
            insertions.push({
              after: i + 1,
              code: `${target} = __kyora_watch(${target}, "${target}", "${args.path}:${i + 2}");`,
            })
          }
        }

        const traceMatch = line.match(TRACE_RE)
        if (traceMatch && i + 1 < lines.length) {
          const nextLine = lines[i + 1]!
          // arrow function: const name = ...
          const arrowMatch = nextLine.match(/(?:const|let|var)\s+(\w+)\s*=/)
          // function declaration: function name or async function name
          const fnMatch = nextLine.match(/(?:async\s+)?function\s+(\w+)/)
          const target = traceMatch[1] ?? arrowMatch?.[1] ?? fnMatch?.[1]
          if (target) {
            needsTrace = true
            insertions.push({
              after: findDeclarationEnd(i + 1),
              code: `${target} = __kyora_trace(${target}, "${target}");`,
            })
          }
        }
      }

      if (!needsWatch && !needsTrace) return
      // insert in reverse so indices stay valid
      for (const ins of insertions.sort((a, b) => b.after - a.after)) {
        // for let/var declarations, insert reassignment on next line
        // for const, we need to change const to let
        const declLine = lines[ins.after]!
        if (declLine.match(/^\s*const\s/)) {
          lines[ins.after] = declLine.replace(/^(\s*)const\s/, "$1let ")
        }
        lines.splice(ins.after + 1, 0, ins.code)
      }

      let imports = ""
      if (needsWatch) imports += `import { watch as __kyora_watch } from "@kyora/sdk";\n`
      if (needsTrace) imports += `import { trace as __kyora_trace } from "@kyora/sdk";\n`

      return { contents: imports + lines.join("\n"), loader: args.loader }
    })
  },
})
