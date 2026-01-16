#!/usr/bin/env bun
import { optimize } from "./commands/optimize";

const args = process.argv.slice(2);
const command = args[0];

function printHelp(): void {
  console.log(`
kyora - solana validator tooling

Usage:
  kyora <command> [options]

Commands:
  optimize          Check and apply system optimizations

Options:
  --check           Dry-run only, show what needs changing
  --yes, -y         Skip confirmation, apply directly
  --help, -h        Show this help message

Examples:
  kyora optimize              Check + apply (with confirmation)
  kyora optimize --check      Show what needs changing
  kyora optimize --yes        Apply without confirmation
`);
}

async function main(): Promise<void> {
  if (!command || command === "--help" || command === "-h") {
    printHelp();
    return;
  }

  if (command === "optimize") {
    const checkOnly = args.includes("--check");
    const yes = args.includes("--yes") || args.includes("-y");
    await optimize({ check: checkOnly, yes });
    return;
  }

  console.log(`Unknown command: ${command}`);
  printHelp();
  process.exit(1);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
