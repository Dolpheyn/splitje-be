use std::collections::HashMap;
use std::fmt::Display;

use anyhow::anyhow;
use axum::extract::{Extension, Path};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::to_value as to_json_value;

use crate::http::error::{Error, ResultExt};
use crate::http::{ApiContext, Result};

use super::extractor::{to_sqlx_uuid, to_uuid, AuthUser};
use super::users;

pub fn router() -> Router {
    Router::new().route("/v1/transactions", post(create_transaction))
}

/// A wrapper type for all requests/responses from this module.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TxBody<T> {
    transaction: T,
}

#[derive(serde::Serialize, serde::Deserialize, sqlx::Type, Copy, Clone, PartialEq)]
#[sqlx(type_name = "txT", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TxType {
    Credit,
    Debit,
}

impl Display for TxType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                TxType::Credit => "Credit",
                TxType::Debit => "Debit",
            }
        )
    }
}

#[derive(serde::Serialize, serde::Deserialize, sqlx::Type)]
#[sqlx(type_name = "ackT", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AckStatus {
    NotAck,
    Ack,
}

impl Display for AckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                AckStatus::NotAck => "NotAck",
                AckStatus::Ack => "Ack",
            }
        )
    }
}

type TxMetadata = HashMap<String, String>;
#[derive(serde::Deserialize)]
struct NewTx {
    group_id: uuid::Uuid,
    payee_id: uuid::Uuid,
    amount: i64,
    tx_type: TxType,
    metadata: Option<TxMetadata>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Transaction {
    pub id: uuid::Uuid,
    pub group_id: uuid::Uuid,
    pub payer_id: uuid::Uuid,
    pub payee_id: uuid::Uuid,
    pub amount: i64,
    pub tx_type: TxType,
    pub ack_status: AckStatus,
    pub metadata: TxMetadata,
}

async fn create_transaction(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Json(req): Json<TxBody<NewTx>>,
) -> Result<Json<TxBody<Transaction>>> {
    // check if both auth_user and payee_id are in the group
    if !users::is_user_in_group(
        ctx.clone(),
        Path(auth_user.user_id),
        Path(req.transaction.group_id),
    )
    .await?
    .0
    {
        log::info!(
            "[create_transaction] user {} is not in group {}",
            auth_user.user_id.to_string(),
            req.transaction.group_id.to_string(),
        );
        return Err(Error::Forbidden);
    }
    if !users::is_user_in_group(
        ctx.clone(),
        Path(req.transaction.payee_id),
        Path(req.transaction.group_id),
    )
    .await?
    .0
    {
        log::info!(
            "[create_transaction] user {} is not in group {}",
            req.transaction.payee_id.to_string(),
            req.transaction.group_id.to_string(),
        );
        return Err(Error::Forbidden);
    }

    let req_metadata = req.transaction.metadata.unwrap_or_default();
    let metadata_json = to_json_value(req_metadata.clone()).map_err(|e| {
        log::error!("[create_transaction] fail converting metadata to json {e:?}");
        Error::unprocessable_entity([("metadata", "invalid metadata")])
    })?;

    let amount = if TxType::Debit == req.transaction.tx_type {
        -req.transaction.amount
    } else {
        req.transaction.amount
    };

    let mut tx = ctx.db.begin().await?;
    let txn_id = sqlx::query_scalar!(
        r#"
            INSERT INTO "transactions"
            (payer_id, payee_id, group_id, amount, tx_type, ack_status, metadata)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING id
        "#,
        to_sqlx_uuid(auth_user.user_id),
        to_sqlx_uuid(req.transaction.payee_id),
        to_sqlx_uuid(req.transaction.group_id),
        amount,
        req.transaction.tx_type as TxType,
        AckStatus::NotAck as AckStatus,
        metadata_json,
    )
    .fetch_one(&mut *tx)
    .await
    .on_constraint("transactions_name_key", |_| {
        Error::unprocessable_entity([("group_name", "group name taken")])
    })?;

    sqlx::query!(
        r#"
            UPDATE "ledgers"
            SET amount = amount + $1
            WHERE
                group_id = $2 AND
                this_user = $3 AND
                other_user = $4
        "#,
        amount,
        to_sqlx_uuid(req.transaction.group_id),
        to_sqlx_uuid(auth_user.user_id),
        to_sqlx_uuid(req.transaction.payee_id),
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        log::error!("[create_transaction] fail to update ledger auth_user side: {e}");
        Error::Anyhow(anyhow!(""))
    })?;

    sqlx::query!(
        r#"
            UPDATE "ledgers"
            SET amount = amount - $1
            WHERE
                group_id = $2 AND
                this_user = $3 AND
                other_user = $4
        "#,
        amount,
        to_sqlx_uuid(req.transaction.group_id),
        to_sqlx_uuid(req.transaction.payee_id),
        to_sqlx_uuid(auth_user.user_id),
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        log::error!("[create_transaction] fail to update ledger payee side: {e}");
        Error::Anyhow(anyhow!(""))
    })?;

    tx.commit().await.map_err(|e| {
        log::error!("[create_transaction] fail to commit db transaction: {e}");
        Error::Anyhow(anyhow!(""))
    })?;

    Ok(Json(TxBody {
        transaction: Transaction {
            id: to_uuid(txn_id),
            group_id: req.transaction.group_id,
            payer_id: auth_user.user_id,
            payee_id: req.transaction.payee_id,
            amount: req.transaction.amount,
            tx_type: req.transaction.tx_type,
            ack_status: AckStatus::NotAck,
            metadata: req_metadata,
        },
    }))
}

// pub async fn get_transactions_by_user(
//     ctx: Extension<ApiContext>,
//     Path(user_id): Path<String>,
//     // TODO add optional tx type param. If none - list all.
// ) -> Result<Json<TxBody<Vec<Transaction>>>> {
//     let user_id = sqlx::types::Uuid::from_str(&user_id).map_err(|e| {
//         log::debug!("failed to convert string to uuid: {e}");
//         Error::unprocessable_entity([("user_id", "invalid user id")])
//     })?;
//
//     let transactions: Vec<Option<Transaction>> = sqlx::query!(
//         r#"
//             SELECT
//                 g.id, g.name
//             FROM "transactions" g
//             INNER JOIN "user_transactions" ug
//             ON g.id = ug.group_id
//             WHERE ug.user_id = $1"#,
//         user_id,
//     )
//     .fetch(&ctx.db)
//     .map(|g| {
//         g.ok().map(|g| Transaction {
//             id: g.id.to_string(),
//             name: g.name,
//         })
//     })
//     .collect()
//     .await;
//
//     if transactions.iter().any(|g| g.is_none()) {
//         log::debug!("[get_transactions_by_user] some transactions are error");
//         return Err(Error::Anyhow(anyhow!("")));
//     }
//
//     Ok(Json(TxBody {
//         transaction: transactions.into_iter().map(Option::unwrap).collect(),
//     }))
// }
