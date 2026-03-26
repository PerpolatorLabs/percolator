use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
    system_instruction,
};
use anchor_spl::token::{self, CloseAccount, SyncNative, Token, TokenAccount, Transfer};
use anchor_spl::associated_token::AssociatedToken;

declare_id!("MEMELPhk8VXcqAPzY9ooyHGQKbTLpEkpavJ2XQJSsP3");

// ============================================================================
// Constants — percolator-prog slab binary offsets (verified from slab.ts)
// ============================================================================

const PERCOLATOR_PROG_ID: Pubkey =
    pubkey!("DP2EbA2v6rmkmNieZpnjumXosuXQ93r9jyb9eSzzkf1x");

/// Native SOL mint (wSOL)
const WSOL_MINT: Pubkey =
    pubkey!("So11111111111111111111111111111111111111112");

// Slab layout
const ENGINE_OFF: usize = 392;        // HEADER(72) + CONFIG(320)
const ENGINE_ACCOUNTS_OFF: usize = 9136;
const CONFIG_OFFSET: usize = 72;

// Account layout (240 bytes each)
const ACCOUNT_SIZE: usize = 240;
const ACCT_ACCOUNT_ID_OFF: usize = 0;  // u64
const ACCT_CAPITAL_OFF: usize = 8;     // u128
const ACCT_KIND_OFF: usize = 24;       // u8 (0=User, 1=LP)
const ACCT_PNL_OFF: usize = 32;        // i128
const ACCT_OWNER_OFF: usize = 184;     // Pubkey (32 bytes)

// Config offsets (relative to CONFIG_OFFSET)
const CONFIG_COLLATERAL_MINT_OFF: usize = 0;   // abs 72
const CONFIG_VAULT_PUBKEY_OFF: usize = 32;      // abs 104
const CONFIG_UNIT_SCALE_OFF: usize = 108;       // abs 180

const ACCOUNT_KIND_LP: u8 = 1;
const INIT_LP_TAG: u8 = 2;
const DEPOSIT_COLLATERAL_TAG: u8 = 3;
const WITHDRAW_COLLATERAL_TAG: u8 = 4;

/// 1% fee on deposits and withdrawals (100 basis points)
const VAULT_FEE_BPS: u64 = 100;

// ============================================================================
// Program
// ============================================================================

#[program]
pub mod vault_lp {
    use super::*;

