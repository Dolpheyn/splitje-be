use crate::{
    commons::to_sqlx_uuid,
    http::{Error, Result},
};

use sqlx::{self, Postgres, Transaction};

pub trait LedgerHandler {}

pub struct Handler {}

impl Handler {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn init_ledger_entries(
        &self,
        group_id: uuid::Uuid,
        user_id: uuid::Uuid,
        other_users_in_group_ids: Vec<uuid::Uuid>,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<(), Error> {
        log::debug!("other users: {:?}", other_users_in_group_ids);

        if other_users_in_group_ids.is_empty() {
            return Ok(());
        }

        //[...current_users, user_id * len(current_users)]
        let left_side_ids = other_users_in_group_ids
            .iter()
            .chain(
                [user_id]
                    .iter()
                    .cycle()
                    .take(other_users_in_group_ids.len()),
            )
            .map(|id| to_sqlx_uuid(*id))
            .collect::<Vec<_>>();

        log::debug!("left: {:?}", left_side_ids);

        //[user_id * len(current_users), ...current_users, ]
        let right_side_ids = [user_id]
            .iter()
            .cycle()
            .take(other_users_in_group_ids.len())
            .chain(other_users_in_group_ids.iter())
            .map(|id| to_sqlx_uuid(*id))
            .collect::<Vec<_>>();

        log::debug!("right: {:?}", right_side_ids);

        sqlx::query!(
            r#"
            INSERT INTO "ledgers"
              (group_id, this_user, other_user)
            VALUES (
              $1,
              unnest($2::uuid[]),
              unnest($3::uuid[])
            )
        "#,
            to_sqlx_uuid(group_id),
            &left_side_ids,
            &right_side_ids,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}

impl LedgerHandler for Handler {}
