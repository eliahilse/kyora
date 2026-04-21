import { tasks } from "./tasks"

let allPassed = true

for (const task of tasks) {
  console.log(`\n--- ${task.name} ---`)

  const tmpDir = `${import.meta.dir}/results/.validate-${task.name}`
  await Bun.$`rm -rf ${tmpDir}`.quiet()
  await Bun.$`mkdir -p ${tmpDir}`.quiet()

  // copy non-solution, non-test files
  const files = new Bun.Glob("*").scanSync(task.dir)
  for (const file of files) {
    if (file === "solution.ts" || file === "tests.ts") continue
    await Bun.$`cp ${task.dir}/${file} ${tmpDir}/${file}`.quiet()
  }

  // solution replaces code.ts, tests.ts becomes code.test.ts
  await Bun.$`cp ${task.dir}/solution.ts ${tmpDir}/code.ts`.quiet()
  await Bun.$`cp ${task.dir}/tests.ts ${tmpDir}/code.test.ts`.quiet()

  const result = await Bun.$`bun test code.test.ts 2>&1`.cwd(tmpDir).quiet().nothrow()
  const output = result.text()
  const hasFailure = output.includes("fail") && !output.includes("0 fail")

  if (hasFailure) {
    console.log(`  FAIL - solution doesn't pass all tests`)
    console.log(output)
    allPassed = false
  } else {
    const passMatch = output.match(/(\d+) pass/)
    console.log(`  OK - ${passMatch?.[1] ?? "?"} tests passed`)
  }

  await Bun.$`rm -rf ${tmpDir}`.quiet()
}

console.log(allPassed ? "\nall solutions valid" : "\nsome solutions failed")
process.exit(allPassed ? 0 : 1)