    /// Initialize a vault for a specific market (slab).
    /// Admin-only, called once per market.
    /// Pre-requisite: matcher context must already be initialized.
    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        expected_lp_idx: u16,
        matcher_program: Pubkey,
        matcher_context: Pubkey,
        fee_lamports: u64,
    ) -> Result<()> {
        // Validate accounts moved to AccountInfo for stack savings
        require!(
            ctx.accounts.collateral_mint.key() == WSOL_MINT,
            VaultError::InvalidCollateralMint
        );
        require!(
            ctx.accounts.percolator_program.key() == PERCOLATOR_PROG_ID,
            VaultError::InvalidPercolatorProgram
        );
        require!(
            *ctx.accounts.slab.owner == PERCOLATOR_PROG_ID,
            VaultError::InvalidSlabOwner
        );

        let vault = &mut ctx.accounts.vault_state;
        let slab_key = ctx.accounts.slab.key();

        // Read config from slab
        let slab_data = ctx.accounts.slab.try_borrow_data()?;
        require!(slab_data.len() > ENGINE_OFF + ENGINE_ACCOUNTS_OFF, VaultError::SlabTooSmall);

        let unit_scale = read_u32_le(&slab_data, CONFIG_OFFSET + CONFIG_UNIT_SCALE_OFF);
        let vault_pubkey = read_pubkey(&slab_data, CONFIG_OFFSET + CONFIG_VAULT_PUBKEY_OFF);
        let collateral_mint = read_pubkey(&slab_data, CONFIG_OFFSET + CONFIG_COLLATERAL_MINT_OFF);

        require!(collateral_mint == WSOL_MINT, VaultError::InvalidCollateralMint);
        require!(
            vault_pubkey == ctx.accounts.percolator_vault.key(),
            VaultError::InvalidPercolatorVault
        );
        drop(slab_data);

        // ---- SOL wrap for InitLP fee ----
        if fee_lamports > 0 {
            anchor_lang::solana_program::program::invoke(
                &system_instruction::transfer(
                    &ctx.accounts.admin.key(),
                    &ctx.accounts.vault_wsol_ata.key(),
                    fee_lamports,
                ),
                &[
                    ctx.accounts.admin.to_account_info(),
                    ctx.accounts.vault_wsol_ata.to_account_info(),
                    ctx.accounts.system_program.to_account_info(),
                ],
            )?;
            token::sync_native(CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                SyncNative { account: ctx.accounts.vault_wsol_ata.to_account_info() },
            ))?;
        }

        // ---- CPI: InitLP on percolator-prog ----
        let mut ix_data = Vec::with_capacity(73);
        ix_data.push(INIT_LP_TAG);
        ix_data.extend_from_slice(&matcher_program.to_bytes());
        ix_data.extend_from_slice(&matcher_context.to_bytes());
        ix_data.extend_from_slice(&fee_lamports.to_le_bytes());

        let ix = Instruction {
            program_id: PERCOLATOR_PROG_ID,
            accounts: vec![
                AccountMeta::new_readonly(vault.key(), true),
                AccountMeta::new(slab_key, false),
                AccountMeta::new(ctx.accounts.vault_wsol_ata.key(), false),
                AccountMeta::new(ctx.accounts.percolator_vault.key(), false),
                AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
            ],
            data: ix_data,
        };

        let bump = ctx.bumps.vault_state;
        let seeds: &[&[u8]] = &[b"vault_lp", slab_key.as_ref(), &[bump]];
        invoke_signed(
            &ix,
            &[
                vault.to_account_info(),
                ctx.accounts.slab.to_account_info(),
                ctx.accounts.vault_wsol_ata.to_account_info(),
                ctx.accounts.percolator_vault.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            &[seeds],
        )?;

        // ---- Post-CPI: verify LP index ----
        let slab_data = ctx.accounts.slab.try_borrow_data()?;
        let lp_base = ENGINE_OFF + ENGINE_ACCOUNTS_OFF + (expected_lp_idx as usize * ACCOUNT_SIZE);
        require!(slab_data.len() >= lp_base + ACCOUNT_SIZE, VaultError::SlabTooSmall);

        let kind = slab_data[lp_base + ACCT_KIND_OFF];
        require!(kind == ACCOUNT_KIND_LP, VaultError::LpIndexNotLp);

        let owner = read_pubkey(&slab_data, lp_base + ACCT_OWNER_OFF);
        require!(owner == vault.key(), VaultError::LpOwnerMismatch);

        let lp_account_id = read_u64_le(&slab_data, lp_base + ACCT_ACCOUNT_ID_OFF);
        drop(slab_data);

        // ---- Save vault state ----
        vault.slab = slab_key;
        vault.lp_idx = expected_lp_idx;
        vault.lp_account_id = lp_account_id;
        vault.unit_scale = unit_scale;
        vault.total_shares = 0;
        vault.admin = ctx.accounts.admin.key();
        vault.bump = bump;
        vault.matcher_program = matcher_program;
        vault.matcher_context = matcher_context;
        vault.percolator_program = PERCOLATOR_PROG_ID;
        vault.collateral_mint = WSOL_MINT;
        vault.vault_pubkey = vault_pubkey;

        msg!("Vault initialized: slab={}, lp_idx={}", slab_key, expected_lp_idx);
        Ok(())
    }

    /// Deposit native SOL into the vault, receive shares.
    /// Anyone can call. SOL is automatically wrapped to wSOL internally.
    /// A 1% fee is deducted and sent to the vault admin.
    pub fn deposit(ctx: Context<Deposit>, amount_lamports: u64) -> Result<()> {
        require!(amount_lamports > 0, VaultError::ZeroAmount);

        // ---- Calculate fee ----
        let fee = amount_lamports
            .checked_mul(VAULT_FEE_BPS)
            .ok_or(VaultError::MathOverflow)?
            / 10_000;
        let net_amount = amount_lamports
            .checked_sub(fee)
            .ok_or(VaultError::MathOverflow)?;
        require!(net_amount > 0, VaultError::DepositTooSmall);

        let vault = &ctx.accounts.vault_state;
        let admin_key = vault.admin;
        let slab_key = vault.slab;
        let lp_idx = vault.lp_idx;
        let bump = vault.bump;
        let total_shares = vault.total_shares;

        // ---- Read vault value from slab ----
        let slab_data = ctx.accounts.slab.try_borrow_data()?;
        let (capital, pnl) = read_lp_capital_pnl(&slab_data, lp_idx)?;
        drop(slab_data);

        let vault_value: i128 = (capital as i128) + pnl;

        // ---- Calculate shares to issue (based on net amount after fee) ----
        let new_shares: u128 = if total_shares == 0 {
            net_amount as u128
        } else {
            require!(vault_value > 0, VaultError::VaultDepleted);
            let result = (net_amount as u128)
                .checked_mul(total_shares)
                .ok_or(VaultError::MathOverflow)?
                / (vault_value as u128);
            require!(result > 0, VaultError::DepositTooSmall);
            result
        };

        // ---- Fee: depositor → admin (native SOL) ----
        if fee > 0 {
            require!(
                ctx.accounts.admin.key() == admin_key,
                VaultError::AdminMismatch
            );
            anchor_lang::solana_program::program::invoke(
                &system_instruction::transfer(
                    &ctx.accounts.depositor.key(),
                    &ctx.accounts.admin.key(),
                    fee,
                ),
                &[
                    ctx.accounts.depositor.to_account_info(),
                    ctx.accounts.admin.to_account_info(),
                    ctx.accounts.system_program.to_account_info(),
                ],
            )?;
        }

        // ---- SOL Wrap: user → vault_wsol_ata (net amount only) ----
        anchor_lang::solana_program::program::invoke(
            &system_instruction::transfer(
                &ctx.accounts.depositor.key(),
                &ctx.accounts.vault_wsol_ata.key(),
                net_amount,
            ),
            &[
                ctx.accounts.depositor.to_account_info(),
                ctx.accounts.vault_wsol_ata.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
        )?;
        token::sync_native(CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            SyncNative { account: ctx.accounts.vault_wsol_ata.to_account_info() },
        ))?;

        // ---- CPI: DepositCollateral (net amount) ----
        let mut ix_data = Vec::with_capacity(11);
        ix_data.push(DEPOSIT_COLLATERAL_TAG);
        ix_data.extend_from_slice(&lp_idx.to_le_bytes());
        ix_data.extend_from_slice(&net_amount.to_le_bytes());

        let ix = Instruction {
            program_id: PERCOLATOR_PROG_ID,
            accounts: vec![
                AccountMeta::new_readonly(ctx.accounts.vault_state.key(), true),
                AccountMeta::new(ctx.accounts.slab.key(), false),
                AccountMeta::new(ctx.accounts.vault_wsol_ata.key(), false),
                AccountMeta::new(ctx.accounts.percolator_vault.key(), false),
                AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
                AccountMeta::new_readonly(ctx.accounts.clock.key(), false),
            ],
            data: ix_data,
        };

        let seeds: &[&[u8]] = &[b"vault_lp", slab_key.as_ref(), &[bump]];
        invoke_signed(
            &ix,
            &[
                ctx.accounts.vault_state.to_account_info(),
                ctx.accounts.slab.to_account_info(),
                ctx.accounts.vault_wsol_ata.to_account_info(),
                ctx.accounts.percolator_vault.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
                ctx.accounts.clock.to_account_info(),
            ],
            &[seeds],
        )?;

        // ---- Update shares ----
        let position = &mut ctx.accounts.user_position;
        if position.shares == 0 && position.vault == Pubkey::default() {
            // First time init
            position.vault = ctx.accounts.vault_state.key();
            position.owner = ctx.accounts.depositor.key();
            position.bump = ctx.bumps.user_position;
        }
        // After vault reset: if user has stale shares from before reset, zero them
        if total_shares == 0 && position.shares > 0 {
            position.shares = 0;
        }
        position.shares = position.shares
            .checked_add(new_shares)
            .ok_or(VaultError::MathOverflow)?;

        let vault = &mut ctx.accounts.vault_state;
        vault.total_shares = vault.total_shares
            .checked_add(new_shares)
            .ok_or(VaultError::MathOverflow)?;

        msg!(
            "Deposited {} lamports (fee={}, net={}), {} new shares (total_shares={}, vault_value={})",
            amount_lamports, fee, net_amount, new_shares, vault.total_shares, vault_value
        );
        Ok(())
    }

    /// Withdraw native SOL by burning shares.
    /// Anyone can call. wSOL is automatically unwrapped to native SOL.
    /// A 1% fee is deducted and sent to the vault admin.
    pub fn withdraw(ctx: Context<Withdraw>, shares_to_burn: u128) -> Result<()> {
        require!(shares_to_burn > 0, VaultError::ZeroAmount);

        // Invalidate stale shares from before vault reset
        let position = &mut ctx.accounts.user_position;
        let total = ctx.accounts.vault_state.total_shares;
        if total > 0 && position.shares > total {
            position.shares = 0;
            return Err(VaultError::StaleShares.into());
        }
        require!(position.shares >= shares_to_burn, VaultError::InsufficientShares);

        let vault = &ctx.accounts.vault_state;
        let admin_key = vault.admin;
        let slab_key = vault.slab;
        let lp_idx = vault.lp_idx;
        let bump = vault.bump;
        let total_shares = vault.total_shares;
        require!(total_shares > 0, VaultError::VaultDepleted);

        // ---- Read vault value from slab ----
        let slab_data = ctx.accounts.slab.try_borrow_data()?;
        let (capital, pnl) = read_lp_capital_pnl(&slab_data, lp_idx)?;
        drop(slab_data);

        let vault_value: i128 = (capital as i128) + pnl;
        require!(vault_value > 0, VaultError::VaultDepleted);

        // ---- Calculate gross withdrawal, then deduct fee ----
        let gross_withdraw: u64 = {
            let result = (shares_to_burn as u128)
                .checked_mul(vault_value as u128)
                .ok_or(VaultError::MathOverflow)?
                / total_shares;
            require!(result > 0, VaultError::WithdrawTooSmall);
            result as u64
        };

        let fee = gross_withdraw
            .checked_mul(VAULT_FEE_BPS)
            .ok_or(VaultError::MathOverflow)?
            / 10_000;
        let withdraw_lamports = gross_withdraw
            .checked_sub(fee)
            .ok_or(VaultError::MathOverflow)?;
        require!(withdraw_lamports > 0, VaultError::WithdrawTooSmall);

        // ---- Update shares FIRST (before CPI, reentrancy protection) ----
        let position = &mut ctx.accounts.user_position;
        position.shares = position.shares
            .checked_sub(shares_to_burn)
            .ok_or(VaultError::InsufficientShares)?;

        let vault = &mut ctx.accounts.vault_state;
        vault.total_shares = vault.total_shares
            .checked_sub(shares_to_burn)
            .ok_or(VaultError::MathOverflow)?;

        // ---- CPI: WithdrawCollateral (gross amount — full amount from percolator) ----
        let mut ix_data = Vec::with_capacity(11);
        ix_data.push(WITHDRAW_COLLATERAL_TAG);
        ix_data.extend_from_slice(&lp_idx.to_le_bytes());
        ix_data.extend_from_slice(&gross_withdraw.to_le_bytes());

        let ix = Instruction {
            program_id: PERCOLATOR_PROG_ID,
            accounts: vec![
                AccountMeta::new_readonly(ctx.accounts.vault_state.key(), true),
                AccountMeta::new(ctx.accounts.slab.key(), false),
                AccountMeta::new(ctx.accounts.percolator_vault.key(), false),
                AccountMeta::new(ctx.accounts.vault_wsol_ata.key(), false),
                AccountMeta::new_readonly(ctx.accounts.percolator_vault_pda.key(), false),
                AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
                AccountMeta::new_readonly(ctx.accounts.clock.key(), false),
                AccountMeta::new_readonly(ctx.accounts.oracle.key(), false),
            ],
            data: ix_data,
        };

        let seeds: &[&[u8]] = &[b"vault_lp", slab_key.as_ref(), &[bump]];
        invoke_signed(
            &ix,
            &[
                ctx.accounts.vault_state.to_account_info(),
                ctx.accounts.slab.to_account_info(),
                ctx.accounts.percolator_vault.to_account_info(),
                ctx.accounts.vault_wsol_ata.to_account_info(),
                ctx.accounts.percolator_vault_pda.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
                ctx.accounts.clock.to_account_info(),
                ctx.accounts.oracle.to_account_info(),
            ],
            &[seeds],
        )?;

        // ---- Fee: transfer fee portion (wSOL) to admin's wSOL ATA ----
        if fee > 0 {
            require!(
                ctx.accounts.admin_wsol_ata.key() == anchor_spl::associated_token::get_associated_token_address(&admin_key, &WSOL_MINT),
                VaultError::AdminMismatch
            );
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault_wsol_ata.to_account_info(),
                        to: ctx.accounts.admin_wsol_ata.to_account_info(),
                        authority: ctx.accounts.vault_state.to_account_info(),
                    },
                    &[seeds],
                ),
                fee,
            )?;
        }

        // ---- Unwrap: wSOL → native SOL (net amount to user) ----
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault_wsol_ata.to_account_info(),
                    to: ctx.accounts.withdrawer_wsol_ata.to_account_info(),
                    authority: ctx.accounts.vault_state.to_account_info(),
                },
                &[seeds],
            ),
            withdraw_lamports,
        )?;

        // Close user's wSOL ATA → native SOL refunded to withdrawer
        token::close_account(CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.withdrawer_wsol_ata.to_account_info(),
                destination: ctx.accounts.withdrawer.to_account_info(),
                authority: ctx.accounts.withdrawer.to_account_info(),
            },
        ))?;

        msg!(
            "Withdrew {} shares → {} lamports (fee={}, net={}, total_shares={}, vault_value={})",
            shares_to_burn, gross_withdraw, fee, withdraw_lamports, ctx.accounts.vault_state.total_shares, vault_value
        );
        Ok(())
    }

    /// Reset a depleted vault so it can accept new deposits.
    /// Admin-only. Zeroes total_shares when vault value is 0 (LP capital + PnL <= 0).
    /// This allows the vault to restart from scratch after being depleted.
    /// Admin-only reset: zeroes total_shares so vault can restart.
    /// Use after force-realize or when vault is in inconsistent state.
    pub fn reset_vault(ctx: Context<ResetVault>) -> Result<()> {
        // Only allow reset when vault value is depleted/non-positive.
        let slab_data = ctx.accounts.slab.try_borrow_data()?;
        let (capital, pnl) = read_lp_capital_pnl(&slab_data, ctx.accounts.vault_state.lp_idx)?;
        drop(slab_data);
        let vault_value: i128 = (capital as i128) + pnl;
        require!(vault_value <= 0, VaultError::VaultNotDepleted);

        let vault = &mut ctx.accounts.vault_state;
        let old_shares = vault.total_shares;
        vault.total_shares = 0;

        msg!(
            "Vault reset: total_shares {} → 0 (vault_value={})",
            old_shares,
            vault_value
        );
        Ok(())
    }

    /// Admin-only: zero out a user's stale shares after vault reset.
    pub fn reset_user_position(ctx: Context<ResetUserPosition>) -> Result<()> {
        let position = &mut ctx.accounts.user_position;
        let old_shares = position.shares;
        position.shares = 0;

        msg!("User position reset: shares {} → 0", old_shares);
        Ok(())
    }

    /// Admin-only: withdraw ALL capital from the vault's LP position.
    /// CPI WithdrawCollateral to percolator-prog, then unwrap wSOL to admin.
    pub fn admin_withdraw_all(ctx: Context<AdminWithdrawAll>) -> Result<()> {
        let vault = &ctx.accounts.vault_state;
        let slab_key = vault.slab;
        let lp_idx = vault.lp_idx;
        let bump = vault.bump;

        // Read LP capital
        let slab_data = ctx.accounts.slab.try_borrow_data()?;
        let (capital, pnl) = read_lp_capital_pnl(&slab_data, lp_idx)?;
        drop(slab_data);

        let vault_value: i128 = (capital as i128) + pnl;
        require!(vault_value > 0, VaultError::VaultDepleted);
        let withdraw_amount = vault_value as u64;

        // CPI: WithdrawCollateral (full amount)
        let mut ix_data = Vec::with_capacity(11);
        ix_data.push(WITHDRAW_COLLATERAL_TAG);
        ix_data.extend_from_slice(&lp_idx.to_le_bytes());
        ix_data.extend_from_slice(&withdraw_amount.to_le_bytes());

        let ix = Instruction {
            program_id: PERCOLATOR_PROG_ID,
            accounts: vec![
                AccountMeta::new_readonly(ctx.accounts.vault_state.key(), true),
                AccountMeta::new(ctx.accounts.slab.key(), false),
                AccountMeta::new(ctx.accounts.percolator_vault.key(), false),
                AccountMeta::new(ctx.accounts.vault_wsol_ata.key(), false),
                AccountMeta::new_readonly(ctx.accounts.percolator_vault_pda.key(), false),
                AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
                AccountMeta::new_readonly(ctx.accounts.clock.key(), false),
                AccountMeta::new_readonly(ctx.accounts.oracle.key(), false),
            ],
            data: ix_data,
        };

        let seeds: &[&[u8]] = &[b"vault_lp", slab_key.as_ref(), &[bump]];
        invoke_signed(
            &ix,
            &[
                ctx.accounts.vault_state.to_account_info(),
                ctx.accounts.slab.to_account_info(),
                ctx.accounts.percolator_vault.to_account_info(),
                ctx.accounts.vault_wsol_ata.to_account_info(),
                ctx.accounts.percolator_vault_pda.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
                ctx.accounts.clock.to_account_info(),
                ctx.accounts.oracle.to_account_info(),
            ],
            &[seeds],
        )?;

        // Transfer wSOL to admin's ATA
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault_wsol_ata.to_account_info(),
                    to: ctx.accounts.admin_wsol_ata.to_account_info(),
                    authority: ctx.accounts.vault_state.to_account_info(),
                },
                &[seeds],
            ),
            withdraw_amount,
        )?;

        // Reset total_shares
        let vault = &mut ctx.accounts.vault_state;
        vault.total_shares = 0;

        msg!("Admin withdrew {} lamports from LP #{}", withdraw_amount, lp_idx);
        Ok(())
    }

    /// Admin-only: close the VaultState PDA and recover rent.
    pub fn close_vault(ctx: Context<CloseVault>) -> Result<()> {
        msg!("Vault closed, rent returned to admin");
        Ok(())
    }
}

