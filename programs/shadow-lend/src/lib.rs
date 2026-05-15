use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount, Transfer};
use pyth_solana_receiver_sdk::price_update::{Price, PriceUpdateV2};

declare_id!("5jqXbgExBEnKPahsQineFmMJHNcEvwnniiYvDy81bZCF");

pub const MARKET_SEED: &[u8] = b"market";
pub const AUTH_SEED: &[u8] = b"auth";
pub const VAULT_SEED: &[u8] = b"vault";
pub const CLAIM_SEED: &[u8] = b"claim";
pub const POSITION_SEED: &[u8] = b"position";
pub const STATS_SEED: &[u8] = b"stats";

pub const BPS_DENOMINATOR: u128 = 10_000;
/// Max age of a Pyth price update we will accept, in seconds. Devnet feeds are
/// updated less aggressively than mainnet, so we are generous here.
pub const MAX_PRICE_AGE_SEC: u64 = 300;
/// Liquidator bonus when seizing collateral from an underwater position.
/// 500 bps = 5%.
pub const LIQUIDATION_BONUS_BPS: u128 = 500;
/// Fixed-point scale for the per-market borrow index and per-slot rate.
/// 1e18, the same convention Compound uses.
pub const RATE_SCALE_E18: u128 = 1_000_000_000_000_000_000;

#[program]
pub mod shadow_lend {
    use super::*;

    /// Admin-only: creates a market — a test mint, its PDA-owned vault, and the
    /// per-market config (faucet amount, LTV, Pyth feed id). One call per asset.
    pub fn initialize_market(
        ctx: Context<InitializeMarket>,
        amount_per_claim: u64,
        max_ltv_bps: u16,
        feed_id: [u8; 32],
        borrow_rate_per_slot_e18: u64,
    ) -> Result<()> {
        require!(
            max_ltv_bps > 0 && (max_ltv_bps as u128) < BPS_DENOMINATOR,
            MarketError::BadLtv
        );
        let clock = Clock::get()?;
        let market = &mut ctx.accounts.market;
        market.admin = ctx.accounts.admin.key();
        market.mint = ctx.accounts.mint.key();
        market.amount_per_claim = amount_per_claim;
        market.max_ltv_bps = max_ltv_bps;
        market.feed_id = feed_id;
        market.borrow_rate_per_slot_e18 = borrow_rate_per_slot_e18;
        market.borrow_index_e18 = RATE_SCALE_E18;
        market.last_update_slot = clock.slot;
        market.total_supplied = 0;
        market.total_borrowed = 0;
        market.claim_count = 0;
        market.bump = ctx.bumps.market;
        market.authority_bump = ctx.bumps.authority;
        Ok(())
    }

