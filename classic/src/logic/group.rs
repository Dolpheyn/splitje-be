use crate::{
    commons::{to_sqlx_uuid, to_uuid},
    dto::{group::Group, user::User},
    http::{extractor::AuthUser, Error, Result, ResultExt},
};

use anyhow::anyhow;
use futures::StreamExt;
use sqlx::{self, Pool, Postgres, Transaction};

use super::ledger::{self};

pub trait GroupsHandler {
    fn create_group(
        &self,
        name: String,
        owner: AuthUser,
    ) -> impl std::future::Future<Output = Result<Group, Error>> + Send;

    fn add_user_to_group(
        &self,
        user: &AuthUser,
        group: &Group,
        tx: Option<&mut Transaction<'_, Postgres>>,
    ) -> impl std::future::Future<Output = Result<uuid::Uuid, Error>> + Send;

    fn get_users_by_group(
        &self,
        group_id: &uuid::Uuid,
        tx: Option<&mut Transaction<'_, Postgres>>,
    ) -> impl std::future::Future<Output = Result<Vec<User>, Error>> + Send;
}

pub struct Handler {
    db: Pool<Postgres>,
    ledger_handler: ledger::Handler,
}

impl Handler {
    pub fn new(db: Pool<Postgres>, ledger_handler: ledger::Handler) -> Self {
        Self { db, ledger_handler }
    }
}

impl GroupsHandler for Handler {
    // Creates a group with `name` and add user `owner` to the group.
    async fn create_group(&self, group_name: String, owner: AuthUser) -> Result<Group, Error> {
        let mut tx = self.db.begin().await?;

        let group_id = sqlx::query_scalar!(
            r#"insert into "groups" (name) values ($1) returning id"#,
            group_name,
        )
        .fetch_one(&mut *tx)
        .await
        .on_constraint("groups_name_key", |_| {
            Error::unprocessable_entity([("group_name", "group name taken")])
        })?;

        let group = Group {
            id: to_uuid(group_id),
            name: group_name,
        };

        if let Err(e) = self.add_user_to_group(&owner, &group, Some(&mut tx)).await {
            log::error!("[create_group] fail to add user to group: {e:?}");
            let _ = tx.rollback().await;
            return Err(Error::Anyhow(anyhow!("")));
        };

        tx.commit().await.map_err(|e| {
            log::error!("[create_group] fail to commit db transaction: {e:?}");
            Error::Anyhow(anyhow!(""))
        })?;

        Ok(group)
    }

    // Add user `user` to group `group`,
    // then initializes ledger entries for `user` against other members of the group.
    async fn add_user_to_group(
        &self,
        user: &AuthUser,
        group: &Group,
        tx: Option<&mut Transaction<'_, Postgres>>,
    ) -> Result<uuid::Uuid, Error> {
        let group_id = group.id;
        let user_id = user.user_id;

        let query = sqlx::query_scalar!(
            r#"insert into "user_groups" (user_id, group_id) values ($1, $2) returning id"#,
            to_sqlx_uuid(user.user_id),
            to_sqlx_uuid(group_id),
        );

        // Use given transaction if present, otherwise begin a new transaction.
        let user_group_id = if let Some(tx) = tx {
            let user_group_id = query
                .fetch_one(&mut **tx)
                .await
                .on_constraint("user_groups_user_id_fkey", |_| {
                    Error::unprocessable_entity([("user", "user does not exist")])
                })
                .on_constraint("user_groups_group_id_fkey", |_| {
                    Error::unprocessable_entity([("group", "group does not exist")])
                })?;

            let other_users_in_group_ids = self
                .get_users_by_group(&group_id, Some(tx))
                .await?
                .iter()
                .map(|u| u.id)
                .filter(|id| id != &user_id)
                .collect::<Vec<_>>();

            self.ledger_handler
                .init_ledger_entries(group_id, user.user_id, other_users_in_group_ids, tx)
                .await?;

            user_group_id
        } else {
            let mut tx = self.db.begin().await?;
            let user_group_id = query
                .fetch_one(&mut *tx)
                .await
                .on_constraint("user_groups_user_id_fkey", |_| {
                    Error::unprocessable_entity([("user", "user does not exist")])
                })
                .on_constraint("user_groups_group_id_fkey", |_| {
                    Error::unprocessable_entity([("group", "group does not exist")])
                })?;

            let other_users_in_group_ids = self
                .get_users_by_group(&group_id, Some(&mut tx))
                .await?
                .iter()
                .map(|u| u.id)
                .filter(|id| id != &user_id)
                .collect::<Vec<_>>();

            self.ledger_handler
                .init_ledger_entries(group_id, user.user_id, other_users_in_group_ids, &mut tx)
                .await?;

            tx.commit().await?;

            user_group_id
        };

        Ok(to_uuid(user_group_id))
    }

    async fn get_users_by_group(
        &self,
        group_id: &uuid::Uuid,
        tx: Option<&mut Transaction<'_, Postgres>>,
    ) -> Result<Vec<User>, Error> {
        let query = sqlx::query!(
            r#"
            SELECT
                u.id, u.username, u.email
            FROM "users" u
            INNER JOIN "user_groups" ug
            ON u.id = ug.user_id
            WHERE ug.group_id = $1"#,
            to_sqlx_uuid(*group_id),
        );

        let query_stream = if let Some(tx) = tx {
            query.fetch(&mut **tx)
        } else {
            query.fetch(&self.db)
        };

        let users: Vec<Option<User>> = query_stream
            .map(|u| {
                u.ok().map(|u| User {
                    id: to_uuid(u.id),
                    username: u.username,
                    email: u.email,
                })
            })
            .collect()
            .await;

        if users.iter().any(|u| u.is_none()) {
            log::debug!("[get_users_by_group] some users are error");
            return Err(Error::Anyhow(anyhow!("")));
        }

        Ok(users.into_iter().map(Option::unwrap).collect())
    }
}
