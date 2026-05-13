use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount};

declare_id!("5jqXbgExBEnKPahsQineFmMJHNcEvwnniiYvDy81bZCF");

pub const FAUCET_SEED: &[u8] = b"faucet";
pub const MINT_AUTH_SEED: &[u8] = b"mint-auth";
pub const CLAIM_SEED: &[u8] = b"claim";
pub const STATS_SEED: &[u8] = b"stats";

pub const DEFAULT_CLAIM_AMOUNT: u64 = 10_000 * 1_000_000_000;

#[program]
pub mod shadow_lend {
    use super::*;

    pub fn initialize_faucet(ctx: Context<InitializeFaucet>, amount_per_claim: u64) -> Result<()> {
        let faucet = &mut ctx.accounts.faucet;
        faucet.admin = ctx.accounts.admin.key();
        faucet.mint = ctx.accounts.mint.key();
        faucet.amount_per_claim = amount_per_claim;
        faucet.total_claimed = 0;
        faucet.claim_count = 0;
        faucet.bump = ctx.bumps.faucet;
        faucet.mint_authority_bump = ctx.bumps.mint_authority;
        Ok(())
    }

    pub fn set_claim_amount(ctx: Context<AdminUpdate>, amount: u64) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.faucet.admin,
            ctx.accounts.admin.key(),
            FaucetError::NotAdmin
        );
        ctx.accounts.faucet.amount_per_claim = amount;
        Ok(())
    }

    pub fn claim_faucet(ctx: Context<ClaimFaucet>) -> Result<()> {
        let faucet = &mut ctx.accounts.faucet;
        let receipt = &mut ctx.accounts.receipt;
        let clock = Clock::get()?;

        let amount = faucet.amount_per_claim;
        let authority_bump = faucet.mint_authority_bump;
        let signer_seeds: &[&[&[u8]]] = &[&[MINT_AUTH_SEED, &[authority_bump]]];

        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            MintTo {
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.recipient_ata.to_account_info(),
                authority: ctx.accounts.mint_authority.to_account_info(),
            },
            signer_seeds,
        );
        token::mint_to(cpi_ctx, amount)?;

        receipt.user = ctx.accounts.user.key();
        receipt.amount = amount;
        receipt.claimed_at = clock.unix_timestamp;
        receipt.bump = ctx.bumps.receipt;

        faucet.total_claimed = faucet
            .total_claimed
            .checked_add(amount)
            .ok_or(FaucetError::Overflow)?;
        faucet.claim_count = faucet
            .claim_count
            .checked_add(1)
            .ok_or(FaucetError::Overflow)?;

        emit!(FaucetClaimed {
            user: ctx.accounts.user.key(),
            amount,
            claimed_at: clock.unix_timestamp,
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

#[derive(Accounts)]
pub struct InitializeFaucet<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + FaucetConfig::SIZE,
        seeds = [FAUCET_SEED],
        bump
    )]
    pub faucet: Account<'info, FaucetConfig>,

    #[account(
        init,
        payer = admin,
        mint::decimals = 9,
        mint::authority = mint_authority,
        mint::freeze_authority = mint_authority,
    )]
    pub mint: Account<'info, Mint>,

    /// CHECK: PDA used only as the mint authority for the SL token.
    #[account(seeds = [MINT_AUTH_SEED], bump)]
    pub mint_authority: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct AdminUpdate<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [FAUCET_SEED], bump = faucet.bump)]
    pub faucet: Account<'info, FaucetConfig>,
}

#[derive(Accounts)]
pub struct ClaimFaucet<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [FAUCET_SEED],
        bump = faucet.bump,
        has_one = mint @ FaucetError::MintMismatch
    )]
    pub faucet: Account<'info, FaucetConfig>,

    #[account(mut, address = faucet.mint @ FaucetError::MintMismatch)]
    pub mint: Account<'info, Mint>,

    /// CHECK: PDA mint authority for the SL token.
    #[account(seeds = [MINT_AUTH_SEED], bump = faucet.mint_authority_bump)]
    pub mint_authority: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = user,
        associated_token::mint = mint,
        associated_token::authority = user
    )]
    pub recipient_ata: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = user,
        space = 8 + ClaimReceipt::SIZE,
        seeds = [CLAIM_SEED, user.key().as_ref()],
        bump
    )]
    pub receipt: Account<'info, ClaimReceipt>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
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
    pub stats: Account<'info, UserStats>,

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
    pub stats: Account<'info, UserStats>,
}

#[account]
pub struct FaucetConfig {
    pub admin: Pubkey,
    pub mint: Pubkey,
    pub amount_per_claim: u64,
    pub total_claimed: u64,
    pub claim_count: u64,
    pub bump: u8,
    pub mint_authority_bump: u8,
}

impl FaucetConfig {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 8 + 1 + 1;
}

#[account]
pub struct ClaimReceipt {
    pub user: Pubkey,
    pub amount: u64,
    pub claimed_at: i64,
    pub bump: u8,
}

impl ClaimReceipt {
    pub const SIZE: usize = 32 + 8 + 8 + 1;
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

#[event]
pub struct FaucetClaimed {
    pub user: Pubkey,
    pub amount: u64,
    pub claimed_at: i64,
}

#[event]
pub struct ActionRecorded {
    pub user: Pubkey,
    pub kind: ActionKind,
    pub health_bps: u16,
    pub at: i64,
}

#[error_code]
pub enum FaucetError {
    #[msg("Caller is not the faucet admin.")]
    NotAdmin,
    #[msg("Mint account does not match the faucet config.")]
    MintMismatch,
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
