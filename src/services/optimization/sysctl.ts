import { exec, execSudo, readFile, writeFileWithSudo } from "../../utils/exec";

const SYSCTL_CONF_PATH = "/etc/sysctl.d/99-solana-kyora.conf";

export interface SysctlParam {
  key: string;
  target: number;
  current: number;
  ok: boolean;
}

export const SYSCTL_TARGETS: Record<string, number> = {
  "net.core.rmem_max": 134217728,
  "net.core.wmem_max": 134217728,
  "vm.max_map_count": 1000000,
  "fs.nr_open": 1000000,
};

async function getCurrentValue(key: string): Promise<number> {
  try {
    const result = await exec(["sysctl", "-n", key]);
    return parseInt(result, 10) || 0;
  } catch {
    return 0;
  }
}

export async function checkSysctl(): Promise<SysctlParam[]> {
  const results: SysctlParam[] = [];

  for (const [key, target] of Object.entries(SYSCTL_TARGETS)) {
    const current = await getCurrentValue(key);
    results.push({
      key,
      target,
      current,
      ok: current >= target,
    });
  }

  return results;
}

export async function applySysctl(params: SysctlParam[]): Promise<void> {
  const needsUpdate = params.filter((p) => !p.ok);
  if (needsUpdate.length === 0) return;

  const existingContent = (await readFile(SYSCTL_CONF_PATH)) || "";
  const existingKeys = new Set(
    existingContent
      .split("\n")
      .filter((line) => line.trim() && !line.startsWith("#"))
      .map((line) => line.split("=")[0]?.trim())
  );

  const newLines: string[] = [];
  for (const param of needsUpdate) {
    if (!existingKeys.has(param.key)) {
      newLines.push(`${param.key} = ${param.target}`);
    }
  }

  if (newLines.length > 0) {
    const header = existingContent ? "" : "# solana node optimizations (kyora)\n";
    const content = existingContent
      ? existingContent.trimEnd() + "\n" + newLines.join("\n") + "\n"
      : header + newLines.join("\n") + "\n";

    await writeFileWithSudo(SYSCTL_CONF_PATH, content);
  }

  await execSudo(["sysctl", "-p", SYSCTL_CONF_PATH]);
}