// ============================================================================
// Slab reader helpers
// ============================================================================

fn read_lp_capital_pnl(slab_data: &[u8], lp_idx: u16) -> Result<(u128, i128)> {
    let base = ENGINE_OFF + ENGINE_ACCOUNTS_OFF + (lp_idx as usize * ACCOUNT_SIZE);
    require!(slab_data.len() >= base + ACCOUNT_SIZE, VaultError::SlabTooSmall);

    let kind = slab_data[base + ACCT_KIND_OFF];
    require!(kind == ACCOUNT_KIND_LP, VaultError::LpIndexNotLp);

    let capital = u128::from_le_bytes(slab_data[base + ACCT_CAPITAL_OFF..base + ACCT_CAPITAL_OFF + 16].try_into().unwrap());
    let pnl = i128::from_le_bytes(slab_data[base + ACCT_PNL_OFF..base + ACCT_PNL_OFF + 16].try_into().unwrap());

    Ok((capital, pnl))
}

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::new_from_array(data[offset..offset + 32].try_into().unwrap())
}

// ============================================================================
// Accounts
// ============================================================================

#[derive(Accounts)]
pub struct InitializeVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init_if_needed,
        payer = admin,
        space = 8 + VaultState::INIT_SPACE,
        seeds = [b"vault_lp", slab.key().as_ref()],
        bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// Vault PDA's wSOL ATA — transit account for wrapping/unwrapping SOL
    #[account(
        init_if_needed,
        payer = admin,
        associated_token::mint = collateral_mint,
        associated_token::authority = vault_state,
    )]
    pub vault_wsol_ata: Box<Account<'info, TokenAccount>>,

    /// CHECK: percolator-prog slab — validated in handler
    #[account(mut)]
    pub slab: AccountInfo<'info>,

    /// CHECK: percolator-prog vault token account
    #[account(mut)]
    pub percolator_vault: AccountInfo<'info>,

    /// CHECK: wSOL mint — validated in handler
    pub collateral_mint: AccountInfo<'info>,

    /// CHECK: percolator-prog program — validated in handler
    pub percolator_program: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        mut,
        seeds = [b"vault_lp", vault_state.slab.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// User's position in this vault (init_if_needed on first deposit)
    #[account(
        init_if_needed,
        payer = depositor,
        space = 8 + UserPosition::INIT_SPACE,
        seeds = [b"position", vault_state.key().as_ref(), depositor.key().as_ref()],
        bump,
    )]
    pub user_position: Account<'info, UserPosition>,

    /// CHECK: percolator-prog slab
    #[account(
        mut,
        constraint = slab.key() == vault_state.slab @ VaultError::SlabMismatch
    )]
    pub slab: AccountInfo<'info>,

    /// Vault PDA's wSOL ATA
    #[account(
        mut,
        constraint = vault_wsol_ata.key() == anchor_spl::associated_token::get_associated_token_address(&vault_state.key(), &WSOL_MINT) @ VaultError::InvalidVaultAta
    )]
    pub vault_wsol_ata: Box<Account<'info, TokenAccount>>,

    /// CHECK: percolator-prog vault token account
    #[account(
        mut,
        constraint = percolator_vault.key() == vault_state.vault_pubkey @ VaultError::InvalidPercolatorVault
    )]
    pub percolator_vault: AccountInfo<'info>,

    /// CHECK: percolator-prog program
    #[account(
        constraint = percolator_program.key() == PERCOLATOR_PROG_ID @ VaultError::InvalidPercolatorProgram
    )]
    pub percolator_program: AccountInfo<'info>,

    /// CHECK: admin account to receive deposit fee (must match vault_state.admin)
    #[account(
        mut,
        constraint = admin.key() == vault_state.admin @ VaultError::AdminMismatch
    )]
    pub admin: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub clock: Sysvar<'info, Clock>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub withdrawer: Signer<'info>,

    #[account(
        mut,
        seeds = [b"vault_lp", vault_state.slab.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// User's position in this vault
    #[account(
        mut,
        seeds = [b"position", vault_state.key().as_ref(), withdrawer.key().as_ref()],
        bump = user_position.bump,
        constraint = user_position.owner == withdrawer.key() @ VaultError::PositionOwnerMismatch,
    )]
    pub user_position: Account<'info, UserPosition>,

    /// CHECK: percolator-prog slab
    #[account(
        mut,
        constraint = slab.key() == vault_state.slab @ VaultError::SlabMismatch
    )]
    pub slab: AccountInfo<'info>,

    /// Withdrawer's temporary wSOL ATA (init_if_needed, will be closed to unwrap)
    #[account(
        init_if_needed,
        payer = withdrawer,
        associated_token::mint = collateral_mint,
        associated_token::authority = withdrawer,
    )]
    pub withdrawer_wsol_ata: Box<Account<'info, TokenAccount>>,

    /// Vault PDA's wSOL ATA
    #[account(
        mut,
        constraint = vault_wsol_ata.key() == anchor_spl::associated_token::get_associated_token_address(&vault_state.key(), &WSOL_MINT) @ VaultError::InvalidVaultAta
    )]
    pub vault_wsol_ata: Box<Account<'info, TokenAccount>>,

    /// CHECK: percolator-prog vault token account
    #[account(
        mut,
        constraint = percolator_vault.key() == vault_state.vault_pubkey @ VaultError::InvalidPercolatorVault
    )]
    pub percolator_vault: AccountInfo<'info>,

    /// CHECK: percolator-prog vault authority PDA (seeds: ["vault", slab])
    pub percolator_vault_pda: AccountInfo<'info>,

    /// CHECK: percolator-prog program
    #[account(
        constraint = percolator_program.key() == PERCOLATOR_PROG_ID @ VaultError::InvalidPercolatorProgram
    )]
    pub percolator_program: AccountInfo<'info>,

    /// CHECK: wSOL mint
    #[account(address = WSOL_MINT @ VaultError::InvalidCollateralMint)]
    pub collateral_mint: AccountInfo<'info>,

    /// CHECK: oracle price account for percolator-prog WithdrawCollateral
    pub oracle: AccountInfo<'info>,

    /// CHECK: admin's wSOL ATA to receive withdrawal fee
    #[account(mut)]
    pub admin_wsol_ata: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub clock: Sysvar<'info, Clock>,
}

