use std::str::FromStr;

use crate::http::{ApiContext, Result};
use anyhow::anyhow;
use axum::extract::{Extension, Path};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::StreamExt;
use sqlx::{Postgres, Transaction};

use crate::http::error::{Error, ResultExt};

use super::extractor::{to_sqlx_uuid, to_uuid, AuthUser};
use super::users::{is_user_in_group, User, UserBody};

pub fn router() -> Router {
    Router::new()
        .route("/v1/groups", post(create_group)) // /groups
        .route(
            "/v1/groups/:group_id",
            get(find_group_by_id).put(update_group),
        )
        .route("/v1/groups/:group_id/users", post(add_user_to_group))
}

/// A wrapper type for all requests/responses from this module.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GroupBody<T> {
    pub group: T,
}

#[derive(serde::Deserialize)]
struct NewGroup {
    name: String,
}

#[derive(serde::Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
struct UpdateGroup {
    name: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Group {
    pub id: uuid::Uuid,
    pub name: String,
}

async fn create_group(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Json(req): Json<GroupBody<NewGroup>>,
) -> Result<Json<GroupBody<Group>>> {
    let mut tx = ctx.db.begin().await?;

    let group_id = sqlx::query_scalar!(
        r#"insert into "groups" (name) values ($1) returning id"#,
        req.group.name,
    )
    .fetch_one(&mut *tx)
    .await
    .on_constraint("groups_name_key", |_| {
        Error::unprocessable_entity([("group_name", "group name taken")])
    })?;

    if let Err(e) =
        add_user_to_group_inner(ctx, auth_user, Path(to_uuid(group_id)), Some(&mut tx)).await
    {
        log::error!("[create_group] fail to add user to group: {e:?}");
        let _ = tx.rollback().await;
        return Err(Error::Anyhow(anyhow!("")));
    };

    tx.commit().await.map_err(|e| {
        log::error!("[create_group] fail to commit db transaction: {e:?}");
        Error::Anyhow(anyhow!(""))
    })?;
    Ok(Json(GroupBody {
        group: Group {
            id: to_uuid(group_id),
            name: req.group.name,
        },
    }))
}

pub async fn get_groups_by_user(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(user_id): Path<uuid::Uuid>,
) -> Result<Json<GroupBody<Vec<Group>>>> {
    if auth_user.user_id != user_id {
        return Err(Error::Forbidden);
    }
    let groups: Vec<Option<Group>> = sqlx::query!(
        r#"
            SELECT
                g.id, g.name
            FROM "groups" g
            INNER JOIN "user_groups" ug
            ON g.id = ug.group_id
            WHERE ug.user_id = $1"#,
        to_sqlx_uuid(user_id),
    )
    .fetch(&ctx.db)
    .map(|g| {
        g.ok().map(|g| Group {
            id: to_uuid(g.id),
            name: g.name,
        })
    })
    .collect()
    .await;

    if groups.iter().any(|g| g.is_none()) {
        log::debug!("[get_groups_by_user] some groups are error");
        return Err(Error::Anyhow(anyhow!("")));
    }

    Ok(Json(GroupBody {
        group: groups.into_iter().map(Option::unwrap).collect(),
    }))
}

pub async fn get_users_by_group(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(group_id): Path<uuid::Uuid>,
) -> Result<Json<UserBody<Vec<User>>>> {
    if !is_user_in_group(ctx.clone(), Path(auth_user.user_id), Path(group_id))
        .await?
        .0
    {
        return Err(Error::Forbidden);
    }

    get_users_by_group_inner(ctx, &group_id, None).await
}

async fn get_users_by_group_inner(
    ctx: Extension<ApiContext>,
    group_id: &uuid::Uuid,
    tx: Option<&mut Transaction<'_, Postgres>>,
) -> Result<Json<UserBody<Vec<User>>>> {
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
        query.fetch(&ctx.db)
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

    Ok(Json(UserBody {
        user: users.into_iter().map(Option::unwrap).collect(),
    }))
}

async fn add_user_to_group(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(group_id): Path<uuid::Uuid>,
) -> Result<Json<uuid::Uuid>> {
    add_user_to_group_inner(ctx, auth_user, Path(group_id), None).await
}

async fn add_user_to_group_inner(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(group_id): Path<uuid::Uuid>,
    tx: Option<&mut Transaction<'_, Postgres>>,
) -> Result<Json<uuid::Uuid>> {
    let query = sqlx::query_scalar!(
        r#"insert into "user_groups" (user_id, group_id) values ($1, $2) returning id"#,
        to_sqlx_uuid(auth_user.user_id),
        to_sqlx_uuid(group_id),
    );

    let user_group_id = if let Some(mut tx) = tx {
        let user_group_id = query
            .fetch_one(&mut **tx)
            .await
            .on_constraint("user_groups_user_id_fkey", |_| {
                Error::unprocessable_entity([("user", "user does not exist")])
            })
            .on_constraint("user_groups_group_id_fkey", |_| {
                Error::unprocessable_entity([("group", "group does not exist")])
            })?;

        create_ledger_entries(ctx, group_id, auth_user.user_id, &mut tx).await?;
        user_group_id
    } else {
        let mut tx = ctx.db.begin().await?;
        let user_group_id = query
            .fetch_one(&mut *tx)
            .await
            .on_constraint("user_groups_user_id_fkey", |_| {
                Error::unprocessable_entity([("user", "user does not exist")])
            })
            .on_constraint("user_groups_group_id_fkey", |_| {
                Error::unprocessable_entity([("group", "group does not exist")])
            })?;

        create_ledger_entries(ctx, group_id, auth_user.user_id, &mut tx).await?;

        tx.commit().await?;
        user_group_id
    };

    Ok(Json(to_uuid(user_group_id)))
}

async fn create_ledger_entries(
    ctx: Extension<ApiContext>,
    group_id: uuid::Uuid,
    user_id: uuid::Uuid,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<()> {
    let other_users_in_group_ids = get_users_by_group_inner(ctx, &group_id, Some(tx))
        .await?
        .user
        .iter()
        .map(|u| u.id)
        .filter(|id| id != &user_id)
        .collect::<Vec<_>>();

    log::debug!("other users: {:?}", other_users_in_group_ids);

    if other_users_in_group_ids.is_empty() {
        return Ok(());
    }

    //[...current_users, user_id * len(current_users)]
    let left_side_ids = other_users_in_group_ids
        .iter()
        .chain(
            vec![user_id]
                .iter()
                .cycle()
                .take(other_users_in_group_ids.len()),
        )
        .map(|id| to_sqlx_uuid(*id))
        .collect::<Vec<_>>();

    log::debug!("left: {:?}", left_side_ids);

    //[user_id * len(current_users), ...current_users, ]
    let right_side_ids = vec![user_id]
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

async fn find_group_by_id(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(group_id): Path<uuid::Uuid>,
) -> Result<Json<GroupBody<Group>>> {
    if group_id.is_nil() {
        return Err(Error::unprocessable_entity([(
            "group_id",
            "group id empty",
        )]));
    }

    // is user in the group?
    let Json(g) = get_groups_by_user(ctx.clone(), auth_user, Path(auth_user.user_id)).await?;
    if !g.group.iter().any(|g| g.id == group_id) {
        return Err(Error::Unauthorized);
    }

    let group_name = sqlx::query_scalar!(
        r#"
         SELECT name
         FROM "groups"
         WHERE id=$1
         "#,
        to_sqlx_uuid(group_id)
    )
    .fetch_one(&ctx.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => Error::NotFound,
        e => Error::Sqlx(e),
    })?;

    Ok(Json(GroupBody {
        group: Group {
            id: group_id,
            name: group_name,
        },
    }))
}

async fn update_group(
    Path(group_id): Path<String>,
    ctx: Extension<ApiContext>,
    Json(req): Json<GroupBody<UpdateGroup>>,
) -> Result<Json<GroupBody<Group>>> {
    if group_id.is_empty() {
        return Err(Error::unprocessable_entity([(
            "group_id",
            "group id empty",
        )]));
    }
    if req.group == UpdateGroup::default() {
        return Err(Error::unprocessable_entity([("all", "all fields empty")]));
    }

    let group_id = sqlx::types::Uuid::from_str(&group_id).map_err(|e| {
        log::debug!("failed to convert string to uuid: {e}");
        Error::unprocessable_entity([("group_id", "invalid group id")])
    })?;

    let group = sqlx::query!(
        // Optional updates of fields without needing a separate query for each.
        r#"
            update "groups"
            set name = coalesce($2, "groups".name)
            where id = $1
            returning name
        "#,
        group_id,
        req.group.name,
    )
    .fetch_one(&ctx.db)
    .await
    .on_constraint("groups_name_key", |_| {
        Error::unprocessable_entity([("group_name", "group name taken")])
    })?;

    Ok(Json(GroupBody {
        group: Group {
            id: to_uuid(group_id),
            name: group.name,
        },
    }))
}
