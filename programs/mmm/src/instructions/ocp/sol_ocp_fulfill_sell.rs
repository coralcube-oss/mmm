use anchor_lang::{prelude::*, solana_program::sysvar, AnchorDeserialize, AnchorSerialize};
use anchor_spl::{
    associated_token::AssociatedToken,
    token::Token,
    token_interface::{Mint, TokenAccount},
};
use open_creator_protocol::state::Policy;
use std::convert::TryFrom;

use crate::{
    ata::init_if_needed_ocp_ata,
    constants::*,
    errors::MMMErrorCode,
    state::{Pool, SellState},
    util::{
        assert_valid_fees_bp, check_allowlists_for_mint, get_metadata_royalty_bp, get_sol_fee,
        get_sol_lp_fee, get_sol_total_price_and_next_price, log_pool, pay_creator_fees_in_sol,
        try_close_pool, try_close_sell_state,
    },
    verify_referral::verify_referral,
};

#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct SolOcpFulfillSellArgs {
    asset_amount: u64,
    max_payment_amount: u64,
    allowlist_aux: Option<String>, // TODO: use it for future allowlist_aux
    maker_fee_bp: i16,             // will be checked by cosigner
    taker_fee_bp: i16,             // will be checked by cosigner
}

// FulfillSell means a buyer wants to buy NFT/SFT from the pool
// where the pool has some sellside asset liquidity. Therefore,
// the buyer expects to pay a max_payment_amount for the asset_amount
// that the buyer wants to buy.
#[derive(Accounts)]
#[instruction(args:SolOcpFulfillSellArgs)]
pub struct SolOcpFulfillSell<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: we will check the owner field that matches the pool owner
    #[account(mut)]
    pub owner: UncheckedAccount<'info>,
    #[account(constraint = owner.key() != cosigner.key() @ MMMErrorCode::InvalidCosigner)]
    pub cosigner: Signer<'info>,
    #[account(
        mut,
        constraint = verify_referral(&pool, &referral) @ MMMErrorCode::InvalidReferral,
    )]
    /// CHECK: use verify_referral to check the referral account
    pub referral: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [POOL_PREFIX.as_bytes(), owner.key().as_ref(), pool.uuid.as_ref()],
        has_one = owner @ MMMErrorCode::InvalidOwner,
        has_one = cosigner @ MMMErrorCode::InvalidCosigner,
        constraint = pool.payment_mint.eq(&Pubkey::default()) @ MMMErrorCode::InvalidPaymentMint,
        constraint = pool.expiry == 0 || pool.expiry > Clock::get().unwrap().unix_timestamp @ MMMErrorCode::Expired,
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
    /// CHECK: we will check the metadata in check_allowlists_for_mint()
    #[account(
    seeds = [
        "metadata".as_bytes(),
        mpl_token_metadata::ID.as_ref(),
        asset_mint.key().as_ref(),
    ],
    bump,
    seeds::program = mpl_token_metadata::ID,
    )]
    pub asset_metadata: UncheckedAccount<'info>,
    #[account(
        constraint = asset_mint.supply == 1 && asset_mint.decimals == 0 @ MMMErrorCode::InvalidOcpAssetParams,
    )]
    pub asset_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        associated_token::mint = asset_mint,
        associated_token::authority = pool,
        constraint = sellside_escrow_token_account.amount == 1 @ MMMErrorCode::InvalidOcpAssetParams,
        constraint = args.asset_amount == 1 @ MMMErrorCode::InvalidOcpAssetParams,
    )]
    pub sellside_escrow_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: checked in init_if_needed_ocp_ata
    #[account(mut)]
    pub payer_asset_account: UncheckedAccount<'info>,
    /// CHECK: will be used for allowlist checks
    pub allowlist_aux_account: UncheckedAccount<'info>,
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

    /// CHECK: check in cpi
    #[account(mut)]
    pub ocp_mint_state: UncheckedAccount<'info>,
    /// CHECK: check in cpi
    pub ocp_policy: Box<Account<'info, Policy>>,
    /// CHECK: check in cpi
    pub ocp_freeze_authority: UncheckedAccount<'info>,
    /// CHECK: check in cpi
    #[account(address = open_creator_protocol::id())]
    pub ocp_program: UncheckedAccount<'info>,
    /// CHECK: check in cpi
    #[account(address = community_managed_token::id())]
    pub cmt_program: UncheckedAccount<'info>,
    /// CHECK: check in cpi
    #[account(address = sysvar::instructions::id())]
    pub instructions: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