    pub fn set_claim_amount(ctx: Context<AdminUpdate>, amount: u64) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.market.admin,
            ctx.accounts.admin.key(),
            MarketError::NotAdmin
        );
        ctx.accounts.market.amount_per_claim = amount;
        Ok(())
    }

    /// Mints `amount_per_claim` of this market's test token to the caller.
    /// One claim per (wallet, mint), enforced by the `ClaimReceipt` PDA.
    pub fn claim_faucet(ctx: Context<ClaimFaucet>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let receipt = &mut ctx.accounts.receipt;
        let clock = Clock::get()?;

        let amount = market.amount_per_claim;
        let mint_key = market.mint;
        let signer_seeds: &[&[&[u8]]] =
            &[&[AUTH_SEED, mint_key.as_ref(), &[market.authority_bump]]];

        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.recipient_ata.to_account_info(),
                    authority: ctx.accounts.authority.to_account_info(),
                },
                signer_seeds,
            ),
            amount,
        )?;

        receipt.user = ctx.accounts.user.key();
        receipt.mint = mint_key;
        receipt.amount = amount;
        receipt.claimed_at = clock.unix_timestamp;
        receipt.bump = ctx.bumps.receipt;

        market.claim_count = market
            .claim_count
            .checked_add(1)
            .ok_or(MarketError::Overflow)?;

        emit!(FaucetClaimed {
            user: ctx.accounts.user.key(),
            mint: mint_key,
            amount,
            claimed_at: clock.unix_timestamp,
        });
        Ok(())
    }

    /// Deposits `amount` of the market token into the vault. No global check
    /// needed — adding collateral can only improve a position's health.
    pub fn supply(ctx: Context<ModifyPosition>, amount: u64) -> Result<()> {
        require!(amount > 0, MarketError::ZeroAmount);
        let clock = Clock::get()?;
        accrue_market(&mut ctx.accounts.market, &clock);
        let position = &mut ctx.accounts.position;
        let market_ref = &ctx.accounts.market;
        if position.user == Pubkey::default() {
            position.user = ctx.accounts.user.key();
            position.market = market_ref.key();
            position.bump = ctx.bumps.position;
            position.feed_id = market_ref.feed_id;
            position.max_ltv_bps = market_ref.max_ltv_bps;
            position.decimals = MINT_DECIMALS;
            position.borrow_index_snapshot_e18 = market_ref.borrow_index_e18;
        } else {
            accrue_position(position, market_ref);
        }

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_ata.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;

        position.supplied = position
            .supplied
            .checked_add(amount)
            .ok_or(MarketError::Overflow)?;
        let supplied = position.supplied;
        let borrowed = position.borrowed;

        let market = &mut ctx.accounts.market;
        market.total_supplied = market
            .total_supplied
            .checked_add(amount)
            .ok_or(MarketError::Overflow)?;

        emit!(MarketAction {
            user: ctx.accounts.user.key(),
            mint: market.mint,
            kind: MarketActionKind::Supply,
            supplied,
            borrowed,
        });
        Ok(())
    }

    /// Withdraws collateral. Requires the user's total debt (across markets)
    /// still fits inside their post-withdraw collateral × LTV.
    /// `remaining_accounts`: pairs of (other_position, other_price_update) for
    /// every other market in which the user has activity.
    pub fn withdraw(ctx: Context<ModifyPositionWithPrice>, amount: u64) -> Result<()> {
        require!(amount > 0, MarketError::ZeroAmount);
        let clock = Clock::get()?;
        accrue_market(&mut ctx.accounts.market, &clock);
        let user_key = ctx.accounts.user.key();
        accrue_position(&mut ctx.accounts.position, &ctx.accounts.market);
        let position = &mut ctx.accounts.position;
        let position_key = position.key();
        let market_ref = &ctx.accounts.market;

        let new_supplied = position
            .supplied
            .checked_sub(amount)
            .ok_or(MarketError::InsufficientCollateral)?;

        // Health check: model the post-withdraw state and require debt ≤ LTV.
        let cur_price = ctx
            .accounts
            .price_update
            .get_price_no_older_than(&clock, MAX_PRICE_AGE_SEC, &market_ref.feed_id)
            .map_err(|_| error!(MarketError::StalePrice))?;
        let mut total_collat_at_ltv: u128 = 0;
        let mut total_debt: u128 = 0;
        accumulate_position(
            new_supplied,
            position.borrowed,
            &cur_price,
            position.decimals,
            position.max_ltv_bps,
            &mut total_collat_at_ltv,
            &mut total_debt,
        );
        accumulate_remaining(
            ctx.remaining_accounts,
            &user_key,
            position_key,
            &clock,
            &mut total_collat_at_ltv,
            &mut total_debt,
        )?;
        require!(
            total_debt <= total_collat_at_ltv,
            MarketError::WouldBeUnhealthy
        );

        let mint_key = market_ref.mint;
        let signer_seeds: &[&[&[u8]]] =
            &[&[AUTH_SEED, mint_key.as_ref(), &[market_ref.authority_bump]]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.user_ata.to_account_info(),
                    authority: ctx.accounts.authority.to_account_info(),
                },
                signer_seeds,
            ),
            amount,
        )?;

        position.supplied = new_supplied;
        let supplied = position.supplied;
        let borrowed = position.borrowed;
        let market = &mut ctx.accounts.market;
        market.total_supplied = market
            .total_supplied
            .checked_sub(amount)
            .ok_or(MarketError::Overflow)?;

        emit!(MarketAction {
            user: ctx.accounts.user.key(),
            mint: market.mint,
            kind: MarketActionKind::Withdraw,
            supplied,
            borrowed,
        });
        Ok(())
    }

    /// Borrows `amount` of this market's token against the user's total
    /// collateral across all markets. `remaining_accounts`: pairs of
    /// (other_position, other_price_update).
    pub fn borrow(ctx: Context<ModifyPositionWithPrice>, amount: u64) -> Result<()> {
        require!(amount > 0, MarketError::ZeroAmount);
        let clock = Clock::get()?;
        accrue_market(&mut ctx.accounts.market, &clock);
        let user_key = ctx.accounts.user.key();
        accrue_position(&mut ctx.accounts.position, &ctx.accounts.market);
        let position = &mut ctx.accounts.position;
        let position_key = position.key();
        let market_ref = &ctx.accounts.market;

        // Borrowing from a market you're already supplying to is flash-loan
        // shaped: the user's own collateral is the source of their debt. Force
        // cross-asset collateralisation.
        require!(
            position.supplied == 0,
            MarketError::SameMintBorrow
        );

        let new_borrowed = position
            .borrowed
            .checked_add(amount)
            .ok_or(MarketError::Overflow)?;
        require!(
            ctx.accounts.vault.amount >= amount,
            MarketError::InsufficientLiquidity
        );

        let cur_price = ctx
            .accounts
            .price_update
            .get_price_no_older_than(&clock, MAX_PRICE_AGE_SEC, &market_ref.feed_id)
            .map_err(|_| error!(MarketError::StalePrice))?;
        let mut total_collat_at_ltv: u128 = 0;
        let mut total_debt: u128 = 0;
        accumulate_position(
            position.supplied,
            new_borrowed,
            &cur_price,
            position.decimals,
            position.max_ltv_bps,
            &mut total_collat_at_ltv,
            &mut total_debt,
        );
        accumulate_remaining(
            ctx.remaining_accounts,
            &user_key,
            position_key,
            &clock,
            &mut total_collat_at_ltv,
            &mut total_debt,
        )?;
        require!(total_debt <= total_collat_at_ltv, MarketError::ExceedsLtv);

        let mint_key = market_ref.mint;
        let signer_seeds: &[&[&[u8]]] =
            &[&[AUTH_SEED, mint_key.as_ref(), &[market_ref.authority_bump]]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.user_ata.to_account_info(),
                    authority: ctx.accounts.authority.to_account_info(),
                },
                signer_seeds,
            ),
            amount,
        )?;

        position.borrowed = new_borrowed;
        let supplied = position.supplied;
        let borrowed = position.borrowed;
        let market = &mut ctx.accounts.market;
        market.total_borrowed = market
            .total_borrowed
            .checked_add(amount)
            .ok_or(MarketError::Overflow)?;

        emit!(MarketAction {
            user: ctx.accounts.user.key(),
            mint: market.mint,
            kind: MarketActionKind::Borrow,
            supplied,
            borrowed,
        });
        Ok(())
    }

    /// Repays debt in this market's token. No global check needed — paying
    /// down debt can only improve health.
    pub fn repay(ctx: Context<ModifyPosition>, amount: u64) -> Result<()> {
        require!(amount > 0, MarketError::ZeroAmount);
        let clock = Clock::get()?;
        accrue_market(&mut ctx.accounts.market, &clock);
        accrue_position(&mut ctx.accounts.position, &ctx.accounts.market);
        let position = &mut ctx.accounts.position;
        let pay = amount.min(position.borrowed);
        require!(pay > 0, MarketError::NothingToRepay);

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_ata.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            pay,
        )?;

        position.borrowed = position.borrowed.saturating_sub(pay);
        let supplied = position.supplied;
        let borrowed = position.borrowed;
        let market = &mut ctx.accounts.market;
        market.total_borrowed = market.total_borrowed.saturating_sub(pay);

        emit!(MarketAction {
            user: ctx.accounts.user.key(),
            mint: market.mint,
            kind: MarketActionKind::Repay,
            supplied,
            borrowed,
        });
        Ok(())
    }

    /// Liquidates an undercollateralised borrower. The liquidator repays some
    /// of the borrower's debt in `debt_mint` and seizes a USD-equivalent
    /// amount of `collateral_mint` plus a 5% bonus, drawn from the borrower's
    /// supplied collateral position. `remaining_accounts`: pairs of
    /// (other_position, other_price_update) for *every other* market the
    /// borrower has activity in, so the program can verify global health < 1.
    pub fn liquidate(ctx: Context<Liquidate>, repay_amount: u64) -> Result<()> {
        require!(repay_amount > 0, MarketError::ZeroAmount);
        let clock = Clock::get()?;
        // Bring both markets' indices and the borrower's positions up to date
        // so the health check and seize math operate on real current debt.
        accrue_market(&mut ctx.accounts.debt_market, &clock);
        accrue_market(&mut ctx.accounts.collateral_market, &clock);
        accrue_position(&mut ctx.accounts.debt_position, &ctx.accounts.debt_market);
        accrue_position(
            &mut ctx.accounts.collateral_position,
            &ctx.accounts.collateral_market,
        );
        let borrower = ctx.accounts.borrower.key();
        let liquidator_key = ctx.accounts.liquidator.key();

        require!(
            liquidator_key != borrower,
            MarketError::SelfLiquidation
        );

        require_keys_eq!(
            ctx.accounts.debt_position.user,
            borrower,
            MarketError::BadPositionAccount
        );
        require_keys_eq!(
            ctx.accounts.collateral_position.user,
            borrower,
            MarketError::BadPositionAccount
        );
        require!(
            ctx.accounts.debt_position.borrowed > 0,
            MarketError::NothingToLiquidate
        );
        require!(
            ctx.accounts.collateral_position.supplied > 0,
            MarketError::NothingToLiquidate
        );

        let debt_price = ctx
            .accounts
            .debt_price_update
            .get_price_no_older_than(&clock, MAX_PRICE_AGE_SEC, &ctx.accounts.debt_market.feed_id)
            .map_err(|_| error!(MarketError::StalePrice))?;
        let collat_price = ctx
            .accounts
            .collateral_price_update
            .get_price_no_older_than(
                &clock,
                MAX_PRICE_AGE_SEC,
                &ctx.accounts.collateral_market.feed_id,
            )
            .map_err(|_| error!(MarketError::StalePrice))?;

        // Compute borrower's global health. Include both main positions plus
        // any others passed in remaining_accounts.
        let mut total_collat_at_ltv: u128 = 0;
        let mut total_debt: u128 = 0;
        accumulate_position(
            ctx.accounts.debt_position.supplied,
            ctx.accounts.debt_position.borrowed,
            &debt_price,
            ctx.accounts.debt_position.decimals,
            ctx.accounts.debt_position.max_ltv_bps,
            &mut total_collat_at_ltv,
            &mut total_debt,
        );
        accumulate_position(
            ctx.accounts.collateral_position.supplied,
            ctx.accounts.collateral_position.borrowed,
            &collat_price,
            ctx.accounts.collateral_position.decimals,
            ctx.accounts.collateral_position.max_ltv_bps,
            &mut total_collat_at_ltv,
            &mut total_debt,
        );
        let skip_keys = [
            ctx.accounts.debt_position.key(),
            ctx.accounts.collateral_position.key(),
        ];
        accumulate_remaining_skip(
            ctx.remaining_accounts,
            &borrower,
            &skip_keys,
            &clock,
            &mut total_collat_at_ltv,
            &mut total_debt,
        )?;
        require!(
            total_debt > total_collat_at_ltv,
            MarketError::PositionHealthy
        );

        // Floor the actual repay to what the borrower actually owes.
        let actual_repay = repay_amount.min(ctx.accounts.debt_position.borrowed);

        // Compute how much collateral the liquidator gets to seize.
        let repay_value_usd = token_usd_8dp(
            actual_repay,
            &debt_price,
            ctx.accounts.debt_position.decimals,
        );
        let seize_value_usd = repay_value_usd
            .saturating_mul(BPS_DENOMINATOR + LIQUIDATION_BONUS_BPS)
            / BPS_DENOMINATOR;
        let mut seize_amount = usd_8dp_to_token(
            seize_value_usd,
            &collat_price,
            ctx.accounts.collateral_position.decimals,
        );
        // Cap to the borrower's actual supplied collateral.
        if seize_amount > ctx.accounts.collateral_position.supplied {
            seize_amount = ctx.accounts.collateral_position.supplied;
        }
        require!(seize_amount > 0, MarketError::NothingToLiquidate);
        require!(
            ctx.accounts.collateral_vault.amount >= seize_amount,
            MarketError::InsufficientLiquidity
        );

        // 1. Liquidator pays debt token into the debt vault.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.liquidator_debt_ata.to_account_info(),
                    to: ctx.accounts.debt_vault.to_account_info(),
                    authority: ctx.accounts.liquidator.to_account_info(),
                },
            ),
            actual_repay,
        )?;

        // 2. Collateral vault releases seized tokens to liquidator.
        let collat_mint = ctx.accounts.collateral_market.mint;
        let collat_auth_bump = ctx.accounts.collateral_market.authority_bump;
        let signer_seeds: &[&[&[u8]]] = &[&[
            AUTH_SEED,
            collat_mint.as_ref(),
            &[collat_auth_bump],
        ]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.collateral_vault.to_account_info(),
                    to: ctx.accounts.liquidator_collateral_ata.to_account_info(),
                    authority: ctx.accounts.collateral_authority.to_account_info(),
                },
                signer_seeds,
            ),
            seize_amount,
        )?;

        // 3. Update borrower's positions and market totals.
        let debt_position = &mut ctx.accounts.debt_position;
        debt_position.borrowed = debt_position.borrowed.saturating_sub(actual_repay);
        let collateral_position = &mut ctx.accounts.collateral_position;
        collateral_position.supplied =
            collateral_position.supplied.saturating_sub(seize_amount);

        let debt_market = &mut ctx.accounts.debt_market;
        debt_market.total_borrowed = debt_market.total_borrowed.saturating_sub(actual_repay);
        let collateral_market = &mut ctx.accounts.collateral_market;
        collateral_market.total_supplied =
            collateral_market.total_supplied.saturating_sub(seize_amount);

        emit!(Liquidation {
            liquidator: liquidator_key,
            borrower,
            debt_mint: debt_market.mint,
            collateral_mint: collateral_market.mint,
            repaid: actual_repay,
            seized: seize_amount,
        });
        Ok(())
    }

    pub fn init_user_stats(ctx: Context<InitUserStats>) -> Result<()> {
        let stats = &mut ctx.accounts.stats;
        let clock = Clock::get()?;
        stats.user = ctx.accounts.user.key();
        stats.created_at = clock.unix_timestamp;
        stats.last_action_at = clock.unix_timestamp;
        stats.proofs_submitted = 0;
        stats.supplies = 0;
        stats.borrows = 0;
        stats.repays = 0;
        stats.liquidations = 0;
        stats.last_health_bps = 0;
        stats.bump = ctx.bumps.stats;
        Ok(())
    }

    pub fn record_action(
        ctx: Context<RecordAction>,
        kind: ActionKind,
        health_bps: u16,
    ) -> Result<()> {
        let stats = &mut ctx.accounts.stats;
        require_keys_eq!(stats.user, ctx.accounts.user.key(), StatsError::Unauthorized);

        let clock = Clock::get()?;
        stats.last_action_at = clock.unix_timestamp;
        stats.proofs_submitted = stats
            .proofs_submitted
            .checked_add(1)
            .ok_or(StatsError::Overflow)?;
        if health_bps > 0 {
            stats.last_health_bps = health_bps;
        }

        match kind {
            ActionKind::Supply => {
                stats.supplies = stats.supplies.checked_add(1).ok_or(StatsError::Overflow)?
            }
            ActionKind::Borrow => {
                stats.borrows = stats.borrows.checked_add(1).ok_or(StatsError::Overflow)?
            }
            ActionKind::Repay => {
                stats.repays = stats.repays.checked_add(1).ok_or(StatsError::Overflow)?
            }
            ActionKind::Liquidation => {
                stats.liquidations = stats
                    .liquidations
                    .checked_add(1)
                    .ok_or(StatsError::Overflow)?
            }
        }

        emit!(ActionRecorded {
            user: ctx.accounts.user.key(),
            kind,
            health_bps,
            at: clock.unix_timestamp,
        });
        Ok(())
    }
}

