import { exec, readFile, writeFileWithSudo } from "../../utils/exec";

const LIMITS_CONF_PATH = "/etc/security/limits.d/99-solana-kyora.conf";

export interface LimitParam {
  key: string;
  target: number;
  current: number;
  ok: boolean;
}

export const LIMITS_TARGETS: Record<string, number> = {
  nofile: 1000000,
  memlock: 2000000,
};

async function getCurrentLimit(key: string): Promise<number> {
  try {
    const flag = key === "nofile" ? "-n" : "-l";
    const result = await exec(["sh", "-c", `ulimit ${flag}`]);
    if (result === "unlimited") return Infinity;
    return parseInt(result, 10) || 0;
  } catch {
    return 0;
  }
}

export async function checkLimits(): Promise<LimitParam[]> {
  const results: LimitParam[] = [];

  for (const [key, target] of Object.entries(LIMITS_TARGETS)) {
    const current = await getCurrentLimit(key);
    results.push({
      key,
      target,
      current,
      ok: current >= target,
    });
  }

  return results;
}

export async function applyLimits(params: LimitParam[]): Promise<void> {
  const needsUpdate = params.filter((p) => !p.ok);
  if (needsUpdate.length === 0) return;

  const existingContent = (await readFile(LIMITS_CONF_PATH)) || "";
  const existingKeys = new Set(
    existingContent
      .split("\n")
      .filter((line) => line.trim() && !line.startsWith("#"))
      .map((line) => {
        const parts = line.split(/\s+/);
        return parts[2];
      })
  );

  const newLines: string[] = [];
  for (const param of needsUpdate) {
    if (!existingKeys.has(param.key)) {
      newLines.push(`* - ${param.key} ${param.target}`);
    }
  }

  if (newLines.length > 0) {
    const header = existingContent ? "" : "# solana node limits (kyora)\n";
    const content = existingContent
      ? existingContent.trimEnd() + "\n" + newLines.join("\n") + "\n"
      : header + newLines.join("\n") + "\n";

    await writeFileWithSudo(LIMITS_CONF_PATH, content);
  }
}
