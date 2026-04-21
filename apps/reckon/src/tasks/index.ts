import type { Task } from "../types"

const base = import.meta.dir

export const tasks: Task[] = [
  {
    name: "amm-simulator",
    dir: `${base}/amm-simulator`,
    description: "AMM dynamic fee strategy underperforming static baseline — subtle runtime bug",
  },
]
