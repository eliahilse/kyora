export type DirectiveType = "watch" | "trace" | "track"

export interface Directive {
  type: DirectiveType
  target: string
  file: string
  line: number
}

const DIRECTIVE_REGEX = /\/\/\s*@kyora\.(watch|trace|track)(?:\s+(.+))?/

export function scanSource(source: string, file: string): Directive[] {
  const directives: Directive[] = []
  const lines = source.split("\n")

  for (let i = 0; i < lines.length; i++) {
    const match = lines[i]!.match(DIRECTIVE_REGEX)
    if (!match) continue

    const type = match[1] as DirectiveType
    let target = match[2]?.trim() ?? ""

    // for @kyora.watch and @kyora.trace without explicit target, infer from next line
    if (!target && i + 1 < lines.length) {
      target = inferTarget(lines[i + 1]!, type)
    }

    if (target) {
      directives.push({ type, target, file, line: i + 1 })
    }
  }

  return directives
}

function inferTarget(nextLine: string, type: DirectiveType): string {
  const trimmed = nextLine.trim()

  if (type === "trace") {
    // match function declarations: function name, async function name, const name = (...) =>
    const fnMatch = trimmed.match(
      /(?:async\s+)?function\s+(\w+)|(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s+)?(?:\(|function)/,
    )
    return fnMatch?.[1] ?? fnMatch?.[2] ?? ""
  }

  // for watch/track, match variable declarations
  const varMatch = trimmed.match(/(?:const|let|var)\s+(\w+)/)
  return varMatch?.[1] ?? ""
}

export async function scanFile(filePath: string): Promise<Directive[]> {
  const file = Bun.file(filePath)
  const source = await file.text()
  return scanSource(source, filePath)
}

export async function scanFiles(filePaths: string[]): Promise<Directive[]> {
  const results = await Promise.all(filePaths.map(scanFile))
  return results.flat()
}
