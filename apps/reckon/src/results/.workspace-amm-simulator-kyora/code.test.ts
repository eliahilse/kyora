import { test, expect } from "bun:test"
import {
  createDynamicStrategy, createStaticStrategy, benchmark,
  executeBuyXWithY, arbitrage,
  type TradeInfo, type AMM,
} from "./code"

function makeAmm(x: number, y: number, bidFee: number, askFee: number): AMM {
  return { x, y, bidFee, askFee, strategy: null, edge: 0 }
}

test("dynamic strategy fees vary with observed volatility", () => {
  const strategy = createDynamicStrategy({
    baseBps: 12, alpha: 0.4, volScale: 5.0, minBps: 8, maxBps: 500,
  })
  strategy.afterInitialize(100, 10000)

  const fees: number[] = []
  let reserveX = 100
  let reserveY = 10000
  for (let i = 0; i < 500; i++) {
    const k = reserveX * reserveY
    reserveX = 100 * (1 + Math.sin(i / 8) * 0.15)
    reserveY = k / reserveX
    const trade: TradeInfo = {
      isBuy: i % 2 === 0,
      amountX: 1,
      amountY: 100,
      timestamp: i,
      reserveX,
      reserveY,
    }
    const { bidFee } = strategy.afterSwap(trade)
    fees.push(bidFee)
  }

  const uniqueFees = new Set(fees.slice(5).map((f) => Math.round(f * 1e7)))
  expect(uniqueFees.size).toBeGreaterThan(20)
})

test("fees rise when volatility rises", () => {
  const strategy = createDynamicStrategy({
    baseBps: 12, alpha: 0.4, volScale: 5.0, minBps: 8, maxBps: 500,
  })
  strategy.afterInitialize(100, 10000)

  const quietFees: number[] = []
  for (let i = 0; i < 100; i++) {
    const { bidFee } = strategy.afterSwap({
      isBuy: true, amountX: 1, amountY: 100, timestamp: i,
      reserveX: 100, reserveY: 10000,
    })
    quietFees.push(bidFee)
  }

  const volatileFees: number[] = []
  for (let i = 0; i < 100; i++) {
    const reserveX = 100 * (1 + (Math.random() - 0.5) * 0.3)
    const reserveY = 10000 * (1 + (Math.random() - 0.5) * 0.3)
    const { bidFee } = strategy.afterSwap({
      isBuy: true, amountX: 1, amountY: 100, timestamp: 100 + i,
      reserveX, reserveY,
    })
    volatileFees.push(bidFee)
  }

  const quietAvg = quietFees.reduce((a, b) => a + b, 0) / quietFees.length
  const volatileAvg = volatileFees.reduce((a, b) => a + b, 0) / volatileFees.length
  expect(volatileAvg).toBeGreaterThan(quietAvg * 1.5)
})

test("dynamic strategy outperforms static baseline on average", () => {
  const staticStrat = createStaticStrategy(30)
  const dynamicStrat = createDynamicStrategy({
    baseBps: 12, alpha: 0.4, volScale: 1.3, minBps: 8, maxBps: 300,
  })

  const staticResult = benchmark(staticStrat, 80, true)
  const dynamicResult = benchmark(dynamicStrat, 80, true)

  expect(dynamicResult.avg).toBeGreaterThan(staticResult.avg)
}, 120_000)

test("executeBuyXWithY only credits fee-adjusted amount to reserves", () => {
  const amm = makeAmm(100, 10000, 0.01, 0.01)
  const amountY = 100
  const trade = executeBuyXWithY(amm, amountY, 0)
  expect(trade).not.toBeNull()

  // fee is 1% of amountY = 1 Y. So netY = 99.
  // reserve y should grow by 99, not by 100.
  const expectedYGrowth = amountY * (1 - 0.01)
  const actualYGrowth = amm.y - 10000
  expect(Math.abs(actualYGrowth - expectedYGrowth)).toBeLessThan(0.01)
})

test("executeBuyXWithY preserves k to within fee tolerance over many trades", () => {
  const amm = makeAmm(100, 10000, 0.001, 0.001)
  const k0 = amm.x * amm.y

  for (let i = 0; i < 50; i++) {
    executeBuyXWithY(amm, 10, i)
  }

  const k1 = amm.x * amm.y
  // with correct fee handling, k grows slightly from the 0.1% fee compounding on retained reserves
  // with leak (full amountY credited), k grows by fee*y per trade → wildly bigger
  const growthRatio = k1 / k0
  expect(growthRatio).toBeLessThan(1.1)
})

test("arbitrage on overpriced AMM does not overshoot fair price", () => {
  // AMM overprices X: spot is above fair → arb sells X to AMM (AMM buys X).
  // a correct arb stops at or just above fair (fee creates a stop band).
  // a buggy formula that uses the wrong gamma placement over-corrects and
  // pushes spot BELOW fair, which is economically impossible for a rational arb.
  const amm = makeAmm(100, 11000, 0.003, 0.003) // spot=110, fair=100
  const fairPrice = 100

  arbitrage(amm, fairPrice, 0)

  const spotAfter = amm.y / amm.x
  // spot should land at or slightly above fair (never below)
  expect(spotAfter).toBeGreaterThanOrEqual(fairPrice * 0.999)
  // and should have moved substantially toward fair
  expect(spotAfter).toBeLessThan(105)
})

test("arbitrage on underpriced AMM does not overshoot fair price", () => {
  // mirror case: spot below fair → arb buys X from AMM.
  // correct arb stops at or just below fair.
  const amm = makeAmm(100, 9000, 0.003, 0.003) // spot=90, fair=100
  const fairPrice = 100

  arbitrage(amm, fairPrice, 0)

  const spotAfter = amm.y / amm.x
  expect(spotAfter).toBeLessThanOrEqual(fairPrice * 1.001)
  expect(spotAfter).toBeGreaterThan(95)
})
