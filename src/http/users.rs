use super::groups::{self, GroupBody};
use crate::{
    commons::{to_sqlx_uuid, to_uuid},
    dto::group::Group,
    http::{
        error::{Error, ResultExt},
        extractor::AuthUser,
        ApiContext, Result,
    },
};

use anyhow::{anyhow, Context};
use argon2::{password_hash::SaltString, Argon2, PasswordHash};
use axum::{
    body::HttpBody,
    extract::{Extension, Path},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose, Engine as _};
use hyper::Client;
use hyper_tls::HttpsConnector;

pub fn router() -> Router {
    Router::new()
        .route("/v1/users", post(create_user))
        .route("/v1/users/:user_id/groups", get(get_user_groups))
        .route("/v1/users/login", post(login_user))
        .route("/v1/me", get(get_current_user).put(update_user))
}

/// A wrapper type for all requests/responses from this module.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct UserBody<T> {
    pub user: T,
}

#[derive(serde::Deserialize)]
struct NewUser {
    username: String,
    email: String,
    password: String,
}

#[derive(serde::Deserialize)]
struct LoginUser {
    email: String,
    password: String,
}

#[derive(serde::Deserialize, Default, PartialEq, Eq)]
#[serde(default)] // fill in any missing fields with `..UpdateUser::default()`
struct UpdateUser {
    email: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CurrentUser {
    id: String,
    email: String,
    token: String,
    username: String,
    image: Option<String>,
}

async fn create_user(
    ctx: Extension<ApiContext>,
    Json(req): Json<UserBody<NewUser>>,
) -> Result<Json<UserBody<CurrentUser>>> {
    let password_hash = hash_password(req.user.password).await?;

    let image = get_base64_encoded_svg_image_for_user(&req.user.email)
        .await
        .map_err(|e| Error::Anyhow(anyhow!("failed to get user image: {}", e)))?;

    let user_id = sqlx::query_scalar!(
        r#"insert into "users" (username, email, image, password_hash) values ($1, $2, $3, $4) returning id"#,
        req.user.username,
        req.user.email,
        image,
        password_hash,
    )
    .fetch_one(&ctx.db)
    .await
    .on_constraint("users_username_key", |_| {
        Error::unprocessable_entity([("username", "username taken")])
    })
    .on_constraint("users_email_key", |_| {
        Error::unprocessable_entity([("email", "email taken")])
    })?;

    Ok(Json(UserBody {
        user: CurrentUser {
            id: user_id.to_string(),
            email: req.user.email,
            token: AuthUser {
                user_id: to_uuid(user_id),
            }
            .to_jwt(&ctx),
            username: req.user.username,
            image: Some(image),
        },
    }))
}

async fn login_user(
    ctx: Extension<ApiContext>,
    Json(req): Json<UserBody<LoginUser>>,
) -> Result<Json<UserBody<CurrentUser>>> {
    let user = sqlx::query!(
        r#"
            select id, email, username, image, password_hash 
            from "users" where email = $1
        "#,
        req.user.email,
    )
    .fetch_optional(&ctx.db)
    .await?
    .ok_or(Error::unprocessable_entity([("email", "does not exist")]))?;

    verify_password(req.user.password, user.password_hash).await?;

    Ok(Json(UserBody {
        user: CurrentUser {
            id: user.id.to_string(),
            email: user.email,
            token: AuthUser {
                user_id: to_uuid(user.id),
            }
            .to_jwt(&ctx),
            username: user.username,
            image: user.image,
        },
    }))
}

async fn get_current_user(
    auth_user: AuthUser,
    ctx: Extension<ApiContext>,
) -> Result<Json<UserBody<CurrentUser>>> {
    let user = sqlx::query!(
        r#"select email, username, image from "users" where id = $1"#,
        to_sqlx_uuid(auth_user.user_id)
    )
    .fetch_one(&ctx.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => Error::NotFound,
        e => Error::Sqlx(e),
    })?;

    dbg!("auth user", auth_user);
    Ok(Json(UserBody {
        user: CurrentUser {
            id: auth_user.user_id.to_string(),
            email: user.email,
            token: auth_user.to_jwt(&ctx),
            username: user.username,
            image: user.image,
        },
    }))
}

