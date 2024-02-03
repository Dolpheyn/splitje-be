use super::{
    extractor::AuthUser,
    users::{is_user_in_group, UserBody},
};
use crate::{
    commons::{to_sqlx_uuid, to_uuid},
    dto::group::{Group, GroupBody, NewGroup, UpdateGroup},
    dto::user::User,
    http::{
        error::{Error, ResultExt},
        ApiContext, Result,
    },
    logic::group::{self, GroupsHandler},
    logic::ledger,
};

use anyhow::anyhow;
use axum::{
    extract::{Extension, Path},
    routing::{get, post},
    Json, Router,
};
use futures::stream::StreamExt;

use std::str::FromStr;

pub fn router() -> Router {
    Router::new()
        .route("/v1/groups", post(create_group)) // /groups
        .route(
            "/v1/groups/:group_id",
            get(find_group_by_id).put(update_group),
        )
        .route("/v1/groups/:group_id/users", post(add_user_to_group))
}

async fn create_group(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Json(req): Json<GroupBody<NewGroup>>,
) -> Result<Json<GroupBody<Group>>> {
    let handler = group::Handler::new(ctx.db.clone(), ledger::Handler::new());
    let group = handler.create_group(req.group.name, auth_user).await?;

    Ok(Json(GroupBody { group }))
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
    let handler = group::Handler::new(ctx.db.clone(), ledger::Handler::new());
    if !is_user_in_group(ctx.clone(), Path(auth_user.user_id), Path(group_id))
        .await?
        .0
    {
        return Err(Error::Forbidden);
    }

    handler
        .get_users_by_group(&group_id, None)
        .await
        .map(|user| Json(UserBody { user }))
}

async fn add_user_to_group(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(group_id): Path<uuid::Uuid>,
) -> Result<Json<uuid::Uuid>> {
    let handler = group::Handler::new(ctx.db.clone(), ledger::Handler::new());

    handler
        .add_user_to_group(
            &auth_user,
            &Group {
                id: group_id,
                name: Default::default(),
            },
            None,
        )
        .await
        .map(Json)
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
