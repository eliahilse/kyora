import { log } from "../utils/logger";
import { check, printStatus, apply } from "../services/optimization";

interface OptimizeOptions {
  check: boolean;
  yes: boolean;
}

async function confirm(message: string): Promise<boolean> {
  process.stdout.write(`${message} [y/N] `);

  for await (const line of console) {
    const answer = line.trim().toLowerCase();
    return answer === "y" || answer === "yes";
  }
  return false;
}

export async function optimize(options: OptimizeOptions): Promise<void> {
  const result = await check();
  printStatus(result);

  if (!result.needsChanges) {
    log.success("All optimizations are already applied.");
    return;
  }

  if (options.check) {
    log.dim("  Run `kyora optimize` to apply changes.");
    return;
  }

  if (!options.yes) {
    const confirmed = await confirm("Apply these changes?");
    if (!confirmed) {
      log.warn("Aborted.");
      return;
    }
  }

  try {
    await apply(result);
    log.success("Optimizations applied.");
    log.dim("  Re-login for limits to take effect.");
  } catch (err) {
    log.error(`Failed to apply: ${err instanceof Error ? err.message : err}`);
    process.exit(1);
  }
}