#[derive(Accounts)]
pub struct ResetVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [b"vault_lp", vault_state.slab.as_ref()],
        bump = vault_state.bump,
        constraint = vault_state.admin == admin.key() @ VaultError::AdminMismatch,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// CHECK: percolator-prog slab (read-only to check LP value)
    #[account(
        constraint = slab.key() == vault_state.slab @ VaultError::SlabMismatch
    )]
    pub slab: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct ResetUserPosition<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        seeds = [b"vault_lp", vault_state.slab.as_ref()],
        bump = vault_state.bump,
        constraint = vault_state.admin == admin.key() @ VaultError::AdminMismatch,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// The user position to reset (any user)
    #[account(
        mut,
        constraint = user_position.vault == vault_state.key() @ VaultError::SlabMismatch,
    )]
    pub user_position: Account<'info, UserPosition>,
}

#[derive(Accounts)]
pub struct AdminWithdrawAll<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [b"vault_lp", vault_state.slab.as_ref()],
        bump = vault_state.bump,
        constraint = vault_state.admin == admin.key() @ VaultError::AdminMismatch,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// CHECK: percolator-prog slab
    #[account(mut, constraint = slab.key() == vault_state.slab @ VaultError::SlabMismatch)]
    pub slab: AccountInfo<'info>,

    /// Vault PDA's wSOL ATA
    #[account(
        mut,
        constraint = vault_wsol_ata.key() == anchor_spl::associated_token::get_associated_token_address(&vault_state.key(), &WSOL_MINT) @ VaultError::InvalidVaultAta
    )]
    pub vault_wsol_ata: Box<Account<'info, TokenAccount>>,

    /// CHECK: percolator-prog vault token account
    #[account(mut, constraint = percolator_vault.key() == vault_state.vault_pubkey @ VaultError::InvalidPercolatorVault)]
    pub percolator_vault: AccountInfo<'info>,

    /// CHECK: percolator-prog vault authority PDA
    pub percolator_vault_pda: AccountInfo<'info>,

    /// CHECK: percolator-prog program
    #[account(constraint = percolator_program.key() == PERCOLATOR_PROG_ID @ VaultError::InvalidPercolatorProgram)]
    pub percolator_program: AccountInfo<'info>,

    /// CHECK: oracle
    pub oracle: AccountInfo<'info>,

    /// Admin's wSOL ATA (receives withdrawn funds)
    #[account(mut)]
    pub admin_wsol_ata: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
}

