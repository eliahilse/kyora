function randn(): number {
  const u1 = Math.random();
  const u2 = Math.random();
  return Math.sqrt(-2 * Math.log(u1)) * Math.cos(2 * Math.PI * u2);
}

function poisson(lambda: number): number {
  const L = Math.exp(-lambda);
  let k = 0;
  let p = 1;
  do {
    k++;
    p *= Math.random();
  } while (p > L);
  return k - 1;
}

function lognormal(mean: number, sigma: number): number {
  const mu = Math.log(mean) - (sigma * sigma) / 2;
  return Math.exp(mu + sigma * randn());
}

function uniform(a: number, b: number): number {
  return a + Math.random() * (b - a);
}

export interface TradeInfo {
  isBuy: boolean;
  amountX: number;
  amountY: number;
  timestamp: number;
  reserveX: number;
  reserveY: number;
}

export interface Strategy {
  name: string;
  afterInitialize(initialX: number, initialY: number): { bidFee: number; askFee: number };
  afterSwap(trade: TradeInfo): { bidFee: number; askFee: number };
}

export interface AMM {
  x: number;
  y: number;
  bidFee: number;
  askFee: number;
  strategy: Strategy | null;
  edge: number;
}

const INITIAL_X = 100.0;
const INITIAL_Y = 10000.0;
const INITIAL_PRICE = 100.0;
const STEPS = 10000;
const GBM_SIGMA_MIN = 0.000882;
const GBM_SIGMA_MAX = 0.001008;
const RETAIL_LAMBDA_MIN = 0.6;
const RETAIL_LAMBDA_MAX = 1.0;
const RETAIL_MEAN_MIN = 19.0;
const RETAIL_MEAN_MAX = 21.0;
const RETAIL_SIZE_SIGMA = 1.2;
const NUM_SIMS = 500;

function clampFee(fee: number): number {
  return Math.max(0, Math.min(0.1, fee));
}

function bpsToFee(bps: number): number {
  return bps / 10000;
}

function spotPrice(amm: AMM): number {
  return amm.y / amm.x;
}

function quoteBuyX(amm: AMM, amountX: number): { yOut: number; feeAmount: number } {
  if (amountX <= 0) return { yOut: 0, feeAmount: 0 };
  const fee = amm.bidFee;
  const gamma = Math.max(0, 1 - fee);
  if (gamma <= 0) return { yOut: 0, feeAmount: 0 };
  const netX = amountX * gamma;
  const k = amm.x * amm.y;
  const newRx = amm.x + netX;
  const newRy = k / newRx;
  const yOut = amm.y - newRy;
  if (yOut <= 0) return { yOut: 0, feeAmount: 0 };
  return { yOut, feeAmount: amountX * fee };
}

function quoteXForY(amm: AMM, amountY: number): { xOut: number; totalY: number; feeAmount: number } {
  if (amountY <= 0) return { xOut: 0, totalY: 0, feeAmount: 0 };
  const fee = amm.askFee;
  const gamma = Math.max(0, 1 - fee);
  if (gamma <= 0) return { xOut: 0, totalY: 0, feeAmount: 0 };
  const netY = amountY * gamma;
  const k = amm.x * amm.y;
  const newRy = amm.y + netY;
  const newRx = k / newRy;
  const xOut = amm.x - newRx;
  if (xOut <= 0) return { xOut: 0, totalY: 0, feeAmount: 0 };
  return { xOut, totalY: amountY, feeAmount: amountY * fee };
}

function quoteSellX(amm: AMM, amountX: number): { totalY: number; feeAmount: number } {
  if (amountX <= 0 || amountX >= amm.x) return { totalY: 0, feeAmount: 0 };
  const k = amm.x * amm.y;
  const fee = amm.askFee;
  const gamma = Math.max(0, 1 - fee);
  if (gamma <= 0) return { totalY: 0, feeAmount: 0 };
  const newRx = amm.x - amountX;
  const newRy = k / newRx;
  const netY = newRy - amm.y;
  if (netY <= 0) return { totalY: 0, feeAmount: 0 };
  const totalY = netY / gamma;
  return { totalY, feeAmount: totalY - netY };
}

function executeBuyX(amm: AMM, amountX: number, timestamp: number): TradeInfo | null {
  const { yOut, feeAmount } = quoteBuyX(amm, amountX);
  if (yOut <= 0) return null;
  const netX = amountX - feeAmount;
  amm.x += netX;
  amm.y -= yOut;
  const trade: TradeInfo = {
    isBuy: true,
    amountX,
    amountY: yOut,
    timestamp,
    reserveX: amm.x,
    reserveY: amm.y,
  };
  updateFees(amm, trade);
  return trade;
}

export function executeBuyXWithY(amm: AMM, amountY: number, timestamp: number): TradeInfo | null {
  const { xOut, feeAmount } = quoteXForY(amm, amountY);
  if (xOut <= 0) return null;
  const netY = amountY - feeAmount;
  amm.x -= xOut;
  amm.y += netY;
  const trade: TradeInfo = {
    isBuy: false,
    amountX: xOut,
    amountY: amountY,
    timestamp,
    reserveX: amm.x,
    reserveY: amm.y,
  };
  updateFees(amm, trade);
  return trade;
}

