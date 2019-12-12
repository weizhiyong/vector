use futures::{Future, Stream};
pub use goauth::scopes::Scope;
use goauth::{auth::JwtClaims, auth::Token, credentials::Credentials, error::GOErr};
use hyper::{
    header::{HeaderValue, AUTHORIZATION},
    Request,
};
use serde::{Deserialize, Serialize};
use smpl_jwt::Jwt;
use snafu::{ResultExt, Snafu};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::timer::Interval;

#[derive(Debug, Snafu)]
enum GcpError {
    #[snafu(display("This requires one of api_key or credentials_path to be defined"))]
    MissingAuth,
    #[snafu(display("Invalid GCP credentials"))]
    InvalidCredentials { source: GOErr },
    #[snafu(display("Invalid RSA key in GCP credentials"))]
    InvalidRsaKey { source: GOErr },
    #[snafu(display("Failed to get OAuth token"))]
    GetTokenFailed { source: GOErr },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GcpAuthConfig {
    pub api_key: Option<String>,
    pub credentials_path: Option<String>,
}

impl GcpAuthConfig {
    pub fn make_credentials(&self, scope: Scope) -> crate::Result<Option<GcpCredentials>> {
        if self.api_key.is_none() && self.credentials_path.is_none() {
            Err(GcpError::MissingAuth.into())
        } else {
            Ok(match self.credentials_path.as_ref() {
                Some(path) => Some(GcpCredentials::new(path, scope)?),
                None => None,
            })
        }
    }
}

#[derive(Clone)]
pub struct GcpCredentials {
    creds: Credentials,
    scope: Arc<Scope>, // This `Arc` is only needed because goauth `Scope` doesn't yet support `Clone`.
    token: Arc<RwLock<Token>>,
}

impl GcpCredentials {
    pub fn new(path: &str, scope: Scope) -> crate::Result<Self> {
        let creds = Credentials::from_file(path).context(InvalidCredentials)?;
        let jwt = make_jwt(&creds, &scope)?;
        let token = goauth::get_token_with_creds(&jwt, &creds).context(GetTokenFailed)?;
        let token = Arc::new(RwLock::new(token));
        let scope = Arc::new(scope);
        Ok(Self {
            creds,
            scope,
            token,
        })
    }

    pub fn apply<T>(&self, request: &mut Request<T>) {
        let token = self.token.read().unwrap();
        let value = format!("{} {}", token.token_type(), token.access_token());
        request
            .headers_mut()
            .insert(AUTHORIZATION, HeaderValue::from_str(&value).unwrap());
    }

    fn regenerate_token(&self) -> crate::Result<()> {
        let jwt = make_jwt(&self.creds, &self.scope).unwrap(); // Errors caught above
        let token = goauth::get_token_with_creds(&jwt, &self.creds)?;
        *self.token.write().unwrap() = token;
        Ok(())
    }

    pub fn spawn_regenerate_token(&self) {
        let interval = self.token.read().unwrap().expires_in() as u64 / 2;
        let copy = self.clone();
        let renew_task = Interval::new_interval(Duration::from_secs(interval))
            .for_each(move |_instant| {
                debug!("Renewing GCP authentication token");
                if let Err(error) = copy.regenerate_token() {
                    error!(message = "Failed to update GCP authentication token", %error);
                }
                Ok(())
            })
            .map_err(
                |error| error!(message = "GCP authentication token regenerate interval failed", %error),
            );

        tokio::spawn(renew_task);
    }
}

fn make_jwt(creds: &Credentials, scope: &Scope) -> crate::Result<Jwt<JwtClaims>> {
    let claims = JwtClaims::new(creds.iss(), scope, creds.token_uri(), None, None);
    let rsa_key = creds.rsa_key().context(InvalidRsaKey)?;
    Ok(Jwt::new(claims, rsa_key, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert_downcast_matches;

    #[test]
    fn fails_missing_creds() {
        let config: GcpAuthConfig = toml::from_str("").unwrap();
        match config.make_credentials(Scope::Compute) {
            Ok(_) => panic!("make_credentials failed to error"),
            Err(err) => assert_downcast_matches!(err, GcpError, GcpError::MissingAuth),
        }
    }
}
