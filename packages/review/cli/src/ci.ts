const COVERED =
  /(?:^|[\s/])(test|tests|build|lint|typecheck|check-types|types:check|tsc|vitest|jest|pytest|eslint|prettier)(?::|\s|$)/

export function isCiCovered(command: string): boolean {
  return COVERED.test(command)
}

export function extractRunCommands(workflowYaml: string): string[] {
  const commands: string[] = []
  const lines = workflowYaml.split("\n")
  for (let i = 0; i < lines.length; i++) {
    const match = /^(\s*)(?:-\s+)?run:\s*(.*)$/.exec(lines[i]!)
    if (!match) continue
    const indent = match[1]!.length
    const value = match[2]!.trim()
    if (value && value !== "|" && value !== ">") {
      commands.push(value)
      continue
    }
    for (let j = i + 1; j < lines.length; j++) {
      const line = lines[j]!
      if (line.trim() === "") continue
      const lineIndent = line.length - line.trimStart().length
      if (lineIndent <= indent) break
      const command = line.trim()
      if (!command.startsWith("#")) commands.push(command)
    }
  }
  return commands
}

/** Commands the repo's CI already runs that reviewers must not duplicate. */
export async function ciCoveredCommands(root: string): Promise<string[]> {
  const covered = new Set<string>()
  const glob = new Bun.Glob(".github/workflows/*.{yml,yaml}")
  try {
    for await (const file of glob.scan({ cwd: root, dot: true })) {
      const text = await Bun.file(`${root}/${file}`).text()
      for (const command of extractRunCommands(text)) {
        if (isCiCovered(command)) covered.add(command)
      }
    }
  } catch {
    return []
  }
  return [...covered].sort()
}