#[derive(Accounts)]
pub struct CloseVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        close = admin,
        seeds = [b"vault_lp", vault_state.slab.as_ref()],
        bump = vault_state.bump,
        constraint = vault_state.admin == admin.key() @ VaultError::AdminMismatch,
        constraint = vault_state.total_shares == 0 @ VaultError::VaultNotDepleted,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,
}

// ============================================================================
// State
// ============================================================================

#[account]
#[derive(InitSpace)]
pub struct VaultState {
    pub slab: Pubkey,              // 32
    pub lp_idx: u16,               // 2
    pub lp_account_id: u64,        // 8
    pub unit_scale: u32,           // 4
    pub total_shares: u128,        // 16
    pub admin: Pubkey,             // 32
    pub bump: u8,                  // 1
    pub matcher_program: Pubkey,   // 32
    pub matcher_context: Pubkey,   // 32
    pub percolator_program: Pubkey,// 32
    pub collateral_mint: Pubkey,   // 32
    pub vault_pubkey: Pubkey,      // 32
}

#[account]
#[derive(InitSpace)]
pub struct UserPosition {
    pub vault: Pubkey,      // 32
    pub owner: Pubkey,      // 32
    pub shares: u128,       // 16
    pub bump: u8,           // 1
}

// ============================================================================
// Errors
// ============================================================================

