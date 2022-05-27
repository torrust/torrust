use actix_web::{web, Responder, HttpResponse, HttpRequest};
use serde::{Deserialize, Serialize};
use rand::{distributions::Alphanumeric, Rng};
use pbkdf2::{
    password_hash::{
        rand_core::OsRng,
        PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
    },
    Pbkdf2,
};
use std::borrow::Cow;
use crate::errors::{ServiceResult, ServiceError};
use crate::common::WebAppData;
use jsonwebtoken::{DecodingKey, decode, Validation, Algorithm};
use crate::models::response::OkResponse;
use crate::models::response::TokenResponse;
use crate::mailer::VerifyClaims;

pub fn init_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/user")
            .service(web::resource("/register")
                .route(web::post().to(register)))
            .service(web::resource("/login")
                .route(web::post().to(login)))
            .service(web::resource("/ban/{user}")
                .route(web::delete().to(ban_user)))
            .service(web::resource("/verify/{token}")
                .route(web::get().to(verify_user)))
            .service(web::resource("/create_invite")
                .route(web::get().to(create_invite)))
            );
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Register {
    pub username: String,
    pub email: String,
    pub password: String,
    pub confirm_password: String,
    pub invite_code: Option<String>
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Login {
    pub login: String,
    pub password: String,
}

pub async fn register(req: HttpRequest, payload: web::Json<Register>, app_data: WebAppData) -> ServiceResult<impl Responder> {

    let settings = app_data.cfg.settings.read().await;

    if !settings.auth.registration_enabled {
        return Err(ServiceError::ClosedForRegistration)
    }

    // if invite_code is None after this block invite_only must be disabled
    let mut invite_code = None;
    if settings.auth.invite_only {
        let code = match &payload.invite_code {
            Some(code) => code,
            None => return Err(ServiceError::InvalidInviteCode)
        };
        let code = app_data.database.verify_invite_code(code).await;
        invite_code = match code {
            None => return Err(ServiceError::InvalidInviteCode),
            Some(c) => Some(c),
        };
    }


    if payload.password != payload.confirm_password {
        return Err(ServiceError::PasswordsDontMatch);
    }

    let password_length = payload.password.len();
    if password_length <= settings.auth.min_password_length {
        return Err(ServiceError::PasswordTooShort);
    }
    if password_length >= settings.auth.max_password_length {
        return Err(ServiceError::PasswordTooLong);
    }

    let salt = SaltString::generate(&mut OsRng);
    let password_hash;
    if let Ok(password) = Pbkdf2.hash_password(payload.password.as_bytes(), &salt) {
        password_hash = password.to_string();
    } else {
        return Err(ServiceError::InternalServerError);
    }

    if payload.username.contains('@') {
        return Err(ServiceError::UsernameInvalid)
    }

    let res = sqlx::query!(
        "INSERT INTO torrust_users (username, email, password) VALUES ($1, $2, $3)",
        payload.username,
        payload.email,
        password_hash,
    )
        .execute(&app_data.database.pool)
        .await;

    if let Err(sqlx::Error::Database(err)) = res {
        return if err.code() == Some(Cow::from("2067")) {
            if err.message().contains("torrust_users.username") {
                Err(ServiceError::UsernameTaken)
            } else if err.message().contains("torrust_users.email") {
                Err(ServiceError::EmailTaken)
            } else {
                Err(ServiceError::InternalServerError)
            }
        } else {
            Err(sqlx::Error::Database(err).into())
        };
    }

    // count accounts
    let res_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM torrust_users")
        .fetch_one(&app_data.database.pool)
        .await?;

    // make admin if first account
    if res_count.0 == 1 {
        let _res_make_admin = sqlx::query!("UPDATE torrust_users SET administrator = 1")
            .execute(&app_data.database.pool)
            .await;
    }

    let conn_info = req.connection_info();

    if settings.mail.email_verification_enabled {
        let mail_res = app_data.mailer.send_verification_mail(
            &payload.email,
            &payload.username,
            format!("{}://{}", conn_info.scheme(), conn_info.host()).as_str()
        )
            .await;

        // get user id from user insert res
        let user_id = res.unwrap().last_insert_rowid();

        if mail_res.is_err() {
            let _ = app_data.database.delete_user(user_id).await;
            return Err(ServiceError::FailedToSendVerificationEmail)
        }
    }
   
    if let Some(invite_code) = &invite_code {
    //now that the user is created it is safe to invalidate the invite link
        if settings.auth.invite_only {
            let _res = sqlx::query!("UPDATE invites SET username = $1 WHERE key=$2", payload.username ,invite_code.key)
            .execute(&app_data.database.pool)
            .await;
 
            let _res = sqlx::query!("UPDATE invites SET valid=FALSE WHERE username=?", payload.username)
                .execute(&app_data.database.pool)
                .await;
        }
    }
 
    Ok(HttpResponse::Ok())
}

pub async fn login(payload: web::Json<Login>, app_data: WebAppData) -> ServiceResult<impl Responder> {
    let settings = app_data.cfg.settings.read().await;

    let res = if payload.login.contains('@') {
        app_data.database.get_user_with_email(&payload.login).await
    } else {
        app_data.database.get_user_with_username(&payload.login).await
    };

    match res {
        Some(user) => {
            if settings.mail.email_verification_enabled && !user.email_verified {
                return Err(ServiceError::EmailNotVerified)
            }

            drop(settings);

            let parsed_hash = PasswordHash::new(&user.password)?;

            if !Pbkdf2.verify_password(payload.password.as_bytes(), &parsed_hash).is_ok() {
                return Err(ServiceError::WrongPasswordOrUsername);
            }

            let username = user.username.clone();
            let token = app_data.auth.sign_jwt(user.clone()).await;


            Ok(HttpResponse::Ok().json(OkResponse {
                data: TokenResponse {
                    token,
                    username,
                    admin: user.administrator
                }
            }))
        }
        None => Err(ServiceError::WrongPasswordOrUsername)
    }
}

pub async fn verify_user(req: HttpRequest, app_data: WebAppData) -> String {
    let settings = app_data.cfg.settings.read().await;
    let token = req.match_info().get("token").unwrap();

    let token_data = match decode::<VerifyClaims>(
        token,
        &DecodingKey::from_secret(settings.auth.secret_key.as_bytes()),
        &Validation::new(Algorithm::HS256),
    ) {
        Ok(token_data) => {
            if !token_data.claims.iss.eq("email-verification") {
                return ServiceError::TokenInvalid.to_string()
            }

            token_data.claims
        },
        Err(_) => return ServiceError::TokenInvalid.to_string()
    };

    drop(settings);

    let res = sqlx::query!(
        "UPDATE torrust_users SET email_verified = TRUE WHERE username = ?",
        token_data.sub
    )
        .execute(&app_data.database.pool)
        .await;

    if let Err(_) = res {
        return ServiceError::InternalServerError.to_string()
    }

    String::from("Email verified, you can close this page.")
}

pub async fn create_invite(req: HttpRequest, app_data: WebAppData) -> ServiceResult<impl Responder> {
    let user = app_data.auth.get_user_from_request(&req).await?;

    if !user.administrator { return Err(ServiceError::Unauthorized ) }
    
    let invite_code: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let res = sqlx::query!("INSERT INTO invites (valid, key)
                            VALUES (true, ?)", invite_code)
        .execute(&app_data.database.pool)
        .await;
    if let Err(_) = res { return Err(ServiceError::UsernameNotFound) }
    Ok(invite_code)
}

pub async fn ban_user(req: HttpRequest, app_data: WebAppData) -> ServiceResult<impl Responder> {
    let user = app_data.auth.get_user_from_request(&req).await?;

    // check if user is administrator
    if !user.administrator { return Err(ServiceError::Unauthorized) }

    let to_be_banned_username = req.match_info().get("user").unwrap();

    let res = sqlx::query!(
        "DELETE FROM torrust_users WHERE username = ? AND administrator = 0",
        to_be_banned_username
    )
        .execute(&app_data.database.pool)
        .await;

    if let Err(_) = res { return Err(ServiceError::UsernameNotFound) }
    if res.unwrap().rows_affected() == 0 { return Err(ServiceError::UsernameNotFound) }

    Ok(HttpResponse::Ok().json(OkResponse {
        data: format!("Banned user: {}", to_be_banned_username)
    }))
}

pub async fn me(req: HttpRequest, app_data: WebAppData) -> ServiceResult<impl Responder> {
    let user = match app_data.auth.get_user_from_request(&req).await {
        Ok(user) => Ok(user),
        Err(e) => Err(e)
    }?;

    let username = user.username.clone();
    let token = app_data.auth.sign_jwt(user.clone()).await;

    Ok(HttpResponse::Ok().json(OkResponse {
        data: TokenResponse {
            token,
            username,
            admin: user.administrator
        }
    }))
}
