use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token_2022::close_account,
    token_interface::{Mint, TokenAccount, TokenInterface},
};
use solana_program::{program::invoke, system_instruction};
use spl_token_2022::onchain::invoke_transfer_checked;
use std::convert::TryFrom;

use crate::{
    constants::*,
    errors::MMMErrorCode,
    instructions::{
        get_sell_fulfill_pool_price_info, log_pool, try_close_pool, try_close_sell_state,
        PoolPriceInfo,
    },
    state::{Pool, SellState},
    util::check_allowlists_for_mint_ext,
    SolFulfillSellArgs,
};

// ExtSolFulfillSell means a buyer wants to buy NFTs from the pool
// where the pool has some sellside asset liquidity. Therefore,
// the buyer expects to pay a max_payment_amount for the asset_amount
// that the buyer wants to buy.
// This is mainly used for Token22 extension
#[derive(Accounts)]
#[instruction(args:SolFulfillSellArgs)]
pub struct ExtSolFulfillSell<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: we will check the owner field that matches the pool owner
    #[account(mut)]
    pub owner: UncheckedAccount<'info>,
    pub cosigner: Signer<'info>,
    /// CHECK: we will check that the referral matches the pool's referral
    #[account(mut)]
    pub referral: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [POOL_PREFIX.as_bytes(), owner.key().as_ref(), pool.uuid.as_ref()],
        has_one = owner @ MMMErrorCode::InvalidOwner,
        has_one = referral @ MMMErrorCode::InvalidReferral,
        has_one = cosigner @ MMMErrorCode::InvalidCosigner,
        constraint = pool.payment_mint.eq(&Pubkey::default()) @ MMMErrorCode::InvalidPaymentMint,
        constraint = pool.expiry == 0 || pool.expiry > Clock::get().unwrap().unix_timestamp @ MMMErrorCode::Expired,
        constraint = args.buyside_creator_royalty_bp <= 10000 @ MMMErrorCode::InvalidBP,
        bump
    )]
    pub pool: Box<Account<'info, Pool>>,
    /// CHECK: it's a pda, and the private key is owned by the seeds
    #[account(
        mut,
        seeds = [BUYSIDE_SOL_ESCROW_ACCOUNT_PREFIX.as_bytes(), pool.key().as_ref()],
        bump,
    )]
    pub buyside_sol_escrow_account: AccountInfo<'info>,
    /// CHECK: check_allowlists_for_mint_ext
    pub asset_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        associated_token::mint = asset_mint,
        associated_token::authority = pool,
        associated_token::token_program = token_program,
    )]
    pub sellside_escrow_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = asset_mint,
        associated_token::authority = payer,
        associated_token::token_program = token_program,
    )]
    pub payer_asset_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [
            SELL_STATE_PREFIX.as_bytes(),
            pool.key().as_ref(),
            asset_mint.key().as_ref(),
        ],
        bump
    )]
    pub sell_state: Account<'info, SellState>,
    pub system_program: Program<'info, System>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

