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
    ) -> Result<()> {
        require!(
            max_ltv_bps > 0 && (max_ltv_bps as u128) < BPS_DENOMINATOR,
            MarketError::BadLtv
        );
        let market = &mut ctx.accounts.market;
        market.admin = ctx.accounts.admin.key();
        market.mint = ctx.accounts.mint.key();
        market.amount_per_claim = amount_per_claim;
        market.max_ltv_bps = max_ltv_bps;
        market.feed_id = feed_id;
        market.total_supplied = 0;
        market.total_borrowed = 0;
        market.total_claimed = 0;
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

        market.total_claimed = market
            .total_claimed
            .checked_add(amount)
            .ok_or(MarketError::Overflow)?;
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
        let position = &mut ctx.accounts.position;
        let market_ref = &ctx.accounts.market;
        if position.user == Pubkey::default() {
            position.user = ctx.accounts.user.key();
            position.market = market_ref.key();
            position.bump = ctx.bumps.position;
            position.feed_id = market_ref.feed_id;
            position.max_ltv_bps = market_ref.max_ltv_bps;
            position.decimals = MINT_DECIMALS;
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
        let user_key = ctx.accounts.user.key();
        let position = &mut ctx.accounts.position;
        let position_key = position.key();
        let market_ref = &ctx.accounts.market;
        let clock = Clock::get()?;

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
        let user_key = ctx.accounts.user.key();
        let position = &mut ctx.accounts.position;
        let position_key = position.key();
        let market_ref = &ctx.accounts.market;
        let clock = Clock::get()?;

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

#[account]
pub struct Market {
    pub admin: Pubkey,
    pub mint: Pubkey,
    pub amount_per_claim: u64,
    pub max_ltv_bps: u16,
    pub feed_id: [u8; 32],
    pub total_supplied: u64,
    pub total_borrowed: u64,
    pub total_claimed: u64,
    pub claim_count: u64,
    pub bump: u8,
    pub authority_bump: u8,
}

impl Market {
    pub const SIZE: usize = 32 + 32 + 8 + 2 + 32 + 8 + 8 + 8 + 8 + 1 + 1;
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
}

impl Position {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 32 + 2 + 1 + 1;
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
}

#[error_code]
pub enum StatsError {
    #[msg("Stats account does not belong to this signer.")]
    Unauthorized,
    #[msg("Arithmetic overflow.")]
    Overflow,
}
