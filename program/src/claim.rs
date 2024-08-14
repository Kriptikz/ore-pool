use ore_api::{consts::*, loaders::*};
use ore_pool_api::{consts::*, instruction::*, loaders::*, state::Member};
use ore_utils::AccountDeserialize;
use solana_program::{
    self, account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError,
};

/// Claim allows a member to claim their ORE from the pool.
pub fn process_claim<'a, 'info>(accounts: &'a [AccountInfo<'info>], data: &[u8]) -> ProgramResult {
    // Parse args.
    let args = ClaimArgs::try_from_bytes(data)?;
    let amount = u64::from_le_bytes(args.amount);

    // Load accounts.
    let [signer, beneficiary_info, member_info, pool_info, proof_info, treasury_info, treasury_tokens_info, ore_program, token_program] =
        accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    load_signer(signer)?;
    load_token_account(beneficiary_info, None, &MINT_ADDRESS, true)?;
    load_member(member_info, signer.key, true)?;
    load_pool(pool_info, false)?;
    load_treasury(treasury_info, false)?;
    load_treasury_tokens(treasury_tokens_info, true)?;
    load_program(ore_program, ore_api::id())?;
    load_program(token_program, spl_token::id())?;

    // Update member balance
    let mut member_data = member_info.try_borrow_mut_data()?;
    let member = Member::try_from_bytes_mut(&mut member_data)?;
    member.balance = member.balance.checked_sub(amount).unwrap();

    // Claim tokens to the beneficiary
    solana_program::program::invoke_signed(
        &ore_api::instruction::claim(*pool_info.key, *beneficiary_info.key, amount),
        &[
            pool_info.clone(),
            beneficiary_info.clone(),
            proof_info.clone(),
            treasury_info.clone(),
            treasury_tokens_info.clone(),
            token_program.clone(),
        ],
        &[&[POOL, &[POOL_BUMP]]],
    )?;

    Ok(())
}