#[error_code]
pub enum VaultError {
    #[msg("Slab account too small")]
    SlabTooSmall,
    #[msg("Slab not owned by percolator-prog")]
    InvalidSlabOwner,
    #[msg("Invalid percolator program ID")]
    InvalidPercolatorProgram,
    #[msg("Invalid collateral mint (expected wSOL)")]
    InvalidCollateralMint,
    #[msg("Percolator vault mismatch")]
    InvalidPercolatorVault,
    #[msg("LP at given index is not an LP account")]
    LpIndexNotLp,
    #[msg("LP owner does not match vault PDA")]
    LpOwnerMismatch,
    #[msg("Slab key mismatch")]
    SlabMismatch,
    #[msg("Invalid vault wSOL ATA")]
    InvalidVaultAta,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Vault value is depleted (capital + pnl <= 0)")]
    VaultDepleted,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Deposit too small (would issue 0 shares)")]
    DepositTooSmall,
    #[msg("Withdrawal too small (would receive 0 lamports)")]
    WithdrawTooSmall,
    #[msg("Insufficient shares for withdrawal")]
    InsufficientShares,
    #[msg("Position owner mismatch")]
    PositionOwnerMismatch,
    #[msg("Admin account mismatch")]
    AdminMismatch,
    #[msg("Vault is not depleted (capital + pnl > 0), cannot reset")]
    VaultNotDepleted,
    #[msg("Stale shares from before vault reset, position zeroed")]
    StaleShares,
}