async fn update_user(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Json(req): Json<UserBody<UpdateUser>>,
) -> Result<Json<UserBody<CurrentUser>>> {
    if req.user == UpdateUser::default() {
        return get_current_user(auth_user, ctx).await;
    }

    let password_hash = if let Some(password) = req.user.password {
        Some(hash_password(password).await?)
    } else {
        None
    };

    let user = sqlx::query!(
        // Optional updates of fields without needing a separate query for each.
        r#"
            update "users"
            set email = coalesce($1, "users".email),
                username = coalesce($2, "users".username),
                password_hash = coalesce($3, "users".password_hash)
            where id = $4
            returning id, email, username, image
        "#,
        req.user.email,
        req.user.username,
        password_hash,
        to_sqlx_uuid(auth_user.user_id)
    )
    .fetch_one(&ctx.db)
    .await
    .on_constraint("user_username_key", |_| {
        Error::unprocessable_entity([("username", "username taken")])
    })
    .on_constraint("user_email_key", |_| {
        Error::unprocessable_entity([("email", "email taken")])
    })?;

    Ok(Json(UserBody {
        user: CurrentUser {
            id: user.id.to_string(),
            email: user.email,
            token: auth_user.to_jwt(&ctx),
            username: user.username,
            image: user.image,
        },
    }))
}

async fn get_user_groups(
    ctx: Extension<ApiContext>,
    auth_user: AuthUser,
    Path(user_id): Path<uuid::Uuid>,
) -> Result<Json<GroupBody<Vec<Group>>>> {
    groups::get_groups_by_user(ctx, auth_user, Path(user_id)).await
}

pub async fn is_user_in_group(
    ctx: Extension<ApiContext>,
    Path(user_id): Path<uuid::Uuid>,
    Path(group_id): Path<uuid::Uuid>,
) -> Result<Json<bool>> {
    let user_groups = groups::get_groups_by_user(ctx, AuthUser { user_id }, Path(user_id))
        .await?
        .0;
    Ok(Json(user_groups.group.iter().any(|g| g.id == group_id)))
}

async fn hash_password(password: String) -> Result<String> {
    tokio::task::spawn_blocking(move || -> Result<String> {
        let salt = SaltString::generate(rand::thread_rng());
        Ok(PasswordHash::generate(Argon2::default(), password, &salt)
            .map_err(|e| anyhow::anyhow!("failed to generate password hash: {}", e))?
            .to_string())
    })
    .await
    .context("panic in generating password hash")?
}

async fn verify_password(password: String, password_hash: String) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let hash = PasswordHash::new(&password_hash)
            .map_err(|e| anyhow::anyhow!("invalid password hash: {}", e))?;

        hash.verify_password(&[&Argon2::default()], password)
            .map_err(|e| match e {
                argon2::password_hash::Error::Password => Error::Unauthorized,
                _ => anyhow::anyhow!("failed to verify password hash: {}", e).into(),
            })
    })
    .await
    .context("panic in verifying password hash")?
}

async fn get_base64_encoded_svg_image_for_user(email: &String) -> Result<String> {
    let https = HttpsConnector::new();
    let client = Client::builder().build::<_, hyper::Body>(https);
    let mut res = client
        .get(
            format!("https://joesch.moe/api/v1/male/{email}")
                .parse()
                .map_err(|_| Error::Anyhow(anyhow!("failed to parse profile picture uri")))?,
        )
        .await
        .map_err(|e| Error::Anyhow(anyhow!("failed to get profile picture {}", e)))?;

    let status = res.status();
    if status.is_success() {
        let mut full_body: Vec<u8> = Vec::new();
        while let Some(chunk) = res.body_mut().data().await {
            let mut chunk = chunk
                .map_err(|e| Error::Anyhow(anyhow!("chunk fail {}", e)))?
                .into_iter()
                .collect::<Vec<u8>>();
            full_body.append(&mut chunk);
        }
        let encoded = general_purpose::STANDARD.encode(&full_body);

        Ok(encoded)
    } else {
        Err(Error::Anyhow(anyhow!(
            "get profile picture return err. err={status}",
        )))
    }
}
