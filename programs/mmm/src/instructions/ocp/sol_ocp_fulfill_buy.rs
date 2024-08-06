use anchor_lang::{prelude::*, solana_program::sysvar, AnchorDeserialize};
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
    index_ra,
    instructions::{
        check_remaining_accounts_for_m2, sol_fulfill_buy::SolFulfillBuyArgs, withdraw_m2,
    },
    state::{Pool, SellState},
    util::{
        assert_valid_fees_bp, check_allowlists_for_mint, get_buyside_seller_receives,
        get_lp_fee_bp, get_metadata_royalty_bp, get_sol_fee, get_sol_lp_fee,
        get_sol_total_price_and_next_price, log_pool, pay_creator_fees_in_sol, try_close_escrow,
        try_close_pool, try_close_sell_state,
    },
    verify_referral::verify_referral,
};

// FulfillBuy means a seller wants to sell NFT/SFT into the pool
// where the pool has some buyside payment liquidity. Therefore,
// the seller expects a min_payment_amount that goes back to the
// seller's wallet for the asset_amount that the seller wants to sell.
#[derive(Accounts)]
#[instruction(args:SolFulfillBuyArgs)]
pub struct SolOcpFulfillBuy<'info> {
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
    pub buyside_sol_escrow_account: UncheckedAccount<'info>,
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
        mint::token_program = token_program,
    )]
    pub asset_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = asset_mint,
        token::authority = payer,
        constraint = payer_asset_account.amount == 1 @ MMMErrorCode::InvalidOcpAssetParams,
        constraint = args.asset_amount == 1 @ MMMErrorCode::InvalidOcpAssetParams,
    )]
    pub payer_asset_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: check in cpi
    #[account(mut)]
    pub sellside_escrow_token_account: UncheckedAccount<'info>,
    /// CHECK: check in cpi
    #[account(mut)]
    pub owner_token_account: UncheckedAccount<'info>,
    /// CHECK: will be used for allowlist checks
    pub allowlist_aux_account: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = payer,
        seeds = [
            SELL_STATE_PREFIX.as_bytes(),
            pool.key().as_ref(),
            asset_mint.key().as_ref(),
        ],
        space = SellState::LEN,
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
    // Remaining accounts
    // Branch: using shared escrow accounts
    //   0: m2_program
    //   1: shared_escrow_account
    //   2+: creator accounts
    // Branch: not using shared escrow accounts
    //   0+: creator accounts
}