pub const MINT_DECIMALS: u8 = 9;

/// USD value of `amount` of a token, scaled to 8 decimals of dollars
/// (i.e. the unit is dollar-microcents, 1e-8 USD). Saturating math: a wildly
/// out-of-range result clamps rather than overflows.
fn token_usd_8dp(amount: u64, price: &Price, decimals: u8) -> u128 {
    let p = price.price.max(0) as u128;
    let raw = (amount as u128).saturating_mul(p);
    let shift = 8i32 + price.exponent - decimals as i32;
    if shift >= 0 {
        let mul = 10u128.checked_pow(shift as u32).unwrap_or(u128::MAX);
        raw.saturating_mul(mul)
    } else {
        let div = 10u128.checked_pow((-shift) as u32).unwrap_or(1);
        raw / div
    }
}

/// Inverse of `token_usd_8dp`: how many base units of a token (`decimals`
/// precision, priced at `price`) does `value_usd_8dp` correspond to.
fn usd_8dp_to_token(value_usd_8dp: u128, price: &Price, decimals: u8) -> u64 {
    if price.price <= 0 {
        return 0;
    }
    let p = price.price as u128;
    let shift = 8i32 + price.exponent - decimals as i32;
    let result = if shift >= 0 {
        let pow = 10u128.checked_pow(shift as u32).unwrap_or(u128::MAX);
        let denom = p.saturating_mul(pow);
        if denom == 0 { 0 } else { value_usd_8dp / denom }
    } else {
        let mul = 10u128.checked_pow((-shift) as u32).unwrap_or(1);
        value_usd_8dp.saturating_mul(mul) / p
    };
    result.try_into().unwrap_or(u64::MAX)
}

