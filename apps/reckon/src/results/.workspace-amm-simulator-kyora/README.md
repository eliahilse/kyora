The AMM simulator in `code.ts` is broken in several ways:

1. The tuned dynamic fee strategy underperforms the static 30bps baseline, when it should beat it.
2. Edge accounting is off — running two identical 30bps strategies against each other produces non-zero average edge divergence.
3. Reserve invariants drift unexpectedly after certain trade types.

There are MULTIPLE bugs. Figure out what's happening at runtime and fix them all. Running `bun code.ts` produces benchmark output you can use to observe the symptoms.
