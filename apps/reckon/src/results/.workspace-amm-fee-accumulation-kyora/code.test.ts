import { test, expect } from "bun:test"
import { ConstantProductAMM } from "./code"

test("single swap produces correct output", () => {
  const amm = new ConstantProductAMM(1000, 1000)
  const { amountOut, fee } = amm.swapAForB(10)
  expect(fee).toBeCloseTo(0.03, 4)
  expect(amountOut).toBeGreaterThan(0)
  expect(amountOut).toBeLessThan(10)
})

test("swap and reverse returns approximately original amount", () => {
  const amm = new ConstantProductAMM(10000, 10000)
  const { amountOut: out1 } = amm.swapAForB(100)
  const { amountOut: out2 } = amm.swapBForA(out1)
  // after fees, should get back less than original
  expect(out2).toBeLessThan(100)
  // but not drastically less (within 1% for fees)
  expect(out2).toBeGreaterThan(99)
})

test("fee is correctly deducted from swap amount", () => {
  const amm = new ConstantProductAMM(10000, 10000, 0.01)
  const { amountOut } = amm.swapAForB(1000)

  // with 1% fee, effective input is 990
  // output should be: 10000 - (10000 * 10000) / (10000 + 990) = ~900.08
  const expectedOut = 10000 - (10000 * 10000) / (10000 + 990)
  expect(amountOut).toBeCloseTo(expectedOut, 2)
})

test("invariant stays stable after many swaps", () => {
  const initialK = 1_000_000
  const amm = new ConstantProductAMM(1000, 1000)
  const initialInvariant = amm.getInvariant()

  const swaps = Array.from({ length: 200 }, (_, i) => ({
    direction: (i % 2 === 0 ? "AtoB" : "BtoA") as "AtoB" | "BtoA",
    amount: 5 + Math.sin(i) * 3,
  }))

  const { amm: finalAmm } = ConstantProductAMM.simulateSwaps(1000, 1000, swaps)

  // invariant should grow slightly due to fees being retained
  // but should NOT grow by more than the total fees collected
  const totalFees = finalAmm.totalFees.a + finalAmm.totalFees.b
  const invariantGrowth = finalAmm.getInvariant() - initialK
  expect(invariantGrowth).toBeLessThan(totalFees * finalAmm.getSpotPrice() * 2)
})

test("output amount uses fee-adjusted input", () => {
  const amm1 = new ConstantProductAMM(10000, 10000, 0.003)
  const amm2 = new ConstantProductAMM(10000, 10000, 0)

  const { amountOut: withFee } = amm1.swapAForB(500)
  const { amountOut: noFee } = amm2.swapAForB(500)

  // with fee, you get less output
  expect(withFee).toBeLessThan(noFee)

  // the difference should be approximately proportional to the fee
  // fee removes 0.3% of input, so output should be ~0.3% less
  const diff = (noFee - withFee) / noFee
  expect(diff).toBeGreaterThan(0.001)
  expect(diff).toBeLessThan(0.01)
})

test("reserves after swap reflect fee-adjusted amounts", () => {
  const amm = new ConstantProductAMM(10000, 10000, 0.01)
  amm.swapAForB(1000)

  // reserve A should increase by amountIn minus fee (990), not full 1000
  expect(amm.reserveA).toBeCloseTo(10990, 0)
})

test("many small swaps don't compound error", () => {
  const amm = new ConstantProductAMM(100000, 100000, 0.003)
  const k0 = amm.getInvariant()

  for (let i = 0; i < 500; i++) {
    amm.swapAForB(10)
    amm.swapBForA(10)
  }

  // k should grow from fees, but growth should be bounded
  // with 0.3% fee on each swap, ~500 round trips of $10 each
  // total fee volume ~ 500 * 2 * 10 * 0.003 = 30
  // k growth should be roughly proportional, not exponential
  const kGrowthPct = (amm.getInvariant() - k0) / k0
  expect(kGrowthPct).toBeLessThan(0.01)
})
