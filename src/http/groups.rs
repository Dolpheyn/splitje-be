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

pub fn router() -> Router {
    Router::new()
        .route("/v1/groups", post(create_group)) // /groups
        .route(
            "/v1/groups/:group_id",
            get(find_group_by_id).put(update_group),
        )
}

/// A wrapper type for all requests/responses from this module.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GroupBody<T> {
    group: T,
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
    pub id: String,
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
        add_user_to_group_inner(ctx, auth_user, Path(group_id.to_string()), Some(&mut tx)).await
    {
        log::debug!("[create_group] fail to add user to group: {e}");
        let _ = tx.rollback().await;
        return Err(Error::Anyhow(anyhow!("")));
    };

    tx.commit().await.map_err(|e| {
        log::debug!("[create_group] fail to commit transaction: {e}");
        Error::Anyhow(anyhow!(""))
    })?;
    Ok(Json(GroupBody {
        group: Group {
            id: group_id.to_string(),
            name: req.group.name,
        },
    }))
}

pub async fn get_groups_by_user(
    ctx: Extension<ApiContext>,
    Path(user_id): Path<String>,
) -> Result<Json<GroupBody<Vec<Group>>>> {
    let user_id = sqlx::types::Uuid::from_str(&user_id).map_err(|e| {
        log::debug!("failed to convert string to uuid: {e}");
        Error::unprocessable_entity([("user_id", "invalid user id")])
    })?;

    let groups: Vec<Option<Group>> = sqlx::query!(
        r#"
            SELECT
                g.id, g.name
            FROM "groups" g
            INNER JOIN "user_groups" ug
            ON g.id = ug.group_id
            WHERE ug.user_id = $1"#,
        user_id,
    )
    .fetch(&ctx.db)
    .map(|g| {
        g.ok().map(|g| Group {
            id: g.id.to_string(),
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

async fn add_user_to_group_inner(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(group_id): Path<String>,
    tx: Option<&mut Transaction<'_, Postgres>>,
) -> Result<Json<uuid::Uuid>> {
    let group_id = sqlx::types::Uuid::from_str(&group_id).map_err(|e| {
        log::debug!("failed to convert string to uuid: {e}");
        Error::unprocessable_entity([("group_id", "invalid group id")])
    })?;

    let query = sqlx::query_scalar!(
        r#"insert into "user_groups" (user_id, group_id) values ($1, $2) returning id"#,
        to_sqlx_uuid(auth_user.user_id),
        group_id,
    );

    let id = if let Some(tx) = tx {
        query.fetch_one(&mut **tx).await?
    } else {
        query.fetch_one(&ctx.db).await?
    };

    Ok(Json(to_uuid(id)))
}

async fn find_group_by_id(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(group_id): Path<String>,
) -> Result<Json<GroupBody<Group>>> {
    if group_id.is_empty() {
        return Err(Error::unprocessable_entity([(
            "group_id",
            "group id empty",
        )]));
    }

    // is user in the group?
    let g = get_groups_by_user(ctx.clone(), Path(auth_user.user_id.to_string()))
        .await?
        .0;
    if !g.group.iter().any(|g| g.id == group_id) {
        return Err(Error::Unauthorized);
    }

    let group_id = sqlx::types::Uuid::from_str(&group_id).map_err(|e| {
        log::debug!("failed to convert string to uuid: {e}");
        Error::unprocessable_entity([("group_id", "invalid group id")])
    })?;

    let group_name = sqlx::query_scalar!(
        r#"
         SELECT name
         FROM "groups"
         WHERE id=$1
         "#,
        group_id
    )
    .fetch_one(&ctx.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => Error::NotFound,
        e => Error::Sqlx(e),
    })?;

    Ok(Json(GroupBody {
        group: Group {
            id: group_id.to_string(),
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
            id: group_id.to_string(),
            name: group.name,
        },
    }))
}