pub fn handler<'info>(
    ctx: Context<'_, '_, '_, 'info, SolOcpFulfillSell<'info>>,
    args: SolOcpFulfillSellArgs,
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
    let payer_asset_metadata = &ctx.accounts.asset_metadata;
    let ocp_policy = &ctx.accounts.ocp_policy;

    let sellside_escrow_token_account = &ctx.accounts.sellside_escrow_token_account;
    let buyside_sol_escrow_account = &ctx.accounts.buyside_sol_escrow_account;
    let pool_seeds: &[&[&[u8]]] = &[&[
        POOL_PREFIX.as_bytes(),
        pool.owner.as_ref(),
        pool.uuid.as_ref(),
        &[ctx.bumps.pool],
    ]];

    let parsed_metadata = check_allowlists_for_mint(
        &pool.allowlists,
        asset_mint,
        payer_asset_metadata,
        None,
        args.allowlist_aux,
    )?;

    let (total_price, next_price) =
        get_sol_total_price_and_next_price(pool, args.asset_amount, false)?;
    let lp_fee = get_sol_lp_fee(pool, buyside_sol_escrow_account.lamports(), total_price)?;

    assert_valid_fees_bp(args.maker_fee_bp, args.taker_fee_bp)?;
    let maker_fee = get_sol_fee(total_price, args.maker_fee_bp)?;
    let taker_fee = get_sol_fee(total_price, args.taker_fee_bp)?;
    let referral_fee = u64::try_from(
        maker_fee
            .checked_add(taker_fee)
            .ok_or(MMMErrorCode::NumericOverflow)?,
    )
    .map_err(|_| MMMErrorCode::NumericOverflow)?;

    let transfer_sol_to = if pool.reinvest_fulfill_sell {
        buyside_sol_escrow_account.to_account_info()
    } else {
        owner.to_account_info()
    };

    init_if_needed_ocp_ata(
        ctx.accounts.ocp_program.to_account_info(),
        open_creator_protocol::cpi::accounts::InitAccountCtx {
            policy: ocp_policy.to_account_info(),
            mint: asset_mint.to_account_info(),
            metadata: payer_asset_metadata.to_account_info(),
            mint_state: ctx.accounts.ocp_mint_state.to_account_info(),
            from: payer.to_account_info(),
            from_account: payer_asset_account.to_account_info(),
            cmt_program: ctx.accounts.cmt_program.to_account_info(),
            instructions: ctx.accounts.instructions.to_account_info(),
            freeze_authority: ctx.accounts.ocp_freeze_authority.to_account_info(),
            token_program: token_program.to_account_info(),
            system_program: ctx.accounts.system_program.to_account_info(),
            associated_token_program: ctx.accounts.associated_token_program.to_account_info(),
            payer: payer.to_account_info(),
        },
        &token_program.key(),
    )?;

    // TODO: make sure that the lp fee is paid with the correct amount
    anchor_lang::solana_program::program::invoke(
        &anchor_lang::solana_program::system_instruction::transfer(
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

    open_creator_protocol::cpi::transfer(CpiContext::new_with_signer(
        ctx.accounts.ocp_program.to_account_info(),
        open_creator_protocol::cpi::accounts::TransferCtx {
            policy: ocp_policy.to_account_info(),
            mint: asset_mint.to_account_info(),
            metadata: payer_asset_metadata.to_account_info(),
            mint_state: ctx.accounts.ocp_mint_state.to_account_info(),
            from: pool.to_account_info(),
            from_account: sellside_escrow_token_account.to_account_info(),
            cmt_program: ctx.accounts.cmt_program.to_account_info(),
            instructions: ctx.accounts.instructions.to_account_info(),
            freeze_authority: ctx.accounts.ocp_freeze_authority.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            to: payer.to_account_info(),
            to_account: payer_asset_account.to_account_info(),
        },
        pool_seeds,
    ))?;

    // we can close the sellside_escrow_token_account if no amount left
    if sellside_escrow_token_account.amount == args.asset_amount {
        open_creator_protocol::cpi::close(CpiContext::new_with_signer(
            ctx.accounts.ocp_program.to_account_info(),
            open_creator_protocol::cpi::accounts::CloseCtx {
                policy: ocp_policy.to_account_info(),
                freeze_authority: ctx.accounts.ocp_freeze_authority.to_account_info(),
                mint: asset_mint.to_account_info(),
                metadata: payer_asset_metadata.to_account_info(),
                mint_state: ctx.accounts.ocp_mint_state.to_account_info(),
                from: pool.to_account_info(),
                from_account: sellside_escrow_token_account.to_account_info(),
                destination: owner.to_account_info(),
                token_program: token_program.to_account_info(),
                cmt_program: ctx.accounts.cmt_program.to_account_info(),
                instructions: ctx.accounts.instructions.to_account_info(),
            },
            pool_seeds,
        ))?;
    }

    if lp_fee > 0 {
        anchor_lang::solana_program::program::invoke(
            &anchor_lang::solana_program::system_instruction::transfer(
                payer.key, owner.key, lp_fee,
            ),
            &[
                payer.to_account_info(),
                owner.to_account_info(),
                system_program.to_account_info(),
            ],
        )?;
    }

    if referral_fee > 0 {
        anchor_lang::solana_program::program::invoke(
            &anchor_lang::solana_program::system_instruction::transfer(
                payer.key,
                referral.key,
                referral_fee,
            ),
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

    let royalty_bp = get_metadata_royalty_bp(total_price, &parsed_metadata, Some(ocp_policy));
    let royalty_paid = pay_creator_fees_in_sol(
        10000,
        total_price,
        &parsed_metadata,
        ctx.remaining_accounts,
        payer.to_account_info(),
        royalty_bp,
        &[&[&[]]],
        system_program.to_account_info(),
    )?;

    // prevent frontrun by pool config changes
    let payment_amount = total_price
        .checked_add(lp_fee)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_add(taker_fee as u64)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_add(royalty_paid)
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
    log_pool("post_sol_ocp_fulfill_sell", pool)?;
    try_close_pool(pool, owner.to_account_info())?;

    msg!(
        "{{\"lp_fee\":{},\"royalty_paid\":{},\"total_price\":{}}}",
        lp_fee,
        royalty_paid,
        total_price,
    );

    Ok(())
}