function executeSellX(amm: AMM, amountX: number, timestamp: number): { totalY: number; trade: TradeInfo } | null {
  const { totalY, feeAmount } = quoteSellX(amm, amountX);
  if (totalY <= 0) return null;
  const netY = totalY - feeAmount;
  amm.x -= amountX;
  amm.y += netY;
  const trade: TradeInfo = {
    isBuy: false,
    amountX,
    amountY: totalY,
    timestamp,
    reserveX: amm.x,
    reserveY: amm.y,
  };
  updateFees(amm, trade);
  return { totalY, trade };
}

function updateFees(amm: AMM, trade: TradeInfo): void {
  if (amm.strategy) {
    const fees = amm.strategy.afterSwap(trade);
    amm.bidFee = clampFee(fees.bidFee);
    amm.askFee = clampFee(fees.askFee);
  }
}

export function arbitrage(amm: AMM, fairPrice: number, timestamp: number): void {
  const spot = spotPrice(amm);

  if (spot < fairPrice) {
    const k = amm.x * amm.y;
    const fee = amm.askFee;
    const gamma = 1 - fee;
    if (gamma <= 0 || fairPrice <= 0) return;

    const newX = Math.sqrt(k / (gamma * fairPrice));
    let amountX = amm.x - newX;
    if (amountX <= 0) return;
    amountX = Math.min(amountX, amm.x * 0.99);

    const { totalY } = quoteSellX(amm, amountX);
    if (totalY <= 0) return;

    const profit = amountX * fairPrice - totalY;
    if (profit <= 0) return;

    executeSellX(amm, amountX, timestamp);
    amm.edge += -profit;

  } else if (spot > fairPrice) {
    const k = amm.x * amm.y;
    const fee = amm.bidFee;
    const gamma = 1 - fee;
    if (gamma <= 0 || fairPrice <= 0) return;

    const xVirtual = Math.sqrt(k * gamma / fairPrice);
    const netX = xVirtual - amm.x;
    const amountX = netX / gamma;
    if (amountX <= 0) return;

    const { yOut } = quoteBuyX(amm, amountX);
    if (yOut <= 0) return;

    const profit = yOut - amountX * fairPrice;
    if (profit <= 0) return;

    executeBuyX(amm, amountX, timestamp);
    amm.edge += -profit;
  }
}

function splitBuy(amm1: AMM, amm2: AMM, totalY: number): [number, number] {
  const gamma1 = 1 - amm1.askFee;
  const gamma2 = 1 - amm2.askFee;
  const a1 = Math.sqrt(amm1.x * gamma1 * amm1.y);
  const a2 = Math.sqrt(amm2.x * gamma2 * amm2.y);
  if (a2 === 0) return [totalY, 0];
  const r = a1 / a2;
  const num = r * (amm2.y + gamma2 * totalY) - amm1.y;
  const den = gamma1 + r * gamma2;
  const y1 = den === 0 ? totalY / 2 : Math.max(0, Math.min(totalY, num / den));
  return [y1, totalY - y1];
}

function splitSell(amm1: AMM, amm2: AMM, totalX: number): [number, number] {
  const gamma1 = 1 - amm1.bidFee;
  const gamma2 = 1 - amm2.bidFee;
  const b1 = Math.sqrt(amm1.y * gamma1 * amm1.x);
  const b2 = Math.sqrt(amm2.y * gamma2 * amm2.x);
  if (b2 === 0) return [totalX, 0];
  const r = b1 / b2;
  const num = r * (amm2.x + gamma2 * totalX) - amm1.x;
  const den = gamma1 + r * gamma2;
  const x1 = den === 0 ? totalX / 2 : Math.max(0, Math.min(totalX, num / den));
  return [x1, totalX - x1];
}

const MIN_AMOUNT = 0.0001;

function routeRetailOrder(
  amm1: AMM,
  amm2: AMM,
  side: "buy" | "sell",
  size: number,
  fairPrice: number,
  timestamp: number,
): void {
  if (side === "buy") {
    const [y1, y2] = splitBuy(amm1, amm2, size);

    if (y1 > MIN_AMOUNT) {
      const trade = executeBuyXWithY(amm1, y1, timestamp);
      if (trade) {
        amm1.edge += trade.amountY - trade.amountX * fairPrice;
      }
    }
    if (y2 > MIN_AMOUNT) {
      const trade = executeBuyXWithY(amm2, y2, timestamp);
      if (trade) {
        amm2.edge += trade.amountY - trade.amountX * fairPrice;
      }
    }
  } else {
    const totalX = size / fairPrice;
    const [x1, x2] = splitSell(amm1, amm2, totalX);

    if (x1 > MIN_AMOUNT) {
      const trade = executeBuyX(amm1, x1, timestamp);
      if (trade) {
        amm1.edge += trade.amountX * fairPrice - trade.amountY;
      }
    }
    if (x2 > MIN_AMOUNT) {
      const trade = executeBuyX(amm2, x2, timestamp);
      if (trade) {
        amm2.edge += trade.amountX * fairPrice - trade.amountY;
      }
    }
  }
}