/// Advances the market's borrow index by however many slots have passed since
/// the last accrual. Linear approximation, which slightly underestimates
/// continuous compounding but is fine at devnet rates.
fn accrue_market(market: &mut Market, clock: &Clock) {
    if clock.slot <= market.last_update_slot {
        return;
    }
    let slots = (clock.slot - market.last_update_slot) as u128;
    let growth = market
        .borrow_index_e18
        .saturating_mul(market.borrow_rate_per_slot_e18 as u128)
        .saturating_mul(slots)
        / RATE_SCALE_E18;
    market.borrow_index_e18 = market.borrow_index_e18.saturating_add(growth);
    market.last_update_slot = clock.slot;
}

/// Restates `position.borrowed` from its snapshot index to the market's
/// current index. Idempotent: after the call the position's snapshot equals
/// the market's current index.
fn accrue_position(position: &mut Position, market: &Market) {
    if position.borrow_index_snapshot_e18 == 0
        || position.borrowed == 0
        || position.borrow_index_snapshot_e18 == market.borrow_index_e18
    {
        position.borrow_index_snapshot_e18 = market.borrow_index_e18;
        return;
    }
    let current = (position.borrowed as u128)
        .saturating_mul(market.borrow_index_e18)
        / position.borrow_index_snapshot_e18;
    position.borrowed = current.try_into().unwrap_or(u64::MAX);
    position.borrow_index_snapshot_e18 = market.borrow_index_e18;
}