pub fn handler<'info>(
    ctx: Context<'_, '_, '_, 'info, ExtSolFulfillSell<'info>>,
    args: SolFulfillSellArgs,
) -> Result<()> {
    let token_program = &ctx.accounts.token_program;
    let system_program = &ctx.accounts.system_program;
    let owner = &ctx.accounts.owner;
    let referral = &ctx.accounts.referral;
    let pool = &mut ctx.accounts.pool;
    let sell_state = &mut ctx.accounts.sell_state;

    let payer = &ctx.accounts.payer;
    let payer_asset_account = &ctx.accounts.payer_asset_account;
    let asset_mint = &ctx.accounts.asset_mint;

    let sellside_escrow_token_account = &ctx.accounts.sellside_escrow_token_account;
    let buyside_sol_escrow_account = &ctx.accounts.buyside_sol_escrow_account;
    let pool_seeds: &[&[&[u8]]] = &[&[
        POOL_PREFIX.as_bytes(),
        pool.owner.as_ref(),
        pool.uuid.as_ref(),
        &[ctx.bumps.pool],
    ]];

    check_allowlists_for_mint_ext(
        &pool.allowlists,
        &asset_mint.to_account_info(),
        args.allowlist_aux,
    )?;

    let PoolPriceInfo {
        total_price,
        next_price,
        lp_fee,
        maker_fee,
        taker_fee,
        referral_fee,
        transfer_sol_to,
    } = get_sell_fulfill_pool_price_info(
        pool,
        owner,
        buyside_sol_escrow_account,
        args.asset_amount,
        args.maker_fee_bp,
        args.taker_fee_bp,
    )?;

    // TODO: make sure that the lp fee is paid with the correct amount
    invoke(
        &system_instruction::transfer(
            payer.key,
            transfer_sol_to.key,
            u64::try_from(
                i64::try_from(total_price)
                    .map_err(|_| MMMErrorCode::NumericOverflow)?
                    .checked_sub(maker_fee)
                    .ok_or(MMMErrorCode::NumericOverflow)?,
            )
            .map_err(|_| MMMErrorCode::NumericOverflow)?,
        ),
        &[
            payer.to_account_info(),
            transfer_sol_to,
            system_program.to_account_info(),
        ],
    )?;

    invoke_transfer_checked(
        token_program.key,
        sellside_escrow_token_account.to_account_info(),
        asset_mint.to_account_info(),
        payer_asset_account.to_account_info(),
        pool.to_account_info(),
        &[],
        args.asset_amount,
        0,
        pool_seeds,
    )?;

    // we can close the sellside_escrow_token_account if no amount left
    if sellside_escrow_token_account.amount == args.asset_amount {
        close_account(CpiContext::new_with_signer(
            token_program.to_account_info(),
            anchor_spl::token_2022::CloseAccount {
                account: sellside_escrow_token_account.to_account_info(),
                destination: owner.to_account_info(),
                authority: pool.to_account_info(),
            },
            pool_seeds,
        ))?;
    }

    if lp_fee > 0 {
        invoke(
            &system_instruction::transfer(payer.key, owner.key, lp_fee),
            &[
                payer.to_account_info(),
                owner.to_account_info(),
                system_program.to_account_info(),
            ],
        )?;
    }

    if referral_fee > 0 {
        invoke(
            &system_instruction::transfer(payer.key, referral.key, referral_fee),
            &[
                payer.to_account_info(),
                referral.to_account_info(),
                system_program.to_account_info(),
            ],
        )?;
    }

    pool.spot_price = next_price;
    pool.sellside_asset_amount = pool
        .sellside_asset_amount
        .checked_sub(args.asset_amount)
        .ok_or(MMMErrorCode::NumericOverflow)?;
    pool.lp_fee_earned = pool
        .lp_fee_earned
        .checked_add(lp_fee)
        .ok_or(MMMErrorCode::NumericOverflow)?;

    // prevent frontrun by pool config changes
    let payment_amount = total_price
        .checked_add(lp_fee)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_add(taker_fee as u64)
        .ok_or(MMMErrorCode::NumericOverflow)?;
    if payment_amount > args.max_payment_amount {
        return Err(MMMErrorCode::InvalidRequestedPrice.into());
    }

    sell_state.asset_amount = sell_state
        .asset_amount
        .checked_sub(args.asset_amount)
        .ok_or(MMMErrorCode::NumericOverflow)?;
    try_close_sell_state(sell_state, owner.to_account_info())?;

    pool.buyside_payment_amount = buyside_sol_escrow_account.lamports();
    log_pool("ext_post_sol_fulfill_sell", pool)?;
    try_close_pool(pool, owner.to_account_info())?;

    msg!("{{\"lp_fee\":{},\"total_price\":{}}}", lp_fee, total_price);

    Ok(())
}
