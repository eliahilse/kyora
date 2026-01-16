import { $ } from "bun";

export async function exec(cmd: string[]): Promise<string> {
  const result = await $`${cmd}`.quiet().text();
  return result.trim();
}

export async function execSudo(cmd: string[]): Promise<void> {
  await $`sudo ${cmd}`.quiet();
}

export async function readFile(path: string): Promise<string | null> {
  try {
    const file = Bun.file(path);
    return await file.text();
  } catch {
    return null;
  }
}

export async function writeFileWithSudo(path: string, content: string): Promise<void> {
  await $`echo ${content} | sudo tee ${path} > /dev/null`.quiet();
}
