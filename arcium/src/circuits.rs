// ShadowLend — Arcium MXE Circuits
// These run inside Arcium's Multi-party eXecution Environment.
// No single node sees plaintext values.

// Circuit 1: LTV Check
// Proves borrow is within the allowed limit without revealing amounts.
// Input: encrypted collateral value, encrypted borrow value, max LTV (public)
// Output: ZK proof that borrow <= collateral * max_ltv — nothing else
pub fn check_ltv(
    collateral_value: u64,  // secret — encrypted
    borrow_value: u64,      // secret — encrypted
    max_ltv_bps: u16,       // public
) -> bool {
    let borrow_scaled = (borrow_value as u128) * 10_000;
    let collateral_scaled = (collateral_value as u128) * (max_ltv_bps as u128);
    borrow_scaled <= collateral_scaled
}

// Circuit 2: Liquidation Check
// Proves health factor < 1.0 without revealing the actual health factor.
// Liquidation bots learn only: this account IS liquidatable.
// They cannot rank accounts by vulnerability — each needs a separate proof.
pub fn check_liquidatable(
    collateral_value: u64,      // secret
    borrow_with_interest: u64,  // secret
    collateral_factor_bps: u16, // secret
) -> bool {
    let left = (collateral_value as u128) * (collateral_factor_bps as u128);
    let right = (borrow_with_interest as u128) * 10_000;
    left < right
}

// Circuit 3: Interest Accrual
// Updates encrypted borrow balance without revealing the rate or amount.
pub fn accrue_interest(
    borrow: u64,            // secret
    rate_bps: u32,          // secret
    slots_elapsed: u64,     // public
) -> u64 {
    let interest = (borrow as u128)
        * (rate_bps as u128)
        * (slots_elapsed as u128)
        / 10_000;
    borrow + interest as u64
}
