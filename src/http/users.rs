use crate::http::{ApiContext, Result};
use anyhow::{anyhow, Context};
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash};
use axum::body::HttpBody;
use axum::extract::Extension;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose, Engine as _};
use hyper::Client;
use hyper_tls::HttpsConnector;

use crate::http::error::{Error, ResultExt};
use crate::http::extractor::AuthUser;

pub fn router() -> Router {
    Router::new()
        .route("/v1/users", post(create_user))
        .route("/v1/users/login", post(login_user))
        .route("/v1/me", get(get_current_user).put(update_user))
}

/// A wrapper type for all requests/responses from this module.
#[derive(serde::Serialize, serde::Deserialize)]
struct UserBody<T> {
    user: T,
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
struct User {
    email: String,
    token: String,
    username: String,
    image: Option<String>,
}

async fn create_user(
    ctx: Extension<ApiContext>,
    Json(req): Json<UserBody<NewUser>>,
) -> Result<Json<UserBody<User>>> {
    let password_hash = hash_password(req.user.password).await?;

    let image = get_base64_encoded_svg_image_for_user(&req.user.email)
        .await
        .map_err(|e| Error::Anyhow(anyhow!("failed to get user image: {}", e)))?;

    let user_id = sqlx::query_scalar!(
        r#"insert into "user" (username, email, image, password_hash) values ($1, $2, $3, $4) returning id"#,
        req.user.username,
        req.user.email,
        image,
        password_hash,
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
        user: User {
            email: req.user.email,
            token: AuthUser {
                user_id: user_id.into(),
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
) -> Result<Json<UserBody<User>>> {
    let user = sqlx::query!(
        r#"
            select id, email, username, image, password_hash 
            from "user" where email = $1
        "#,
        req.user.email,
    )
    .fetch_optional(&ctx.db)
    .await?
    .ok_or(Error::unprocessable_entity([("email", "does not exist")]))?;

    verify_password(req.user.password, user.password_hash).await?;

    Ok(Json(UserBody {
        user: User {
            email: user.email,
            token: AuthUser {
                user_id: user.id.into(),
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
) -> Result<Json<UserBody<User>>> {
    let user = sqlx::query!(
        r#"select email, username, image from "user" where id = $1"#,
        auth_user.user_id.clone().into_sqlx_uuid()
    )
    .fetch_one(&ctx.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => Error::NotFound,
        e => Error::Sqlx(e),
    })?;

    Ok(Json(UserBody {
        user: User {
            email: user.email,
            token: auth_user.to_jwt(&ctx),
            username: user.username,
            image: user.image,
        },
    }))
}

async fn update_user(
    auth_user: AuthUser,
    ctx: Extension<ApiContext>,
    Json(req): Json<UserBody<UpdateUser>>,
) -> Result<Json<UserBody<User>>> {
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
            update "user"
            set email = coalesce($1, "user".email),
                username = coalesce($2, "user".username),
                password_hash = coalesce($3, "user".password_hash)
            where id = $4
            returning email, username, image
        "#,
        req.user.email,
        req.user.username,
        password_hash,
        auth_user.user_id.clone().into_sqlx_uuid()
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
        user: User {
            email: user.email,
            token: auth_user.to_jwt(&ctx),
            username: user.username,
            image: user.image,
        },
    }))
}

async fn hash_password(password: String) -> Result<String> {
    Ok(tokio::task::spawn_blocking(move || -> Result<String> {
        let salt = SaltString::generate(rand::thread_rng());
        Ok(PasswordHash::generate(Argon2::default(), password, &salt)
            .map_err(|e| anyhow::anyhow!("failed to generate password hash: {}", e))?
            .to_string())
    })
    .await
    .context("panic in generating password hash")??)
}

async fn verify_password(password: String, password_hash: String) -> Result<()> {
    Ok(tokio::task::spawn_blocking(move || -> Result<()> {
        let hash = PasswordHash::new(&password_hash)
            .map_err(|e| anyhow::anyhow!("invalid password hash: {}", e))?;

        hash.verify_password(&[&Argon2::default()], password)
            .map_err(|e| match e {
                argon2::password_hash::Error::Password => Error::Unauthorized,
                _ => anyhow::anyhow!("failed to verify password hash: {}", e).into(),
            })
    })
    .await
    .context("panic in verifying password hash")??)
}

async fn get_base64_encoded_svg_image_for_user(email: &String) -> Result<String> {
    let https = HttpsConnector::new();
    let client = Client::builder().build::<_, hyper::Body>(https);
    let mut res = client
        .get(
            format!("https://joesch.moe/api/male/v1/{email}")
                .parse()
                .map_err(|_| Error::Anyhow(anyhow!("failed to parse profile picture uri")))?,
        )
        .await
        .map_err(|e| Error::Anyhow(anyhow!("failed to get profile picture {}", e)))?;

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
}
