//! JWT authentication: password hashing, login, and bearer-token middleware.
//!
//! Token/claims conventions match tiletopia-server (HS256, `sub`/`exp`/`role`,
//! 24h expiry) so tokens look the same across GeoLang services. Passwords are
//! hashed with argon2id instead of tiletopia's salted HMAC-SHA256, and the
//! signing secret is mandatory: there is no unauthenticated fallback mode.

use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::Extension;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{Json, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::store::UserRecord;

pub const TOKEN_TTL_HOURS: i64 = 24;

/// JWT claims (same shape as tiletopia's).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    /// Role name, one of the strings [`Role::parse`] accepts.
    pub role: String,
}

/// What an account may do. Stored as a string in the users table and copied
/// into the `role` claim.
///
/// A string that is not one of these does not parse, so a typo in `create-user`
/// or a role from some future version grants nothing rather than defaulting to
/// something permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    Editor,
    Viewer,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "admin" => Some(Role::Admin),
            "editor" => Some(Role::Editor),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    /// May create forms and file submissions. Viewers are read-only.
    pub fn can_write(self) -> bool {
        matches!(self, Role::Admin | Role::Editor)
    }

    pub fn is_admin(self) -> bool {
        matches!(self, Role::Admin)
    }
}

/// The user behind a bearer-token request.
///
/// Handlers read this from request extensions. It is inserted only after the
/// token verified and both `sub` and `role` parsed, so holding one is proof the
/// caller is authenticated with a role this server understands.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub id: Uuid,
    pub role: Role,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    pub token: String,
}

pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hashing cannot fail with default params")
        .to_string()
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

// verified against when the email is unknown, so login latency does not
// reveal whether an account exists.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| hash_password("no-such-user"));

pub fn create_jwt(user: &UserRecord, secret: &str) -> Result<String, jsonwebtoken::errors::Error> {
    let claims = Claims {
        sub: user.id.to_string(),
        exp: (chrono::Utc::now() + chrono::Duration::hours(TOKEN_TTL_HOURS)).timestamp() as usize,
        role: user.role.clone(),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}

fn extract_claims(request: &Request, secret: &str) -> Result<Claims, StatusCode> {
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .map_err(|_| StatusCode::UNAUTHORIZED)
}

/// Middleware guarding all data endpoints: rejects requests without a valid
/// bearer token and exposes the [`Caller`] to handlers via request extensions.
///
/// A token whose `role` this server does not know is refused here rather than
/// treated as the weakest role, so every route behind this layer is closed to
/// an account nobody has assigned a valid role.
pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let claims = extract_claims(&request, &state.jwt_secret)?;
    // a sub that is not a user id cannot be a token this server minted.
    let id: Uuid = claims.sub.parse().map_err(|_| StatusCode::UNAUTHORIZED)?;
    let role = Role::parse(&claims.role).ok_or(StatusCode::FORBIDDEN)?;
    request.extensions_mut().insert(Caller { id, role });
    Ok(next.run(request).await)
}

/// Route layer for endpoints that create forms or file submissions.
///
/// Must be applied inside [`require_auth`], which is what puts the [`Caller`]
/// in the extensions.
pub async fn require_write(
    Extension(caller): Extension<Caller>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if !caller.role.can_write() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// Route layer for endpoints covering the whole instance rather than one form.
pub async fn require_admin(
    Extension(caller): Extension<Caller>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if !caller.role.is_admin() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// Realm advertised to ODK Collect in the Basic challenge.
pub const BASIC_REALM: &str = "collecta";

/// The `WWW-Authenticate` challenge for the OpenRosa routes.
///
/// ODK Collect probes unauthenticated first and only sends credentials after
/// seeing this header, so every rejection on those routes must carry it.
pub const BASIC_CHALLENGE: &str = r#"Basic realm="collecta", charset="UTF-8""#;

/// Credentials parsed out of an `Authorization: Basic` header.
pub struct BasicCredentials {
    pub user_id: String,
    pub password: String,
}

/// Parse RFC 7617 Basic credentials.
///
/// The scheme token is case-insensitive, the payload must be canonical padded
/// standard base64 decoding to utf-8, and the split is on the *first* colon:
/// user-ids cannot contain one, passwords can.
pub fn parse_basic_header(header: &str) -> Option<BasicCredentials> {
    let (scheme, payload) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    // RFC 7235 allows only spaces here, but tolerate extra whitespace rather
    // than reject a client over framing.
    let payload = payload.trim();
    let decoded = BASE64_STANDARD.decode(payload).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user_id, password) = decoded.split_once(':')?;
    Some(BasicCredentials {
        user_id: user_id.to_string(),
        password: password.to_string(),
    })
}

/// Verify Basic credentials against the users table.
///
/// An unknown user is still run through an argon2 verification so a missing
/// account and a wrong password take the same time, matching [`login`].
pub async fn verify_basic(
    store: &crate::store::Store,
    credentials: &BasicCredentials,
) -> Result<Option<UserRecord>, sqlx::Error> {
    let Some(user) = store.get_user_by_email(&credentials.user_id).await? else {
        let _ = verify_password(&credentials.password, &DUMMY_HASH);
        return Ok(None);
    };
    if !verify_password(&credentials.password, &user.password_hash) {
        return Ok(None);
    }
    Ok(Some(user))
}

/// `POST /api/v1/auth/login` — verify credentials, issue a JWT.
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<TokenResponse>, StatusCode> {
    let user = state
        .store
        .get_user_by_email(&req.email)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(user) = user else {
        let _ = verify_password(&req.password, &DUMMY_HASH);
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !verify_password(&req.password, &user.password_hash) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let token =
        create_jwt(&user, &state.jwt_secret).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(TokenResponse { token }))
}