fn accumulate_position(
    supplied: u64,
    borrowed: u64,
    price: &Price,
    decimals: u8,
    max_ltv_bps: u16,
    total_collat_at_ltv: &mut u128,
    total_debt: &mut u128,
) {
    let collat = token_usd_8dp(supplied, price, decimals);
    let at_ltv = collat.saturating_mul(max_ltv_bps as u128) / BPS_DENOMINATOR;
    *total_collat_at_ltv = total_collat_at_ltv.saturating_add(at_ltv);
    *total_debt = total_debt.saturating_add(token_usd_8dp(borrowed, price, decimals));
}

/// Walks `remaining_accounts` as (Position, PriceUpdateV2) pairs, summing
/// each into the running collateral and debt totals. Skips the position whose
/// pubkey matches `skip` — that one is already counted from the main accounts.
/// Like `accumulate_remaining` but skips any positions whose key appears in
/// `skip_keys`. Used by `liquidate`, which has two main positions in scope.
fn accumulate_remaining_skip<'info>(
    ras: &[AccountInfo<'info>],
    user: &Pubkey,
    skip_keys: &[Pubkey],
    clock: &Clock,
    total_collat_at_ltv: &mut u128,
    total_debt: &mut u128,
) -> Result<()> {
    require!(
        ras.len() % 2 == 0,
        MarketError::BadRemainingAccounts
    );
    let mut i = 0;
    while i < ras.len() {
        let pos_ai = &ras[i];
        let pu_ai = &ras[i + 1];
        i += 2;
        if skip_keys.iter().any(|k| k == pos_ai.key) {
            continue;
        }
        require!(pos_ai.owner == &crate::ID, MarketError::BadPositionAccount);
        let pos = Position::try_deserialize(&mut &pos_ai.data.borrow()[..])
            .map_err(|_| error!(MarketError::BadPositionAccount))?;
        require_keys_eq!(pos.user, *user, MarketError::BadPositionAccount);
        if pos.supplied == 0 && pos.borrowed == 0 {
            continue;
        }
        require!(
            pu_ai.owner == &pyth_solana_receiver_sdk::ID,
            MarketError::BadPriceFeed
        );
        let pu = PriceUpdateV2::try_deserialize(&mut &pu_ai.data.borrow()[..])
            .map_err(|_| error!(MarketError::BadPriceFeed))?;
        let price = pu
            .get_price_no_older_than(clock, MAX_PRICE_AGE_SEC, &pos.feed_id)
            .map_err(|_| error!(MarketError::StalePrice))?;
        accumulate_position(
            pos.supplied,
            pos.borrowed,
            &price,
            pos.decimals,
            pos.max_ltv_bps,
            total_collat_at_ltv,
            total_debt,
        );
    }
    Ok(())
}

fn accumulate_remaining<'info>(
    ras: &[AccountInfo<'info>],
    user: &Pubkey,
    skip: Pubkey,
    clock: &Clock,
    total_collat_at_ltv: &mut u128,
    total_debt: &mut u128,
) -> Result<()> {
    require!(
        ras.len() % 2 == 0,
        MarketError::BadRemainingAccounts
    );
    let mut i = 0;
    while i < ras.len() {
        let pos_ai = &ras[i];
        let pu_ai = &ras[i + 1];
        i += 2;
        if pos_ai.key == &skip {
            continue;
        }
        require!(
            pos_ai.owner == &crate::ID,
            MarketError::BadPositionAccount
        );
        let pos = Position::try_deserialize(&mut &pos_ai.data.borrow()[..])
            .map_err(|_| error!(MarketError::BadPositionAccount))?;
        require_keys_eq!(pos.user, *user, MarketError::BadPositionAccount);
        if pos.supplied == 0 && pos.borrowed == 0 {
            continue;
        }
        require!(
            pu_ai.owner == &pyth_solana_receiver_sdk::ID,
            MarketError::BadPriceFeed
        );
        let pu = PriceUpdateV2::try_deserialize(&mut &pu_ai.data.borrow()[..])
            .map_err(|_| error!(MarketError::BadPriceFeed))?;
        let price = pu
            .get_price_no_older_than(clock, MAX_PRICE_AGE_SEC, &pos.feed_id)
            .map_err(|_| error!(MarketError::StalePrice))?;
        accumulate_position(
            pos.supplied,
            pos.borrowed,
            &price,
            pos.decimals,
            pos.max_ltv_bps,
            total_collat_at_ltv,
            total_debt,
        );
    }
    Ok(())
}

#[derive(Accounts)]
pub struct InitializeMarket<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + Market::SIZE,
        seeds = [MARKET_SEED, mint.key().as_ref()],
        bump
    )]
    pub market: Box<Account<'info, Market>>,

    #[account(
        init,
        payer = admin,
        mint::decimals = 9,
        mint::authority = authority,
        mint::freeze_authority = authority,
    )]
    pub mint: Box<Account<'info, Mint>>,

    /// CHECK: PDA authority for this market's mint and vault.
    #[account(seeds = [AUTH_SEED, mint.key().as_ref()], bump)]
    pub authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = admin,
        token::mint = mint,
        token::authority = authority,
        seeds = [VAULT_SEED, mint.key().as_ref()],
        bump
    )]
    pub vault: Box<Account<'info, TokenAccount>>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminUpdate<'info> {
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [MARKET_SEED, market.mint.as_ref()],
        bump = market.bump
    )]
    pub market: Box<Account<'info, Market>>,
}

