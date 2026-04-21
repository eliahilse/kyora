export interface ExtractedSymbol {
  name: string
  qualified: string
  kind: string
  signature: string
}

const DTS_EXPORT_RE = /export\s+(?:declare\s+)?(?:default\s+)?(function|class|interface|type|const|let|var|enum|namespace)\s+(\w+)([^;\n{]*)/g
const MD_HEADING_FN_RE = /^#{1,4}\s+`?(\w+(?:\.\w+)*)\s*\(([^)]*)\)`?\s*$/gm
const MD_CODE_EXPORT_RE = /^(?:export\s+)?(?:declare\s+)?(?:default\s+)?(function|class|interface|type|const)\s+(\w+)([^;\n{]*)/gm

export function extractFromDts(content: string, packageName?: string): ExtractedSymbol[] {
  const symbols: ExtractedSymbol[] = []
  const seen = new Set<string>()

  for (const match of content.matchAll(DTS_EXPORT_RE)) {
    const kind = match[1]!
    const name = match[2]!
    const rest = match[3]?.trim() ?? ""
    const qualified = packageName ? `${packageName}.${name}` : name

    if (seen.has(qualified)) continue
    seen.add(qualified)

    const signature = `${kind} ${name}${rest}`.trim()
    symbols.push({ name, qualified, kind, signature })
  }

  return symbols
}

export function extractFromMarkdown(content: string): ExtractedSymbol[] {
  const symbols: ExtractedSymbol[] = []
  const seen = new Set<string>()

  for (const match of content.matchAll(MD_HEADING_FN_RE)) {
    const qualified = match[1]!
    const name = qualified.split(".").pop()!
    if (seen.has(qualified)) continue
    seen.add(qualified)
    symbols.push({ name, qualified, kind: "function", signature: `${qualified}(${match[2] ?? ""})` })
  }

  const codeBlocks = content.matchAll(/```(?:ts|typescript|js|javascript)?\n([\s\S]*?)```/g)
  for (const block of codeBlocks) {
    const code = block[1]!
    for (const match of code.matchAll(MD_CODE_EXPORT_RE)) {
      const kind = match[1]!
      const name = match[2]!
      const rest = match[3]?.trim() ?? ""
      if (seen.has(name)) continue
      seen.add(name)
      symbols.push({ name, qualified: name, kind, signature: `${kind} ${name}${rest}`.trim() })
    }
  }

  return symbols
}

export function extractSymbols(content: string, docType?: string, packageName?: string): ExtractedSymbol[] {
  if (docType === "types") return extractFromDts(content, packageName)
  return extractFromMarkdown(content)
}
