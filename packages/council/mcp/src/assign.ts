export interface DelegatedTask {
  task: string
  engine?: string
  context?: string
  model?: string
  effort?: string
}

export interface Assignment {
  task: DelegatedTask
  engine: string
}

/**
 * Distribute independent tasks over the engines that currently have quota:
 * pinned tasks first, then one family each until families run out, then reuse.
 * A task pinned to an unavailable family is reported, never silently rerouted.
 */
export function assignTasks(
  tasks: DelegatedTask[],
  pool: string[],
): { assignments: Assignment[]; unassigned: string[] } {
  const assignments: Assignment[] = []
  const unassigned: string[] = []
  if (pool.length === 0) return { assignments, unassigned: tasks.map((task) => task.task) }

  const taken = new Set<string>()
  for (const task of tasks) {
    if (!task.engine) continue
    if (pool.includes(task.engine)) {
      assignments.push({ task, engine: task.engine })
      taken.add(task.engine)
    } else {
      unassigned.push(task.task)
    }
  }

  let cursor = 0
  for (const task of tasks) {
    if (task.engine) continue
    const fresh = pool.filter((engine) => !taken.has(engine))
    const engine = fresh.length > 0 ? fresh[0]! : pool[cursor % pool.length]!
    if (fresh.length > 0) taken.add(engine)
    assignments.push({ task, engine })
    cursor++
  }
  return { assignments, unassigned }
}