function runSimulation(strategy: Strategy): { edge: number; competitorEdge: number } {
  const sigma = uniform(GBM_SIGMA_MIN, GBM_SIGMA_MAX);
  const lambda = uniform(RETAIL_LAMBDA_MIN, RETAIL_LAMBDA_MAX);
  const retailMean = uniform(RETAIL_MEAN_MIN, RETAIL_MEAN_MAX);

  let fairPrice = INITIAL_PRICE;

  const initFees = strategy.afterInitialize(INITIAL_X, INITIAL_Y);

  const ourAmm: AMM = {
    x: INITIAL_X,
    y: INITIAL_Y,
    bidFee: clampFee(initFees.bidFee),
    askFee: clampFee(initFees.askFee),
    strategy,
    edge: 0,
  };

  const competitorAmm: AMM = {
    x: INITIAL_X,
    y: INITIAL_Y,
    bidFee: bpsToFee(30),
    askFee: bpsToFee(30),
    strategy: null,
    edge: 0,
  };

  for (let t = 0; t < STEPS; t++) {
    const z = randn();
    fairPrice = fairPrice * Math.exp(-0.5 * sigma * sigma + sigma * z);

    arbitrage(ourAmm, fairPrice, t);
    arbitrage(competitorAmm, fairPrice, t);

    const numArrivals = poisson(lambda);
    for (let i = 0; i < numArrivals; i++) {
      const sizeY = lognormal(retailMean, RETAIL_SIZE_SIGMA);
      const side: "buy" | "sell" = Math.random() < 0.5 ? "buy" : "sell";
      routeRetailOrder(ourAmm, competitorAmm, side, sizeY, fairPrice, t);
    }
  }

  return { edge: ourAmm.edge, competitorEdge: competitorAmm.edge };
}

export function createStaticStrategy(bps: number): Strategy {
  const fee = bpsToFee(bps);
  return {
    name: `Static ${bps}bps`,
    afterInitialize() {
      return { bidFee: fee, askFee: fee };
    },
    afterSwap() {
      return { bidFee: fee, askFee: fee };
    },
  };
}

export interface DynamicParams {
  baseBps: number;
  alpha: number;
  volScale: number;
  minBps: number;
  maxBps: number;
}

export function createDynamicStrategy(p: DynamicParams): Strategy {
  let ewmaVol = 0;
  let lastPrice = 0;

  const base = bpsToFee(p.baseBps);
  const minFee = bpsToFee(p.minBps);
  const maxFee = bpsToFee(p.maxBps);
  const cl = (f: number) => Math.max(minFee, Math.min(maxFee, f));

  return {
    name: `Dyn(b=${p.baseBps},a=${p.alpha},vs=${p.volScale},${p.minBps}-${p.maxBps})`,

    afterInitialize(initialX: number, initialY: number) {
      ewmaVol = 0;
      lastPrice = initialY / initialX;
      return { bidFee: base, askFee: base };
    },

    afterSwap(trade: TradeInfo) {
      const price = trade.reserveY / trade.reserveX;

      if (lastPrice > 0) {
        const absRet = Math.abs(price - lastPrice) / lastPrice;
        ewmaVol = p.alpha * (absRet * absRet) + (1 - p.alpha) * ewmaVol;
      }

      const fee = cl(base + Math.sqrt(ewmaVol) * p.volScale);
      lastPrice = price;
      return { bidFee: fee, askFee: fee };
    },
  };
}

export function benchmark(strategy: Strategy, numSims = NUM_SIMS, quiet = false): { avg: number; compAvg: number } {
  let totalEdge = 0;
  let totalCompEdge = 0;
  const edges: number[] = [];

  for (let i = 0; i < numSims; i++) {
    const { edge, competitorEdge } = runSimulation(strategy);
    totalEdge += edge;
    totalCompEdge += competitorEdge;
    edges.push(edge);
  }

  edges.sort((a, b) => a - b);
  const avg = totalEdge / numSims;
  const compAvg = totalCompEdge / numSims;

  if (!quiet) {
    const median = edges[Math.floor(edges.length / 2)];
    const p10 = edges[Math.floor(edges.length * 0.1)];
    const p90 = edges[Math.floor(edges.length * 0.9)];

    console.log(`\n=== ${strategy.name} ===`);
    console.log(`avg edge/sim:    ${avg.toFixed(2)}`);
    console.log(`median edge:     ${median!.toFixed(2)}`);
    console.log(`p10/p90:         ${p10!.toFixed(2)} / ${p90!.toFixed(2)}`);
    console.log(`competitor avg:  ${compAvg.toFixed(2)}`);
  }

  return { avg, compAvg };
}

if (import.meta.main) {
  console.log("\n========== BASELINE ==========");
  benchmark(createStaticStrategy(30), 100);

  console.log("\n========== DYNAMIC (tuned) ==========");
  const bestStrategy = createDynamicStrategy({
    baseBps: 12, alpha: 0.4, volScale: 1.3, minBps: 8, maxBps: 300,
  });
  benchmark(bestStrategy, 100);
}