#[derive(Accounts)]
pub struct ClaimFaucet<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [MARKET_SEED, mint.key().as_ref()],
        bump = market.bump,
        has_one = mint @ MarketError::MintMismatch
    )]
    pub market: Box<Account<'info, Market>>,

    #[account(mut, address = market.mint @ MarketError::MintMismatch)]
    pub mint: Box<Account<'info, Mint>>,

    /// CHECK: PDA authority for this market's mint.
    #[account(seeds = [AUTH_SEED, mint.key().as_ref()], bump = market.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = user,
        associated_token::mint = mint,
        associated_token::authority = user
    )]
    pub recipient_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = user,
        space = 8 + ClaimReceipt::SIZE,
        seeds = [CLAIM_SEED, user.key().as_ref(), mint.key().as_ref()],
        bump
    )]
    pub receipt: Box<Account<'info, ClaimReceipt>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

/// Supply / repay context — no price check needed.
#[derive(Accounts)]
pub struct ModifyPosition<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [MARKET_SEED, mint.key().as_ref()],
        bump = market.bump,
        has_one = mint @ MarketError::MintMismatch
    )]
    pub market: Box<Account<'info, Market>>,

    #[account(address = market.mint @ MarketError::MintMismatch)]
    pub mint: Box<Account<'info, Mint>>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + Position::SIZE,
        seeds = [POSITION_SEED, user.key().as_ref(), mint.key().as_ref()],
        bump
    )]
    pub position: Box<Account<'info, Position>>,

    #[account(
        mut,
        seeds = [VAULT_SEED, mint.key().as_ref()],
        bump
    )]
    pub vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: PDA authority for this market's vault.
    #[account(seeds = [AUTH_SEED, mint.key().as_ref()], bump = market.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = user,
        associated_token::mint = mint,
        associated_token::authority = user
    )]
    pub user_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

/// Withdraw / borrow context — needs the current market's price update for the
/// global health check.
#[derive(Accounts)]
pub struct ModifyPositionWithPrice<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [MARKET_SEED, mint.key().as_ref()],
        bump = market.bump,
        has_one = mint @ MarketError::MintMismatch
    )]
    pub market: Box<Account<'info, Market>>,

    #[account(address = market.mint @ MarketError::MintMismatch)]
    pub mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        seeds = [POSITION_SEED, user.key().as_ref(), mint.key().as_ref()],
        bump = position.bump,
        has_one = user @ MarketError::BadPositionAccount
    )]
    pub position: Box<Account<'info, Position>>,

    #[account(
        mut,
        seeds = [VAULT_SEED, mint.key().as_ref()],
        bump
    )]
    pub vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: PDA authority for this market's vault.
    #[account(seeds = [AUTH_SEED, mint.key().as_ref()], bump = market.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = user,
        associated_token::mint = mint,
        associated_token::authority = user
    )]
    pub user_ata: Box<Account<'info, TokenAccount>>,

    /// Pyth PriceUpdateV2 for this market's mint. Verified against
    /// `market.feed_id` at use time.
    pub price_update: Box<Account<'info, PriceUpdateV2>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct InitUserStats<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        init,
        payer = user,
        space = 8 + UserStats::SIZE,
        seeds = [STATS_SEED, user.key().as_ref()],
        bump
    )]
    pub stats: Box<Account<'info, UserStats>>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RecordAction<'info> {
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [STATS_SEED, user.key().as_ref()],
        bump = stats.bump
    )]
    pub stats: Box<Account<'info, UserStats>>,
}

/// Liquidation context. The liquidator and borrower are different wallets; the
/// program pulls the borrower's debt and collateral positions by PDA from the
/// supplied borrower pubkey.
#[derive(Accounts)]
pub struct Liquidate<'info> {
    #[account(mut)]
    pub liquidator: Signer<'info>,

    /// CHECK: only used to derive the borrower's PDA addresses.
    pub borrower: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [MARKET_SEED, debt_market.mint.as_ref()],
        bump = debt_market.bump
    )]
    pub debt_market: Box<Account<'info, Market>>,

    #[account(
        mut,
        seeds = [POSITION_SEED, borrower.key().as_ref(), debt_market.mint.as_ref()],
        bump = debt_position.bump
    )]
    pub debt_position: Box<Account<'info, Position>>,

    #[account(
        mut,
        seeds = [VAULT_SEED, debt_market.mint.as_ref()],
        bump
    )]
    pub debt_vault: Box<Account<'info, TokenAccount>>,

    pub debt_price_update: Box<Account<'info, PriceUpdateV2>>,

    #[account(
        mut,
        constraint = liquidator_debt_ata.mint == debt_market.mint @ MarketError::MintMismatch,
        constraint = liquidator_debt_ata.owner == liquidator.key() @ MarketError::Unauthorized,
    )]
    pub liquidator_debt_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [MARKET_SEED, collateral_market.mint.as_ref()],
        bump = collateral_market.bump
    )]
    pub collateral_market: Box<Account<'info, Market>>,

    #[account(
        mut,
        seeds = [POSITION_SEED, borrower.key().as_ref(), collateral_market.mint.as_ref()],
        bump = collateral_position.bump
    )]
    pub collateral_position: Box<Account<'info, Position>>,

    #[account(
        mut,
        seeds = [VAULT_SEED, collateral_market.mint.as_ref()],
        bump
    )]
    pub collateral_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: PDA mint+vault authority for the collateral market, signs the
    /// vault → liquidator transfer.
    #[account(
        seeds = [AUTH_SEED, collateral_market.mint.as_ref()],
        bump = collateral_market.authority_bump
    )]
    pub collateral_authority: UncheckedAccount<'info>,

    pub collateral_price_update: Box<Account<'info, PriceUpdateV2>>,

    #[account(
        mut,
        constraint = liquidator_collateral_ata.mint == collateral_market.mint @ MarketError::MintMismatch,
        constraint = liquidator_collateral_ata.owner == liquidator.key() @ MarketError::Unauthorized,
    )]
    pub liquidator_collateral_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