pub fn handler<'info>(
    ctx: Context<'_, '_, '_, 'info, SolOcpFulfillBuy<'info>>,
    args: SolFulfillBuyArgs,
) -> Result<()> {
    let token_program = &ctx.accounts.token_program;
    let system_program = &ctx.accounts.system_program;
    let associated_token_program = &ctx.accounts.associated_token_program;
    let pool = &mut ctx.accounts.pool;
    let sell_state = &mut ctx.accounts.sell_state;
    let owner = &ctx.accounts.owner;
    let referral = &ctx.accounts.referral;
    let payer = &ctx.accounts.payer;
    let payer_asset_account = &ctx.accounts.payer_asset_account;
    let asset_mint = &ctx.accounts.asset_mint;
    let payer_asset_metadata = &ctx.accounts.asset_metadata;
    let buyside_sol_escrow_account = &ctx.accounts.buyside_sol_escrow_account;
    let ocp_policy = &ctx.accounts.ocp_policy;
    let pool_key = pool.key();
    let buyside_sol_escrow_account_seeds: &[&[&[u8]]] = &[&[
        BUYSIDE_SOL_ESCROW_ACCOUNT_PREFIX.as_bytes(),
        pool_key.as_ref(),
        &[ctx.bumps.buyside_sol_escrow_account],
    ]];
    let remaining_accounts = ctx.remaining_accounts;

    let parsed_metadata = check_allowlists_for_mint(
        &pool.allowlists,
        asset_mint,
        payer_asset_metadata,
        None,
        args.allowlist_aux,
    )?;

    let (total_price, next_price) =
        get_sol_total_price_and_next_price(pool, args.asset_amount, true)?;
    let metadata_royalty_bp =
        get_metadata_royalty_bp(total_price, &parsed_metadata, Some(ocp_policy));
    let seller_receives = {
        let lp_fee_bp = get_lp_fee_bp(pool, buyside_sol_escrow_account.lamports());
        get_buyside_seller_receives(total_price, lp_fee_bp, metadata_royalty_bp, 10000)
    }?;
    let lp_fee = get_sol_lp_fee(pool, buyside_sol_escrow_account.lamports(), seller_receives)?;

    assert_valid_fees_bp(args.maker_fee_bp, args.taker_fee_bp)?;
    let maker_fee = get_sol_fee(seller_receives, args.maker_fee_bp)?;
    let taker_fee = get_sol_fee(seller_receives, args.taker_fee_bp)?;
    let referral_fee = u64::try_from(
        maker_fee
            .checked_add(taker_fee)
            .ok_or(MMMErrorCode::NumericOverflow)?,
    )
    .map_err(|_| MMMErrorCode::NumericOverflow)?;

    // check creator_accounts and verify the remaining accounts
    let creator_accounts = if pool.using_shared_escrow() {
        check_remaining_accounts_for_m2(remaining_accounts, &pool.owner.key())?;

        let amount: u64 = (total_price as i64 + maker_fee) as u64;
        withdraw_m2(
            pool,
            ctx.bumps.pool,
            buyside_sol_escrow_account,
            index_ra!(remaining_accounts, 1),
            system_program,
            index_ra!(remaining_accounts, 0),
            pool.owner,
            amount,
        )?;
        pool.shared_escrow_count = pool
            .shared_escrow_count
            .checked_sub(args.asset_amount)
            .ok_or(MMMErrorCode::NumericOverflow)?;

        &remaining_accounts[2..]
    } else {
        remaining_accounts
    };

    let (target_token_account, target_authority) = if pool.reinvest_fulfill_buy {
        (
            ctx.accounts.sellside_escrow_token_account.to_account_info(),
            pool.to_account_info(),
        )
    } else {
        (
            ctx.accounts.owner_token_account.to_account_info(),
            owner.to_account_info(),
        )
    };

    init_if_needed_ocp_ata(
        ctx.accounts.ocp_program.to_account_info(),
        open_creator_protocol::cpi::accounts::InitAccountCtx {
            policy: ocp_policy.to_account_info(),
            mint: asset_mint.to_account_info(),
            metadata: payer_asset_metadata.to_account_info(),
            mint_state: ctx.accounts.ocp_mint_state.to_account_info(),
            from: target_authority.to_account_info(),
            from_account: target_token_account.to_account_info(),
            cmt_program: ctx.accounts.cmt_program.to_account_info(),
            instructions: ctx.accounts.instructions.to_account_info(),
            freeze_authority: ctx.accounts.ocp_freeze_authority.to_account_info(),
            token_program: token_program.to_account_info(),
            system_program: ctx.accounts.system_program.to_account_info(),
            associated_token_program: associated_token_program.to_account_info(),
            payer: payer.to_account_info(),
        },
        &token_program.key(),
    )?;

    open_creator_protocol::cpi::transfer(CpiContext::new(
        ctx.accounts.ocp_program.to_account_info(),
        open_creator_protocol::cpi::accounts::TransferCtx {
            policy: ocp_policy.to_account_info(),
            mint: asset_mint.to_account_info(),
            metadata: payer_asset_metadata.to_account_info(),
            mint_state: ctx.accounts.ocp_mint_state.to_account_info(),
            from: payer.to_account_info(),
            from_account: payer_asset_account.to_account_info(),
            cmt_program: ctx.accounts.cmt_program.to_account_info(),
            instructions: ctx.accounts.instructions.to_account_info(),
            freeze_authority: ctx.accounts.ocp_freeze_authority.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            to: target_authority.to_account_info(),
            to_account: target_token_account.to_account_info(),
        },
    ))?;

    if pool.reinvest_fulfill_buy {
        pool.sellside_asset_amount = pool
            .sellside_asset_amount
            .checked_add(args.asset_amount)
            .ok_or(MMMErrorCode::NumericOverflow)?;
        sell_state.pool = pool.key();
        sell_state.pool_owner = owner.key();
        sell_state.asset_mint = asset_mint.key();
        sell_state.cosigner_annotation = pool.cosigner_annotation;
        sell_state.asset_amount = sell_state
            .asset_amount
            .checked_add(args.asset_amount)
            .ok_or(MMMErrorCode::NumericOverflow)?;
    }

    // we can close the payer_asset_account if no amount left
    if payer_asset_account.amount == args.asset_amount {
        open_creator_protocol::cpi::close(CpiContext::new(
            ctx.accounts.ocp_program.to_account_info(),
            open_creator_protocol::cpi::accounts::CloseCtx {
                policy: ocp_policy.to_account_info(),
                freeze_authority: ctx.accounts.ocp_freeze_authority.to_account_info(),
                mint: asset_mint.to_account_info(),
                metadata: payer_asset_metadata.to_account_info(),
                mint_state: ctx.accounts.ocp_mint_state.to_account_info(),
                from: payer.to_account_info(),
                from_account: payer_asset_account.to_account_info(),
                destination: payer.to_account_info(),
                token_program: token_program.to_account_info(),
                cmt_program: ctx.accounts.cmt_program.to_account_info(),
                instructions: ctx.accounts.instructions.to_account_info(),
            },
        ))?;
    }

    // pool owner as buyer is going to pay the royalties
    let royalty_paid = pay_creator_fees_in_sol(
        10000,
        seller_receives,
        &parsed_metadata,
        creator_accounts,
        buyside_sol_escrow_account.to_account_info(),
        metadata_royalty_bp,
        buyside_sol_escrow_account_seeds,
        system_program.to_account_info(),
    )?;

    // prevent frontrun by pool config changes
    // the royalties are paid by the buyer, but the seller will see the price
    // after adjusting the royalties.
    let payment_amount = total_price
        .checked_sub(lp_fee)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_sub(taker_fee as u64)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_sub(royalty_paid)
        .ok_or(MMMErrorCode::NumericOverflow)?;
    if payment_amount < args.min_payment_amount {
        return Err(MMMErrorCode::InvalidRequestedPrice.into());
    }

    anchor_lang::solana_program::program::invoke_signed(
        &anchor_lang::solana_program::system_instruction::transfer(
            buyside_sol_escrow_account.key,
            payer.key,
            payment_amount,
        ),
        &[
            buyside_sol_escrow_account.to_account_info(),
            payer.to_account_info(),
            system_program.to_account_info(),
        ],
        buyside_sol_escrow_account_seeds,
    )?;

    if lp_fee > 0 {
        anchor_lang::solana_program::program::invoke_signed(
            &anchor_lang::solana_program::system_instruction::transfer(
                buyside_sol_escrow_account.key,
                owner.key,
                lp_fee,
            ),
            &[
                buyside_sol_escrow_account.to_account_info(),
                owner.to_account_info(),
                system_program.to_account_info(),
                payer.to_account_info(),
            ],
            buyside_sol_escrow_account_seeds,
        )?;
    }
    if referral_fee > 0 {
        anchor_lang::solana_program::program::invoke_signed(
            &anchor_lang::solana_program::system_instruction::transfer(
                buyside_sol_escrow_account.key,
                referral.key,
                referral_fee,
            ),
            &[
                buyside_sol_escrow_account.to_account_info(),
                referral.to_account_info(),
                system_program.to_account_info(),
            ],
            buyside_sol_escrow_account_seeds,
        )?;
    }

    pool.lp_fee_earned = pool
        .lp_fee_earned
        .checked_add(lp_fee)
        .ok_or(MMMErrorCode::NumericOverflow)?;
    pool.spot_price = next_price;

    try_close_escrow(
        &buyside_sol_escrow_account.to_account_info(),
        pool,
        system_program,
        buyside_sol_escrow_account_seeds,
    )?;
    try_close_sell_state(sell_state, payer.to_account_info())?;

    // return the remaining per pool escrow balance to the shared escrow account
    if pool.using_shared_escrow() {
        let min_rent = Rent::get()?.minimum_balance(0);
        let shared_escrow_account = index_ra!(remaining_accounts, 1).to_account_info();
        if shared_escrow_account.lamports() + buyside_sol_escrow_account.lamports() > min_rent
            && buyside_sol_escrow_account.lamports() > 0
        {
            anchor_lang::solana_program::program::invoke_signed(
                &anchor_lang::solana_program::system_instruction::transfer(
                    buyside_sol_escrow_account.key,
                    shared_escrow_account.key,
                    buyside_sol_escrow_account.lamports(),
                ),
                &[
                    buyside_sol_escrow_account.to_account_info(),
                    shared_escrow_account,
                    system_program.to_account_info(),
                ],
                buyside_sol_escrow_account_seeds,
            )?;
        } else {
            try_close_escrow(
                buyside_sol_escrow_account,
                pool,
                system_program,
                buyside_sol_escrow_account_seeds,
            )?;
        }
    }
    pool.buyside_payment_amount = buyside_sol_escrow_account.lamports();

    log_pool("post_sol_ocp_fulfill_buy", pool)?;
    try_close_pool(pool, owner.to_account_info())?;

    msg!(
        "{{\"lp_fee\":{},\"royalty_paid\":{},\"total_price\":{}}}",
        lp_fee,
        royalty_paid,
        total_price,
    );

    Ok(())
}
