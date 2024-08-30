use std::str::FromStr;

use actix_web::{web, HttpResponse, Responder};
use base64::prelude::*;
use ore_pool_api::state::Member;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Signature, signer::Signer,
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionReturnData;
use types::{ContributePayload, GetChallengePayload, RegisterPayload};

use crate::{aggregator::Aggregator, database, error::Error, operator::Operator, Contribution};

// TODO:
pub async fn register(payload: web::Json<RegisterPayload>) -> impl Responder {
    HttpResponse::Ok().finish()
}

async fn register_new_member(
    operator: &Operator,
    rpc_client: &RpcClient,
    db_client: &deadpool_postgres::Pool,
    payload: RegisterPayload,
) -> Result<(), Error> {
    // build ix
    let payer = &operator.keypair;
    let member_authority = payload.authority;
    let (pool_pda, _) = ore_pool_api::state::pool_pda(payer.pubkey());
    let ix = ore_pool_api::instruction::open(member_authority, pool_pda, payer.pubkey());
    let mut tx = Transaction::new_with_payer(&[ix], Some(&payer.pubkey()));
    let hash = rpc_client.get_latest_blockhash().await?;
    tx.sign(&[payer], hash);
    let sig = rpc_client.send_transaction(&tx).await?;
    // confirm transaction and fetch member-id from return data
    confirm_transaction(rpc_client, &sig).await?;
    let member_id = register_return_data(rpc_client, &sig).await?;
    // write member to db
    let member = Member {
        id: member_id,
        pool: pool_pda,
        authority: member_authority,
        balance: 0,
        total_balance: 0,
    };
    let db_client = db_client.get().await?;
    database::write_new_member(&db_client, &member, false).await?;
    Ok(())
}

async fn confirm_transaction(rpc_client: &RpcClient, sig: &Signature) -> Result<(), Error> {
    // Confirm the transaction with retries
    let max_retries = 5;
    let mut retries = 0;
    while retries < max_retries {
        if let Ok(confirmed) = rpc_client
            .confirm_transaction_with_commitment(sig, CommitmentConfig::confirmed())
            .await
        {
            if confirmed.value {
                break;
            }
        }
        retries += 1;
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }
    if retries == max_retries {
        return Err(Error::Internal("could not confirm transaction".to_string()));
    }
    Ok(())
}

type MemberId = u64;
async fn register_return_data(rpc_client: &RpcClient, sig: &Signature) -> Result<MemberId, Error> {
    let transaction = rpc_client
        .get_transaction(
            &sig,
            solana_transaction_status::UiTransactionEncoding::JsonParsed,
        )
        .await?;
    let return_data = transaction
        .transaction
        .meta
        .ok_or(Error::Internal(
            "missing return data (meta) from open instruction".to_string(),
        ))?
        .return_data;
    let return_data: Option<UiTransactionReturnData> = From::from(return_data);
    let (return_data, _) = return_data
        .ok_or(Error::Internal(
            "missing return data (meta) from open instruction".to_string(),
        ))?
        .data;
    let return_data = BASE64_STANDARD.decode(return_data)?;
    let return_data: [u8; 8] = return_data.as_slice().try_into()?;
    let member_id = u64::from_le_bytes(return_data);
    Ok(member_id)
}

pub async fn challenge(
    payload: web::Path<GetChallengePayload>,
    aggregator: web::Data<tokio::sync::Mutex<Aggregator>>,
) -> impl Responder {
    let member_authority = payload.into_inner().authority;
    match Pubkey::from_str(member_authority.as_str()) {
        Ok(member_authority) => {
            let aggregator = aggregator.as_ref();
            let mut aggregator = aggregator.lock().await;
            let challenge = aggregator.nonce_index(&member_authority).await;
            match challenge {
                Ok(challenge) => HttpResponse::Ok().json(challenge),
                Err(err) => {
                    log::error!("{:?}", err);
                    HttpResponse::InternalServerError().finish()
                }
            }
        }
        Err(err) => {
            log::error!("{:?}", err);
            HttpResponse::BadRequest().body(err.to_string())
        }
    }
}

/// Accepts solutions from pool members. If their solutions are valid, it
/// aggregates the contributions into a list for publishing and submission.
pub async fn contribute(
    payload: web::Json<ContributePayload>,
    tx: web::Data<tokio::sync::mpsc::UnboundedSender<Contribution>>,
    aggregator: web::Data<tokio::sync::Mutex<Aggregator>>,
) -> impl Responder {
    log::info!("received payload");
    log::info!("decoded: {:?}", payload);
    // lock aggregrator to ensure we're contributing to the current challenge
    let aggregator = aggregator.as_ref();
    let aggregator = aggregator.lock().await;
    // decode solution difficulty
    let solution = &payload.solution;
    log::info!("solution: {:?}", solution);
    let difficulty = solution.to_hash().difficulty();
    log::info!("difficulty: {:?}", difficulty);
    // authenticate the sender signature
    if !payload
        .signature
        .verify(&payload.authority.to_bytes(), &solution.to_bytes())
    {
        return HttpResponse::Unauthorized().finish();
    }
    // error if solution below min difficulty
    if difficulty < (aggregator.challenge.min_difficulty as u32) {
        log::error!("solution below min difficulity: {:?}", payload.authority);
        return HttpResponse::BadRequest().finish();
    }
    // error if digest is invalid
    if !drillx::is_valid_digest(&aggregator.challenge.challenge, &solution.n, &solution.d) {
        log::error!("invalid solution");
        return HttpResponse::BadRequest().finish();
    }
    // calculate score
    let score = 2u64.pow(difficulty);
    log::info!("score: {}", score);
    // TODO: Reject if score is below min difficulty (as defined by the pool operator)

    // update the aggegator
    if let Err(err) = tx.send(Contribution {
        member: payload.authority,
        score,
        solution: payload.solution,
    }) {
        log::error!("{:?}", err);
    }
    HttpResponse::Ok().finish()
}