#[account]
pub struct Market {
    pub admin: Pubkey,
    pub mint: Pubkey,
    pub amount_per_claim: u64,
    pub max_ltv_bps: u16,
    pub feed_id: [u8; 32],
    pub borrow_rate_per_slot_e18: u64,
    pub borrow_index_e18: u128,
    pub last_update_slot: u64,
    pub total_supplied: u64,
    pub total_borrowed: u64,
    pub claim_count: u64,
    pub bump: u8,
    pub authority_bump: u8,
}

impl Market {
    pub const SIZE: usize =
        32 + 32 + 8 + 2 + 32 + 8 + 16 + 8 + 8 + 8 + 8 + 1 + 1;
}

#[account]
pub struct Position {
    pub user: Pubkey,
    pub market: Pubkey,
    pub supplied: u64,
    pub borrowed: u64,
    pub feed_id: [u8; 32],
    pub max_ltv_bps: u16,
    pub decimals: u8,
    pub bump: u8,
    /// Snapshot of the market's borrow index at the user's last interaction.
    /// `effective_borrowed = borrowed × market.borrow_index_e18 / snapshot`.
    pub borrow_index_snapshot_e18: u128,
}

impl Position {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 32 + 2 + 1 + 1 + 16;
}

#[account]
pub struct ClaimReceipt {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub claimed_at: i64,
    pub bump: u8,
}

impl ClaimReceipt {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 1;
}

#[account]
pub struct UserStats {
    pub user: Pubkey,
    pub created_at: i64,
    pub last_action_at: i64,
    pub proofs_submitted: u64,
    pub supplies: u64,
    pub borrows: u64,
    pub repays: u64,
    pub liquidations: u64,
    pub last_health_bps: u16,
    pub bump: u8,
}

