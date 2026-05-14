use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount, Transfer};

declare_id!("5jqXbgExBEnKPahsQineFmMJHNcEvwnniiYvDy81bZCF");

pub const MARKET_SEED: &[u8] = b"market";
pub const AUTH_SEED: &[u8] = b"auth";
pub const VAULT_SEED: &[u8] = b"vault";
pub const CLAIM_SEED: &[u8] = b"claim";
pub const POSITION_SEED: &[u8] = b"position";
pub const STATS_SEED: &[u8] = b"stats";

pub const BPS_DENOMINATOR: u128 = 10_000;

#[program]
pub mod shadow_lend {
    use super::*;

    /// Admin-only: creates a market — a test mint, its PDA-owned vault, and the
    /// per-market config (faucet amount + max LTV). One call per asset.
    pub fn initialize_market(
        ctx: Context<InitializeMarket>,
        amount_per_claim: u64,
        max_ltv_bps: u16,
    ) -> Result<()> {
        require!(
            max_ltv_bps > 0 && (max_ltv_bps as u128) < BPS_DENOMINATOR,
            MarketError::BadLtv
        );
        let market = &mut ctx.accounts.market;
        market.admin = ctx.accounts.admin.key();
        market.mint = ctx.accounts.mint.key();
        market.vault = ctx.accounts.vault.key();
        market.amount_per_claim = amount_per_claim;
        market.max_ltv_bps = max_ltv_bps;
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

    /// Deposits `amount` of the market token from the caller into the vault.
    pub fn supply(ctx: Context<ModifyPosition>, amount: u64) -> Result<()> {
        require!(amount > 0, MarketError::ZeroAmount);
        let position = &mut ctx.accounts.position;
        if position.user == Pubkey::default() {
            position.user = ctx.accounts.user.key();
            position.market = ctx.accounts.market.key();
            position.bump = ctx.bumps.position;
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

    /// Withdraws supplied collateral, as long as the position stays within LTV.
    pub fn withdraw(ctx: Context<ModifyPosition>, amount: u64) -> Result<()> {
        require!(amount > 0, MarketError::ZeroAmount);
        let position = &mut ctx.accounts.position;
        let max_ltv_bps = ctx.accounts.market.max_ltv_bps;
        let authority_bump = ctx.accounts.market.authority_bump;
        let mint_key = ctx.accounts.market.mint;

        let remaining = position
            .supplied
            .checked_sub(amount)
            .ok_or(MarketError::InsufficientCollateral)?;
        require!(
            position.borrowed <= max_borrow_for(remaining, max_ltv_bps),
            MarketError::WouldBeUnhealthy
        );

        let signer_seeds: &[&[&[u8]]] =
            &[&[AUTH_SEED, mint_key.as_ref(), &[authority_bump]]];
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

        position.supplied = remaining;
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

    /// Borrows `amount` from the vault against supplied collateral, up to the
    /// market's max LTV.
    pub fn borrow(ctx: Context<ModifyPosition>, amount: u64) -> Result<()> {
        require!(amount > 0, MarketError::ZeroAmount);
        let position = &mut ctx.accounts.position;
        let max_ltv_bps = ctx.accounts.market.max_ltv_bps;
        let authority_bump = ctx.accounts.market.authority_bump;
        let mint_key = ctx.accounts.market.mint;

        let new_borrowed = position
            .borrowed
            .checked_add(amount)
            .ok_or(MarketError::Overflow)?;
        require!(
            new_borrowed <= max_borrow_for(position.supplied, max_ltv_bps),
            MarketError::ExceedsLtv
        );
        require!(
            ctx.accounts.vault.amount >= amount,
            MarketError::InsufficientLiquidity
        );

        let signer_seeds: &[&[&[u8]]] =
            &[&[AUTH_SEED, mint_key.as_ref(), &[authority_bump]]];
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

    /// Repays outstanding debt by returning tokens to the vault.
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

/// Largest debt a position with `supplied` collateral may carry at `ltv_bps`.
fn max_borrow_for(supplied: u64, ltv_bps: u16) -> u64 {
    ((supplied as u128) * (ltv_bps as u128) / BPS_DENOMINATOR) as u64
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

/// Shared account context for supply / withdraw / borrow / repay.
#[derive(Accounts)]
pub struct ModifyPosition<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [MARKET_SEED, mint.key().as_ref()],
        bump = market.bump,
        has_one = mint @ MarketError::MintMismatch,
        has_one = vault @ MarketError::VaultMismatch
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
    pub vault: Pubkey,
    pub amount_per_claim: u64,
    pub max_ltv_bps: u16,
    pub total_supplied: u64,
    pub total_borrowed: u64,
    pub total_claimed: u64,
    pub claim_count: u64,
    pub bump: u8,
    pub authority_bump: u8,
}

impl Market {
    pub const SIZE: usize = 32 + 32 + 32 + 8 + 2 + 8 + 8 + 8 + 8 + 1 + 1;
}

#[account]
pub struct Position {
    pub user: Pubkey,
    pub market: Pubkey,
    pub supplied: u64,
    pub borrowed: u64,
    pub bump: u8,
}

impl Position {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 1;
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
    #[msg("Vault account does not match the market config.")]
    VaultMismatch,
    #[msg("Max LTV must be between 1 and 9999 bps.")]
    BadLtv,
    #[msg("Amount must be greater than zero.")]
    ZeroAmount,
    #[msg("Borrow would exceed the market's max LTV.")]
    ExceedsLtv,
    #[msg("Withdrawing this much would leave the position undercollateralized.")]
    WouldBeUnhealthy,
    #[msg("Not enough supplied collateral.")]
    InsufficientCollateral,
    #[msg("Vault does not have enough liquidity for this borrow.")]
    InsufficientLiquidity,
    #[msg("Position has no outstanding debt to repay.")]
    NothingToRepay,
    #[msg("Arithmetic overflow.")]
    Overflow,
}

#[error_code]
pub enum StatsError {
    #[msg("Stats account does not belong to this signer.")]
    Unauthorized,
    #[msg("Arithmetic overflow.")]
    Overflow,
}
