export class ConstantProductAMM {
  reserveA: number
  reserveB: number
  feeRate: number
  totalFees: { a: number; b: number }
  private k: number

  constructor(reserveA: number, reserveB: number, feeRate = 0.003) {
    this.reserveA = reserveA
    this.reserveB = reserveB
    this.feeRate = feeRate
    this.totalFees = { a: 0, b: 0 }
    this.k = reserveA * reserveB
  }

  getSpotPrice(): number {
    return this.reserveB / this.reserveA
  }

  // swap tokenA for tokenB
  swapAForB(amountIn: number): { amountOut: number; fee: number } {
    if (amountIn <= 0) throw new Error("amount must be positive")

    // calculate fee on input
    const fee = amountIn * this.feeRate
    const amountInAfterFee = amountIn - fee

    const newReserveA = this.reserveA + amountInAfterFee
    const newReserveB = this.k / newReserveA
    const amountOut = this.reserveB - newReserveB

    if (amountOut <= 0) throw new Error("insufficient liquidity")

    // update state
    this.reserveA = newReserveA
    this.reserveB = newReserveB
    this.totalFees.a += fee
    this.k = this.reserveA * this.reserveB

    return { amountOut, fee }
  }

  // swap tokenB for tokenA
  swapBForA(amountIn: number): { amountOut: number; fee: number } {
    if (amountIn <= 0) throw new Error("amount must be positive")

    const fee = amountIn * this.feeRate
    const amountInAfterFee = amountIn - fee

    const newReserveB = this.reserveB + amountInAfterFee
    const newReserveA = this.k / newReserveB
    const amountOut = this.reserveA - newReserveA

    if (amountOut <= 0) throw new Error("insufficient liquidity")

    this.reserveB = newReserveB
    this.reserveA = newReserveA
    this.totalFees.b += fee
    this.k = this.reserveA * this.reserveB

    return { amountOut, fee }
  }

  getInvariant(): number {
    return this.reserveA * this.reserveB
  }

  getInvariantDrift(): number {
    return (this.getInvariant() - this.k) / this.k
  }

  // simulate many swaps and return final state
  static simulateSwaps(
    reserveA: number,
    reserveB: number,
    swaps: Array<{ direction: "AtoB" | "BtoA"; amount: number }>,
    feeRate = 0.003,
  ): { amm: ConstantProductAMM; totalVolume: number } {
    const amm = new ConstantProductAMM(reserveA, reserveB, feeRate)
    let totalVolume = 0

    for (const swap of swaps) {
      if (swap.direction === "AtoB") {
        amm.swapAForB(swap.amount)
      } else {
        amm.swapBForA(swap.amount)
      }
      totalVolume += swap.amount
    }

    return { amm, totalVolume }
  }
}