impl UserStats {
    pub const SIZE: usize = 32 + 8 + 8 + 8 + 8 + 8 + 8 + 8 + 2 + 1;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActionKind {
    Supply,
    Borrow,
    Repay,
    Liquidation,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarketActionKind {
    Supply,
    Withdraw,
    Borrow,
    Repay,
}

#[event]
pub struct FaucetClaimed {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub claimed_at: i64,
}

#[event]
pub struct MarketAction {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub kind: MarketActionKind,
    pub supplied: u64,
    pub borrowed: u64,
}

#[event]
pub struct ActionRecorded {
    pub user: Pubkey,
    pub kind: ActionKind,
    pub health_bps: u16,
    pub at: i64,
}

#[event]
pub struct Liquidation {
    pub liquidator: Pubkey,
    pub borrower: Pubkey,
    pub debt_mint: Pubkey,
    pub collateral_mint: Pubkey,
    pub repaid: u64,
    pub seized: u64,
}

#[error_code]
pub enum MarketError {
    #[msg("Caller is not the market admin.")]
    NotAdmin,
    #[msg("Mint account does not match the market config.")]
    MintMismatch,
    #[msg("Max LTV must be between 1 and 9999 bps.")]
    BadLtv,
    #[msg("Amount must be greater than zero.")]
    ZeroAmount,
    #[msg("Borrow would exceed the user's total collateral × LTV.")]
    ExceedsLtv,
    #[msg("Withdrawing this much would leave the user undercollateralized.")]
    WouldBeUnhealthy,
    #[msg("Not enough supplied collateral in this market.")]
    InsufficientCollateral,
    #[msg("Vault does not have enough liquidity for this borrow.")]
    InsufficientLiquidity,
    #[msg("Position has no outstanding debt to repay.")]
    NothingToRepay,
    #[msg("Arithmetic overflow.")]
    Overflow,
    #[msg("remaining_accounts must be (position, price_update) pairs.")]
    BadRemainingAccounts,
    #[msg("Position account is not owned by this program or not this user's.")]
    BadPositionAccount,
    #[msg("Price feed account is invalid or the feed id does not match.")]
    BadPriceFeed,
    #[msg("Price update is older than the maximum accepted age.")]
    StalePrice,
    #[msg("Cannot borrow from a market you've supplied to. Use cross-asset collateral.")]
    SameMintBorrow,
    #[msg("Position is healthy (debt within LTV); not eligible for liquidation.")]
    PositionHealthy,
    #[msg("Position has nothing of the relevant kind to liquidate.")]
    NothingToLiquidate,
    #[msg("Liquidators cannot liquidate themselves.")]
    SelfLiquidation,
    #[msg("Account owner check failed.")]
    Unauthorized,
}

#[error_code]
pub enum StatsError {
    #[msg("Stats account does not belong to this signer.")]
    Unauthorized,
    #[msg("Arithmetic overflow.")]
    Overflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(p: i64, exponent: i32) -> Price {
        Price {
            price: p,
            conf: 0,
            exponent,
            publish_time: 0,
        }
    }

    // ── token_usd_8dp ────────────────────────────────────────

    #[test]
    fn token_usd_basic_round_dollar() {
        // 1 SOL (1e9 atoms, 9 decimals) at $100 (price = 1e10, expo = -8).
        // Expected USD * 1e8 = $100 * 1e8 = 1e10.
        let v = token_usd_8dp(1_000_000_000, &price(10_000_000_000, -8), 9);
        assert_eq!(v, 10_000_000_000);
    }

    #[test]
    fn token_usd_fractional_token() {
        // 0.5 SOL at $100 → $50 → 5e9 in 8dp.
        let v = token_usd_8dp(500_000_000, &price(10_000_000_000, -8), 9);
        assert_eq!(v, 5_000_000_000);
    }

    #[test]
    fn token_usd_btc_high_price() {
        // 0.001 BTC at $80,000 = $80 = 8e9 in 8dp.
        // BTC 9 decimals: 0.001 BTC = 1e6 atoms.
        // price = 80_000 * 1e8 = 8e12 at expo = -8.
        let v = token_usd_8dp(1_000_000, &price(8_000_000_000_000, -8), 9);
        assert_eq!(v, 8_000_000_000);
    }

    #[test]
    fn token_usd_zero_amount() {
        assert_eq!(token_usd_8dp(0, &price(10_000_000_000, -8), 9), 0);
    }

    #[test]
    fn token_usd_negative_price_clamps_to_zero() {
        let v = token_usd_8dp(1_000_000_000, &price(-100, -8), 9);
        assert_eq!(v, 0);
    }

    // ── usd_8dp_to_token (inverse) ───────────────────────────

    #[test]
    fn round_trip_sol_at_100_dollars() {
        let p = price(10_000_000_000, -8);
        let atoms = 1_234_567_890_000u64; // ~1234.56 SOL
        let usd = token_usd_8dp(atoms, &p, 9);
        let back = usd_8dp_to_token(usd, &p, 9);
        // Allow 1-atom round-trip error from the int division.
        assert!((atoms as i128 - back as i128).abs() <= 1, "{} vs {}", atoms, back);
    }

    #[test]
    fn usd_to_token_zero_price_returns_zero() {
        // A zero or negative price must not let a liquidator seize collateral
        // for free. usd_8dp_to_token returns 0 so the liquidate path will hit
        // the NothingToLiquidate guard.
        assert_eq!(usd_8dp_to_token(1_000_000_000, &price(0, -8), 9), 0);
        assert_eq!(usd_8dp_to_token(1_000_000_000, &price(-42, -8), 9), 0);
    }

    // ── accrue_market ────────────────────────────────────────

    fn test_market(borrow_rate_per_slot_e18: u64, last_slot: u64) -> Market {
        Market {
            admin: Pubkey::default(),
            mint: Pubkey::default(),
            amount_per_claim: 0,
            max_ltv_bps: 8000,
            feed_id: [0u8; 32],
            borrow_rate_per_slot_e18,
            borrow_index_e18: RATE_SCALE_E18,
            last_update_slot: last_slot,
            total_supplied: 0,
            total_borrowed: 0,
            claim_count: 0,
            bump: 0,
            authority_bump: 0,
        }
    }

    fn clock_at(slot: u64) -> Clock {
        Clock {
            slot,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
        }
    }

    #[test]
    fn accrue_zero_slots_is_no_op() {
        let mut m = test_market(1_000_000, 42);
        accrue_market(&mut m, &clock_at(42));
        assert_eq!(m.borrow_index_e18, RATE_SCALE_E18);
        assert_eq!(m.last_update_slot, 42);
    }

    #[test]
    fn accrue_grows_index_linearly() {
        // rate = 1e9 per slot (i.e. 1e9 / 1e18 = 1e-9 of index per slot).
        // After 100 slots, growth = 1e18 * 1e9 * 100 / 1e18 = 1e11.
        let mut m = test_market(1_000_000_000, 0);
        accrue_market(&mut m, &clock_at(100));
        assert_eq!(m.borrow_index_e18, RATE_SCALE_E18 + 100_000_000_000);
        assert_eq!(m.last_update_slot, 100);
    }

    #[test]
    fn accrue_zero_rate_is_no_op_for_index() {
        let mut m = test_market(0, 0);
        accrue_market(&mut m, &clock_at(1_000_000));
        assert_eq!(m.borrow_index_e18, RATE_SCALE_E18);
        assert_eq!(m.last_update_slot, 1_000_000);
    }

    // ── accrue_position ─────────────────────────────────────

    fn test_position(borrowed: u64, snapshot: u128) -> Position {
        Position {
            user: Pubkey::default(),
            market: Pubkey::default(),
            supplied: 0,
            borrowed,
            feed_id: [0u8; 32],
            max_ltv_bps: 8000,
            decimals: 9,
            bump: 0,
            borrow_index_snapshot_e18: snapshot,
        }
    }

    #[test]
    fn accrue_position_no_debt_just_updates_snapshot() {
        let m = test_market(1_000_000_000, 0);
        let mut m = m;
        accrue_market(&mut m, &clock_at(100));
        let mut p = test_position(0, RATE_SCALE_E18);
        accrue_position(&mut p, &m);
        assert_eq!(p.borrowed, 0);
        assert_eq!(p.borrow_index_snapshot_e18, m.borrow_index_e18);
    }

    #[test]
    fn accrue_position_grows_debt_proportionally() {
        // Start with debt 1000 at snapshot = 1e18. After accrual the market
        // index doubled. Debt should also double.
        let mut m = test_market(0, 0);
        m.borrow_index_e18 = 2 * RATE_SCALE_E18;
        let mut p = test_position(1_000, RATE_SCALE_E18);
        accrue_position(&mut p, &m);
        assert_eq!(p.borrowed, 2_000);
        assert_eq!(p.borrow_index_snapshot_e18, m.borrow_index_e18);
    }

    #[test]
    fn accrue_position_idempotent() {
        let m = test_market(0, 0);
        let mut p = test_position(1_000, m.borrow_index_e18);
        accrue_position(&mut p, &m);
        let after = p.borrowed;
        accrue_position(&mut p, &m);
        assert_eq!(p.borrowed, after);
    }

    #[test]
    fn accrue_position_zero_snapshot_initialises() {
        // First time we see a position with a 0 snapshot, just adopt the
        // market's index without touching `borrowed`.
        let m = test_market(0, 0);
        let mut p = test_position(500, 0);
        accrue_position(&mut p, &m);
        assert_eq!(p.borrowed, 500);
        assert_eq!(p.borrow_index_snapshot_e18, m.borrow_index_e18);
    }
}
