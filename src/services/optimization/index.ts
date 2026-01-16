import { log } from "../../utils/logger";
import { checkSysctl, applySysctl, type SysctlParam } from "./sysctl";
import { checkLimits, applyLimits, type LimitParam } from "./limits";

export interface OptimizationResult {
  sysctl: SysctlParam[];
  limits: LimitParam[];
  needsChanges: boolean;
}

export async function check(): Promise<OptimizationResult> {
  const sysctl = await checkSysctl();
  const limits = await checkLimits();

  const needsChanges =
    sysctl.some((p) => !p.ok) || limits.some((p) => !p.ok);

  return { sysctl, limits, needsChanges };
}

export function printStatus(result: OptimizationResult): void {
  log.title("Solana Node Optimization Check");

  console.log("  sysctl:");
  for (const param of result.sysctl) {
    log.item(param.key, param.current, param.target, param.ok);
  }

  log.blank();
  console.log("  limits:");
  for (const param of result.limits) {
    const currentDisplay = param.current === Infinity ? "unlimited" : param.current;
    log.item(param.key, currentDisplay, param.target, param.ok);
  }

  log.blank();
}

export async function apply(result: OptimizationResult): Promise<void> {
  await applySysctl(result.sysctl);
  await applyLimits(result.limits);
}
