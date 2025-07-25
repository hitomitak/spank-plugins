//
// (C) Copyright IBM 2025
//
// This code is licensed under the Apache License, Version 2.0. You may
// obtain a copy of this license in the LICENSE.txt file in the root directory
// of this source tree or at http://www.apache.org/licenses/LICENSE-2.0.
//
// Any modifications or derivative works of this code must retain this
// copyright notice, and modified files need to carry a notice indicating
// that they have been altered from the originals.

use anyhow::{bail, Result};
#[allow(unused_imports)]
use log::{debug, error};
use reqwest::Client;
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct IAMErrorResponse {
    #[serde(rename(deserialize = "errorCode"))]
    pub code: String,
    #[serde(rename(deserialize = "errorMessage"))]
    pub message: String,
    #[serde(rename(deserialize = "errorDetails"))]
    pub details: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GetAccessTokenResponse {
    // The IAM access token that can be used to invoke Direct Access API. Use this token with the prefix Bearer in the HTTP header Authorization for invocations of Direct Access API.
    pub access_token: String,
    // Number of seconds until the IAM access token will expire.
    pub expires_in: u64,
    // Number of seconds counted since January 1st, 1970, until the IAM access token will expire. Only available in the responses from IAM.
    pub expiration: Option<u64>,
    // The type of the token. Currently, only Bearer is returned.
    pub token_type: String,
    // (optional) A refresh token that can be used to get a new IAM access token if that token is expired. Only available in the response from IAM
    pub refresh_token: Option<String>,
    // (optional) Only available in the response from IAM
    pub ims_user_id: Option<u64>,
    // (optional) Only available in the response from IAM
    pub scope: Option<String>,
}

#[derive(Debug)]
pub(crate) struct TokenManager {
    access_token: Option<String>,
    token_expiry: Option<Instant>,
    client: Client,
    token_url: String,
    apikey: String,
}
impl TokenManager {
    /// Constructs TokenManager for IBM Cloud IAM
    pub(crate) fn new(token_url: impl Into<String>, apikey: impl Into<String>) -> Self {
        Self {
            access_token: None,
            token_expiry: None,
            client: Client::new(),
            token_url: token_url.into(),
            apikey: apikey.into(),
        }
    }
    /// Fetches access token from IAM and updates self.access_token property.
    async fn fetch_token(&mut self) -> Result<()> {
        log::debug!("proxy: fetch access token from IAM");
        let params = [
            ("grant_type", "urn:ibm:params:oauth:grant-type:apikey"),
            ("apikey", &self.apikey),
            ("response_type", "cloud_iam"),
        ];
        log::debug!("proxy: token_url {}", self.token_url);
        let response = self
            .client
            .post(&self.token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .form(&params)
            .send()
            .await?;
        if response.status().is_success() {
            log::debug!("proxy: response success");
            let token_response: GetAccessTokenResponse = response.json().await?;
            self.access_token = Some(token_response.access_token);
            log::debug!("proxy: get access_token {:?}", self.access_token);
            self.token_expiry = Some(
                Instant::now()
                    + Duration::from_secs((token_response.expires_in as f64 * 0.9) as u64),
            );
        } else {
            log::debug!("proxy: response error");
            let error_response = response.json::<IAMErrorResponse>().await?;
            log::debug!("proxy: error response {:?}", error_response);
            if let Some(details) = error_response.details {
                bail!(format!("{} ({})", details, error_response.code));
            } else {
                bail!(format!(
                    "{} ({})",
                    error_response.message, error_response.code
                ));
            }
        }
        log::debug!("proxy: return ok");
        Ok(())
    }
    /// Checks token validity and fetches if expired.
    async fn ensure_token_validity(&mut self) -> Result<()> {
        if self.access_token.is_none()
            || self.token_expiry.unwrap_or_else(Instant::now) <= Instant::now()
        {
            self.fetch_token().await?;
        }
        Ok(())
    }
    /// Returns the access token to caller
    pub(crate) async fn get_token(&mut self) -> Result<String> {
        self.ensure_token_validity().await?;
        Ok(self.access_token.clone().unwrap())
    }
}
