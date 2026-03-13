const colors = {
  reset: "\x1b[0m",
  bold: "\x1b[1m",
  dim: "\x1b[2m",
  red: "\x1b[31m",
  green: "\x1b[32m",
  yellow: "\x1b[33m",
  blue: "\x1b[34m",
  cyan: "\x1b[36m",
  white: "\x1b[37m",
} as const;

export const log = {
  info: (msg: string) => console.log(`${colors.blue}ℹ${colors.reset} ${msg}`),
  success: (msg: string) => console.log(`${colors.green}✓${colors.reset} ${msg}`),
  warn: (msg: string) => console.log(`${colors.yellow}⚠${colors.reset} ${msg}`),
  error: (msg: string) => console.log(`${colors.red}✗${colors.reset} ${msg}`),

  title: (msg: string) => console.log(`\n${colors.bold}${colors.cyan}  ${msg}${colors.reset}\n`),

  dim: (msg: string) => console.log(`${colors.dim}${msg}${colors.reset}`),

  item: (key: string, current: string | number, target: string | number, ok: boolean) => {
    const status = ok
      ? `${colors.green}✓${colors.reset}`
      : `${colors.red}✗${colors.reset}`;
    const arrow = ok ? "" : ` → ${target}`;
    const currentStr = ok ? `${current}` : `${colors.dim}${current}${colors.reset}${arrow}`;
    console.log(`    ${key.padEnd(22)} ${currentStr.padEnd(30)} ${status}`);
  },

  blank: () => console.log(),
};
