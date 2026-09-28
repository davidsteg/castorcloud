// What the monthly AI credit buys at the provider. The customer pays the
// credit incl. VAT and Stripe fees; only the rest is spendable, so the
// gateway's hard limit is set to that net amount.
import { STRIPE_FEES, VAT_RATE } from "./catalog.ts";

/** Spendable CHF per month for a booked credit (rounded down to the Rappen). */
export function aiBudgetChf(aiCreditChf: number): number {
  if (!(aiCreditChf > 0)) return 0;
  const net = aiCreditChf / (1 + VAT_RATE) - aiCreditChf * STRIPE_FEES.percent;
  return Math.floor(net * 100) / 100;
}

/**
 * The gateway (LiteLLM) meters in USD. `usdPerChf` is set conservatively in
 * the node config (AI_USD_PER_CHF) so exchange-rate moves never overspend.
 */
export function aiBudgetUsd(aiCreditChf: number, usdPerChf: number): number {
  return Math.floor(aiBudgetChf(aiCreditChf) * usdPerChf * 100) / 100;
}

/** Share of the budget at which the customer is told the credit runs low. */
export const AI_BUDGET_WARN_AT = 0.8;
