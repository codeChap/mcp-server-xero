use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::auth::{self, StoredTokens};

const TOKEN_URL: &str = "https://identity.xero.com/connect/token";
const CONNECTIONS_URL: &str = "https://api.xero.com/connections";
const BASE_URL: &str = "https://api.xero.com/api.xro/2.0";
/// Finance API (Bank Statements Plus, Cash Validation, etc.).
const FINANCE_BASE_URL: &str = "https://api.xero.com/finance.xro/1.0";

/// Token expires in 30 minutes; refresh with 60s buffer.
const TOKEN_EXPIRY_BUFFER_SECS: u64 = 60;

/// How many times to send a request (first try + retries) before surfacing 429.
const MAX_ATTEMPTS: u32 = 5;
/// Base backoff when Xero omits `Retry-After` (doubles each attempt).
const BASE_BACKOFF_MS: u64 = 1_000;
/// Cap on computed backoff (and on a huge `Retry-After`).
const MAX_BACKOFF_MS: u64 = 30_000;

#[derive(Error, Debug)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("Xero API error ({status}): {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },

    #[error("Auth error: {0}")]
    Auth(String),

    /// A precondition on our side failed — the request was never sent.
    #[error("{0}")]
    Invalid(String),
}

#[derive(Deserialize)]
struct ClientCredentialsResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct Connection {
    #[serde(rename = "tenantId")]
    tenant_id: String,
}

struct CachedAuth {
    access_token: String,
    tenant_id: Option<String>,
    expires_at: Instant,
}

pub enum AuthMode {
    /// Custom Connection: client_credentials grant.
    ClientCredentials { scopes: Option<String> },
    /// Web App: authorization code flow with stored refresh tokens.
    AuthorizationCode { tokens: StoredTokens },
}

pub struct XeroClient {
    client_id: String,
    client_secret: String,
    mode: RwLock<AuthMode>,
    http: Client,
    auth: Arc<RwLock<Option<CachedAuth>>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Backoff for a 429: prefer `Retry-After` (seconds), else exponential from attempt number.
fn rate_limit_backoff_ms(attempt: u32, headers: &reqwest::header::HeaderMap) -> u64 {
    if let Some(val) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(secs) = val.parse::<u64>() {
            return secs.saturating_mul(1_000).min(MAX_BACKOFF_MS);
        }
        // HTTP-date form is rare from Xero; fall through to exponential.
    }

    // attempt 1 → 1s, 2 → 2s, 3 → 4s, 4 → 8s, …
    let shift = attempt.saturating_sub(1).min(4);
    BASE_BACKOFF_MS
        .saturating_mul(1u64 << shift)
        .min(MAX_BACKOFF_MS)
}

impl XeroClient {
    pub fn new(client_id: String, client_secret: String, mode: AuthMode) -> Self {
        Self {
            client_id,
            client_secret,
            mode: RwLock::new(mode),
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("Failed to build reqwest client"),
            auth: Arc::new(RwLock::new(None)),
        }
    }

    /// Get a valid access token, refreshing if needed.
    async fn ensure_auth(&self) -> Result<(String, Option<String>), ApiError> {
        // Check if we have a valid cached token.
        {
            let guard = self.auth.read().await;
            if let Some(ref cached) = *guard {
                if Instant::now() < cached.expires_at {
                    return Ok((cached.access_token.clone(), cached.tenant_id.clone()));
                }
            }
        }

        // Need to refresh — acquire write lock.
        let mut guard = self.auth.write().await;

        // Double-check after acquiring write lock.
        if let Some(ref cached) = *guard {
            if Instant::now() < cached.expires_at {
                return Ok((cached.access_token.clone(), cached.tenant_id.clone()));
            }
        }

        let mode = self.mode.read().await;
        match &*mode {
            AuthMode::ClientCredentials { scopes } => {
                self.auth_client_credentials(&mut guard, scopes.as_deref())
                    .await
            }
            AuthMode::AuthorizationCode { tokens } => {
                let tokens = tokens.clone();
                drop(mode); // Release read lock before potentially needing write.
                self.auth_refresh_token(&mut guard, &tokens).await
            }
        }
    }

    async fn auth_client_credentials(
        &self,
        guard: &mut Option<CachedAuth>,
        scopes: Option<&str>,
    ) -> Result<(String, Option<String>), ApiError> {
        debug!("requesting new OAuth2 token (client_credentials)");

        let credentials = BASE64.encode(format!("{}:{}", self.client_id, self.client_secret));

        let body = match scopes {
            Some(s) => format!(
                "grant_type=client_credentials&scope={}",
                urlencoding::encode(s)
            ),
            None => "grant_type=client_credentials".to_string(),
        };

        let resp = self
            .http
            .post(TOKEN_URL)
            .header("Authorization", format!("Basic {credentials}"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Auth(format!(
                "Token request failed ({status}): {body}"
            )));
        }

        let token_resp: ClientCredentialsResponse = resp.json().await?;
        let expires_at = Instant::now()
            + Duration::from_secs(
                token_resp
                    .expires_in
                    .saturating_sub(TOKEN_EXPIRY_BUFFER_SECS),
            );

        // Try to discover tenant ID.
        let tenant_id = match self
            .http
            .get(CONNECTIONS_URL)
            .header(
                "Authorization",
                format!("Bearer {}", token_resp.access_token),
            )
            .send()
            .await
        {
            Ok(resp) => {
                let connections: Vec<Connection> = resp.json().await.unwrap_or_default();
                connections.first().map(|c| c.tenant_id.clone())
            }
            Err(_) => None,
        };

        debug!(tenant_id = ?tenant_id, "authenticated (client_credentials)");

        *guard = Some(CachedAuth {
            access_token: token_resp.access_token.clone(),
            tenant_id: tenant_id.clone(),
            expires_at,
        });

        Ok((token_resp.access_token, tenant_id))
    }

    async fn auth_refresh_token(
        &self,
        guard: &mut Option<CachedAuth>,
        stored: &StoredTokens,
    ) -> Result<(String, Option<String>), ApiError> {
        // If access token hasn't expired (by epoch time), use it directly.
        if now_secs() < stored.expires_at.saturating_sub(TOKEN_EXPIRY_BUFFER_SECS) {
            let expires_in = stored.expires_at.saturating_sub(now_secs());
            let expires_at = Instant::now() + Duration::from_secs(expires_in);

            *guard = Some(CachedAuth {
                access_token: stored.access_token.clone(),
                tenant_id: Some(stored.tenant_id.clone()),
                expires_at,
            });

            return Ok((stored.access_token.clone(), Some(stored.tenant_id.clone())));
        }

        debug!("refreshing OAuth2 token");

        let updated =
            auth::refresh_tokens(&self.http, &self.client_id, &self.client_secret, stored)
                .await
                .map_err(|e| ApiError::Auth(e.to_string()))?;

        let expires_in = updated.expires_at.saturating_sub(now_secs());
        let expires_at = Instant::now() + Duration::from_secs(expires_in);

        debug!(tenant_id = %updated.tenant_id, "authenticated (refresh_token)");

        *guard = Some(CachedAuth {
            access_token: updated.access_token.clone(),
            tenant_id: Some(updated.tenant_id.clone()),
            expires_at,
        });

        // Update the stored tokens so subsequent refreshes use the latest refresh_token.
        let mut mode = self.mode.write().await;
        *mode = AuthMode::AuthorizationCode {
            tokens: updated.clone(),
        };

        Ok((updated.access_token, Some(updated.tenant_id)))
    }

    /// Make an authenticated request to the Xero Accounting API.
    ///
    /// Retries on HTTP 429 (rate limit), honouring `Retry-After` when present,
    /// otherwise exponential backoff. Other non-success statuses fail immediately.
    pub async fn request<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Req>,
    ) -> Result<Resp, ApiError> {
        self.request_at(BASE_URL, method, path, body).await
    }

    /// Authenticated request against the Xero Finance API
    /// (`https://api.xero.com/finance.xro/1.0`).
    ///
    /// Needs `finance.bankstatementsplus.read` / `finance.cashvalidation.read`
    /// (or broader finance scopes) on the token. Same 429 retry policy as
    /// [`Self::request`].
    pub async fn finance_request<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Req>,
    ) -> Result<Resp, ApiError> {
        self.request_at(FINANCE_BASE_URL, method, path, body).await
    }

    async fn request_at<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        base: &str,
        method: Method,
        path: &str,
        body: Option<&Req>,
    ) -> Result<Resp, ApiError> {
        let url = format!("{base}{path}");
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            let (token, tenant_id) = self.ensure_auth().await?;

            let mut builder = self
                .http
                .request(method.clone(), &url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", "application/json");

            if let Some(ref tid) = tenant_id {
                builder = builder.header("xero-tenant-id", tid);
            }

            if let Some(b) = body {
                builder = builder.json(b);
            }

            let response = builder.send().await?;
            let status = response.status();

            if status.as_u16() == 429 && attempt < MAX_ATTEMPTS {
                let backoff_ms = rate_limit_backoff_ms(attempt, response.headers());
                // Drain the body so the connection can be reused.
                let _ = response.text().await;
                warn!(
                    attempt,
                    backoff_ms, path, "Xero rate limited (429), retrying"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                continue;
            }

            if !status.is_success() {
                let err_body = response.text().await.unwrap_or_default();
                warn!(status = %status, "Xero API request failed");
                return Err(ApiError::Api {
                    status,
                    body: err_body,
                });
            }

            return Ok(response.json::<Resp>().await?);
        }
    }

    /// POST with empty body, returning only success/failure (for email endpoints).
    ///
    /// Same 429 retry policy as [`Self::request`].
    pub async fn post_empty(&self, path: &str) -> Result<(), ApiError> {
        let url = format!("{BASE_URL}{path}");
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            let (token, tenant_id) = self.ensure_auth().await?;

            let mut builder = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", "application/json");

            if let Some(ref tid) = tenant_id {
                builder = builder.header("xero-tenant-id", tid);
            }

            let response = builder.send().await?;
            let status = response.status();

            if status.as_u16() == 429 && attempt < MAX_ATTEMPTS {
                let backoff_ms = rate_limit_backoff_ms(attempt, response.headers());
                let _ = response.text().await;
                warn!(
                    attempt,
                    backoff_ms, path, "Xero rate limited (429), retrying"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                continue;
            }

            if !status.is_success() {
                let err_body = response.text().await.unwrap_or_default();
                warn!(status = %status, "Xero API request failed");
                return Err(ApiError::Api {
                    status,
                    body: err_body,
                });
            }

            return Ok(());
        }
    }

    /// Make a GET request returning raw JSON (for reports and flexible responses).
    pub async fn get_json(&self, path: &str) -> Result<serde_json::Value, ApiError> {
        self.request::<(), serde_json::Value>(Method::GET, path, None)
            .await
    }

    /// Set the tax type on every line of an existing bank transaction.
    ///
    /// Fetches the transaction, rewrites only `TaxType`, and posts it back. The
    /// returned summaries let a caller assert the money did not move: on a
    /// tax-inclusive transaction, changing the rate must leave `Total` untouched
    /// and shift the tax into the expense account instead.
    ///
    /// With `dry_run`, the payload is built and validated but never sent.
    pub async fn set_bank_transaction_tax_type(
        &self,
        id: &str,
        tax_type: &str,
        dry_run: bool,
    ) -> Result<TaxTypeUpdate, ApiError> {
        let path = format!("/BankTransactions/{id}");
        let fetched = self.get_json(&path).await?;
        let txn = single_bank_transaction(&fetched)
            .ok_or_else(|| ApiError::Invalid(format!("no bank transaction found for id {id}")))?;

        let before = TxnSummary::read(txn);
        let payload = build_tax_type_payload(txn, tax_type)?;

        if dry_run {
            return Ok(TaxTypeUpdate {
                before,
                after: None,
                payload,
            });
        }

        let resp: Value = self.request(Method::POST, &path, Some(&payload)).await?;
        let updated = single_bank_transaction(&resp).ok_or_else(|| {
            ApiError::Invalid("update succeeded but the response contained no transaction".into())
        })?;

        Ok(TaxTypeUpdate {
            before,
            after: Some(TxnSummary::read(updated)),
            payload,
        })
    }
}

/// First transaction out of a `{"BankTransactions": [...]}` envelope.
fn single_bank_transaction(v: &Value) -> Option<&Value> {
    v.get("BankTransactions")?.as_array()?.first()
}

// ---------------------------------------------------------------------------
// Xero API response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct InvoicesResponse {
    pub invoices: Vec<Invoice>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Invoice {
    #[serde(default, rename = "InvoiceID")]
    pub invoice_id: Option<String>,
    #[serde(default)]
    pub invoice_number: Option<String>,
    #[serde(rename = "Type")]
    #[serde(default)]
    pub invoice_type: Option<String>,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(default)]
    pub contact: Option<ContactRef>,
    #[serde(default)]
    pub sub_total: Option<f64>,
    #[serde(default)]
    pub total_tax: Option<f64>,
    #[serde(default)]
    pub total: Option<f64>,
    #[serde(default)]
    pub amount_due: Option<f64>,
    #[serde(default)]
    pub amount_paid: Option<f64>,
    #[serde(default)]
    pub currency_code: Option<String>,
    #[serde(default)]
    pub line_items: Option<Vec<LineItem>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct ContactRef {
    #[serde(default, rename = "ContactID")]
    pub contact_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct LineItem {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub quantity: Option<f64>,
    #[serde(default)]
    pub unit_amount: Option<f64>,
    #[serde(default)]
    pub account_code: Option<String>,
    #[serde(default)]
    pub tax_type: Option<String>,
    #[serde(default)]
    pub line_amount: Option<f64>,
    #[serde(default)]
    pub item_code: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct ContactsResponse {
    pub contacts: Vec<Contact>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Contact {
    #[serde(default, rename = "ContactID")]
    pub contact_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub email_address: Option<String>,
    #[serde(default)]
    pub contact_status: Option<String>,
    #[serde(default)]
    pub is_supplier: Option<bool>,
    #[serde(default)]
    pub is_customer: Option<bool>,
    #[serde(default)]
    pub phones: Option<Vec<Phone>>,
    #[serde(default)]
    pub addresses: Option<Vec<Address>>,
    #[serde(default)]
    pub tax_number: Option<String>,
    #[serde(default)]
    pub account_number: Option<String>,
    #[serde(default)]
    pub company_number: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Phone {
    #[serde(default)]
    pub phone_type: Option<String>,
    #[serde(default)]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub phone_area_code: Option<String>,
    #[serde(default)]
    pub phone_country_code: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Address {
    #[serde(default)]
    pub address_type: Option<String>,
    #[serde(default)]
    pub address_line1: Option<String>,
    #[serde(default)]
    pub address_line2: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct AccountsResponse {
    pub accounts: Vec<Account>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Account {
    #[serde(default, rename = "AccountID")]
    pub account_id: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "Type")]
    #[serde(default)]
    pub account_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub tax_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct ItemsResponse {
    pub items: Vec<Item>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    #[serde(default, rename = "ItemID")]
    pub item_id: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub purchase_description: Option<String>,
    #[serde(default)]
    pub is_sold: Option<bool>,
    #[serde(default)]
    pub is_purchased: Option<bool>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct PaymentsResponse {
    pub payments: Vec<Payment>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Payment {
    #[serde(default, rename = "PaymentID")]
    pub payment_id: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub invoice: Option<InvoiceRef>,
    #[serde(default)]
    pub account: Option<AccountRef>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct InvoiceRef {
    #[serde(default, rename = "InvoiceID")]
    pub invoice_id: Option<String>,
    #[serde(default)]
    pub invoice_number: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct AccountRef {
    #[serde(default, rename = "AccountID")]
    pub account_id: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct CreditNotesResponse {
    pub credit_notes: Vec<CreditNote>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct CreditNote {
    #[serde(default, rename = "CreditNoteID")]
    pub credit_note_id: Option<String>,
    #[serde(default)]
    pub credit_note_number: Option<String>,
    #[serde(rename = "Type")]
    #[serde(default)]
    pub credit_note_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub contact: Option<ContactRef>,
    #[serde(default)]
    pub total: Option<f64>,
    #[serde(default)]
    pub remaining_credit: Option<f64>,
    #[serde(default)]
    pub line_items: Option<Vec<LineItem>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct QuotesResponse {
    pub quotes: Vec<Quote>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Quote {
    #[serde(default, rename = "QuoteID")]
    pub quote_id: Option<String>,
    #[serde(default)]
    pub quote_number: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub expiry_date: Option<String>,
    #[serde(default)]
    pub contact: Option<ContactRef>,
    #[serde(default)]
    pub sub_total: Option<f64>,
    #[serde(default)]
    pub total_tax: Option<f64>,
    #[serde(default)]
    pub total: Option<f64>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub line_items: Option<Vec<LineItem>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct TaxRatesResponse {
    pub tax_rates: Vec<TaxRate>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct TaxRate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tax_type: Option<String>,
    #[serde(default)]
    pub effective_rate: Option<f64>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct OrganisationsResponse {
    pub organisations: Vec<Organisation>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Organisation {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub legal_name: Option<String>,
    #[serde(default)]
    pub short_code: Option<String>,
    #[serde(default)]
    pub organisation_type: Option<String>,
    #[serde(default)]
    pub base_currency: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub financial_year_end_day: Option<u32>,
    #[serde(default)]
    pub financial_year_end_month: Option<u32>,
    #[serde(default)]
    pub tax_number: Option<String>,
    #[serde(default)]
    pub line_of_business: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct TrackingCategoriesResponse {
    pub tracking_categories: Vec<TrackingCategory>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct TrackingCategory {
    #[serde(default, rename = "TrackingCategoryID")]
    pub tracking_category_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub options: Option<Vec<TrackingOption>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct TrackingOption {
    #[serde(default, rename = "TrackingOptionID")]
    pub tracking_option_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct BankTransactionsResponse {
    pub bank_transactions: Vec<BankTransaction>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct BankTransaction {
    #[serde(default, rename = "BankTransactionID")]
    pub bank_transaction_id: Option<String>,
    #[serde(rename = "Type")]
    #[serde(default)]
    pub transaction_type: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub contact: Option<ContactRef>,
    #[serde(default)]
    pub line_items: Option<Vec<LineItem>>,
    #[serde(default)]
    pub sub_total: Option<f64>,
    #[serde(default)]
    pub total_tax: Option<f64>,
    #[serde(default)]
    pub total: Option<f64>,
    #[serde(default)]
    pub bank_account: Option<AccountRef>,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub is_reconciled: Option<bool>,
}

/// What a bank transaction looked like at a point in time — enough to prove a tax
/// change moved the tax and nothing else.
#[derive(Debug, Clone, PartialEq)]
pub struct TxnSummary {
    pub contact: String,
    pub line_count: usize,
    pub tax_types: Vec<String>,
    pub sub_total: Option<f64>,
    pub total_tax: Option<f64>,
    pub total: Option<f64>,
}

impl TxnSummary {
    fn read(txn: &Value) -> Self {
        let lines = txn.get("LineItems").and_then(Value::as_array);
        Self {
            contact: txn
                .pointer("/Contact/Name")
                .and_then(Value::as_str)
                .unwrap_or("—")
                .to_string(),
            line_count: lines.map_or(0, Vec::len),
            tax_types: lines
                .map(|l| {
                    l.iter()
                        .filter_map(|li| li.get("TaxType").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            sub_total: txn.get("SubTotal").and_then(Value::as_f64),
            total_tax: txn.get("TotalTax").and_then(Value::as_f64),
            total: txn.get("Total").and_then(Value::as_f64),
        }
    }
}

/// Outcome of a tax-type change. `after` is `None` for a dry run.
#[derive(Debug)]
pub struct TaxTypeUpdate {
    pub before: TxnSummary,
    pub after: Option<TxnSummary>,
    pub payload: Value,
}

/// Build the payload that sets `tax_type` on every line of a fetched bank transaction.
///
/// Xero's `POST /BankTransactions` is destructive on line items. Per the API docs:
/// "Not providing an existing LineItem with it's LineItemID will result in that line
/// item being deleted", and "If LineItemIDs are not included with line items in an
/// update request then the line items are deleted and recreated."
///
/// Two consequences shape this function. It rewrites the *raw JSON* rather than
/// round-tripping through `LineItem`, so fields this crate does not model (Tracking,
/// discounts, item codes) survive verbatim instead of being dropped and recreated as
/// null. And it refuses outright if any line lacks a `LineItemID`, rather than send a
/// request Xero would service by deleting that line.
///
/// `TaxAmount` is removed so Xero recalculates it; the docs require any `TaxAmount`
/// sent to equal Xero's own calculation, which the pre-change value will not.
pub fn build_tax_type_payload(txn: &Value, tax_type: &str) -> Result<Value, ApiError> {
    let id = txn
        .get("BankTransactionID")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Invalid("bank transaction has no BankTransactionID".into()))?;

    let src = txn
        .get("LineItems")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::Invalid(format!("bank transaction {id} has no LineItems")))?;

    if src.is_empty() {
        return Err(ApiError::Invalid(format!(
            "bank transaction {id} has no line items to update"
        )));
    }

    let mut lines = Vec::with_capacity(src.len());
    for (i, line) in src.iter().enumerate() {
        let mut obj = line
            .as_object()
            .ok_or_else(|| ApiError::Invalid(format!("line {i} of {id} is not an object")))?
            .clone();

        if !obj.get("LineItemID").is_some_and(Value::is_string) {
            return Err(ApiError::Invalid(format!(
                "line {i} of bank transaction {id} has no LineItemID — refusing to \
                 update, because Xero would delete and recreate the line rather than \
                 amend it"
            )));
        }

        obj.insert("TaxType".into(), Value::String(tax_type.to_string()));
        obj.remove("TaxAmount");
        lines.push(Value::Object(obj));
    }

    let mut payload = json!({
        "BankTransactionID": id,
        "LineItems": lines,
    });

    // Pin the tax basis. Xero defaults an absent LineAmountTypes to Inclusive, which
    // would silently restate the totals of an Exclusive transaction.
    if let Some(basis) = txn.get("LineAmountTypes") {
        payload["LineAmountTypes"] = basis.clone();
    }

    Ok(payload)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

pub fn format_invoice(inv: &Invoice) -> String {
    let num = inv.invoice_number.as_deref().unwrap_or("—");
    let status = inv.status.as_deref().unwrap_or("—");
    let contact = inv
        .contact
        .as_ref()
        .and_then(|c| c.name.as_deref())
        .unwrap_or("—");
    let total = inv.total.map(|t| format!("{t:.2}")).unwrap_or("—".into());
    let due = inv.due_date.as_deref().unwrap_or("—");
    let inv_type = inv.invoice_type.as_deref().unwrap_or("—");
    let reference = inv.reference.as_deref().unwrap_or("");
    let ref_str = if reference.is_empty() {
        String::new()
    } else {
        format!(" ref:{reference}")
    };

    format!("{num} | {inv_type} | {status} | {contact} | {total} | due:{due}{ref_str}")
}

pub fn format_contact(c: &Contact) -> String {
    let name = c.name.as_deref().unwrap_or("—");
    let email = c.email_address.as_deref().unwrap_or("—");
    let status = c.contact_status.as_deref().unwrap_or("—");
    let id = c.contact_id.as_deref().unwrap_or("—");
    format!("{name} | {email} | {status} | id:{id}")
}

pub fn format_account(a: &Account) -> String {
    let code = a.code.as_deref().unwrap_or("—");
    let name = a.name.as_deref().unwrap_or("—");
    let acct_type = a.account_type.as_deref().unwrap_or("—");
    let status = a.status.as_deref().unwrap_or("—");
    let id = a.account_id.as_deref().unwrap_or("—");
    format!("{code} | {name} | {acct_type} | {status} | id:{id}")
}

/// Distinct values of a line-item field, in first-seen order. A split transaction
/// can hit several accounts or tax rates, so collapsing to one value would misreport it.
fn distinct_line_values<'a>(
    lines: Option<&'a [LineItem]>,
    pick: impl Fn(&'a LineItem) -> Option<&'a str>,
) -> String {
    let Some(lines) = lines else {
        return "—".into();
    };
    let mut seen: Vec<&str> = Vec::new();
    for v in lines.iter().filter_map(pick) {
        if !seen.contains(&v) {
            seen.push(v);
        }
    }
    if seen.is_empty() {
        "—".into()
    } else {
        seen.join(",")
    }
}

pub fn format_bank_transaction(bt: &BankTransaction) -> String {
    let date = bt.date.as_deref().unwrap_or("—");
    let bt_type = bt.transaction_type.as_deref().unwrap_or("—");
    let total = bt.total.map(|t| format!("{t:.2}")).unwrap_or("—".into());
    let contact = bt
        .contact
        .as_ref()
        .and_then(|c| c.name.as_deref())
        .unwrap_or("—");
    let status = bt.status.as_deref().unwrap_or("—");
    let tax = bt
        .total_tax
        .map(|t| format!("{t:.2}"))
        .unwrap_or("—".into());
    let accounts = distinct_line_values(bt.line_items.as_deref(), |li| li.account_code.as_deref());
    let tax_types = distinct_line_values(bt.line_items.as_deref(), |li| li.tax_type.as_deref());
    // The id is what set_bank_transaction_tax_type takes; without it here, a caller
    // would have to leave the session to find one.
    let id = bt.bank_transaction_id.as_deref().unwrap_or("—");

    let recon = match bt.is_reconciled {
        Some(true) => "recon:Y",
        Some(false) => "recon:N",
        None => "recon:?",
    };

    format!(
        "{date} | {bt_type} | {total} | {contact} | {status} | {recon} | acct:{accounts} | tax:{tax_types} | vat:{tax} | id:{id}"
    )
}

fn money(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_else(|| "—".into())
}

fn moved(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() > 0.005,
        _ => false,
    }
}

fn describe(s: &TxnSummary) -> String {
    let types = if s.tax_types.is_empty() {
        "—".to_string()
    } else {
        s.tax_types.join(",")
    };
    format!(
        "{} line(s) | tax:{} | sub:{} vat:{} total:{}",
        s.line_count,
        types,
        money(s.sub_total),
        money(s.total_tax),
        money(s.total)
    )
}

pub fn format_tax_type_update(u: &TaxTypeUpdate, dry_run: bool) -> String {
    let b = &u.before;

    let Some(a) = &u.after else {
        return format!(
            "DRY RUN — nothing was sent to Xero.\n\
             {} | {}\n\nPayload that would be POSTed:\n{}",
            b.contact,
            describe(b),
            serde_json::to_string_pretty(&u.payload).unwrap_or_else(|_| "—".into())
        );
    };

    let mut out = format!(
        "Updated {}{}\n  before | {}\n  after  | {}",
        a.contact,
        if dry_run { " (dry run)" } else { "" },
        describe(b),
        describe(a)
    );

    // Xero deletes line items that come back without their LineItemID. The payload
    // builder guards against sending such a payload, but verify against what Xero
    // actually stored rather than trusting the guard.
    if a.line_count != b.line_count {
        out.push_str(&format!(
            "\n\nWARNING: line count changed {} -> {}. Lines may have been dropped — \
             inspect this transaction in Xero.",
            b.line_count, a.line_count
        ));
    }
    if moved(b.total, a.total) {
        out.push_str(&format!(
            "\n\nWARNING: total changed {} -> {}. Changing the tax type of a \
             tax-inclusive transaction should move tax between accounts, not change \
             what left the bank.",
            money(b.total),
            money(a.total)
        ));
    }

    out
}

pub fn format_payment(p: &Payment) -> String {
    let date = p.date.as_deref().unwrap_or("—");
    let amount = p.amount.map(|a| format!("{a:.2}")).unwrap_or("—".into());
    let status = p.status.as_deref().unwrap_or("—");
    let inv_num = p
        .invoice
        .as_ref()
        .and_then(|i| i.invoice_number.as_deref())
        .unwrap_or("—");
    format!("{date} | {amount} | {status} | inv:{inv_num}")
}

// ---------------------------------------------------------------------------
// Finance API — Bank Statements Plus / Cash Validation
// (camelCase JSON, unlike Accounting API's PascalCase)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BankStatementAccountingResponse {
    #[serde(default)]
    pub bank_account_id: Option<String>,
    #[serde(default)]
    pub bank_account_name: Option<String>,
    #[serde(default)]
    pub bank_account_currency_code: Option<String>,
    #[serde(default)]
    pub statements: Vec<FinanceStatement>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FinanceStatement {
    #[serde(default)]
    pub statement_id: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default)]
    pub import_source: Option<String>,
    #[serde(default)]
    pub start_balance: Option<f64>,
    #[serde(default)]
    pub end_balance: Option<f64>,
    #[serde(default)]
    pub statement_lines: Vec<StatementLine>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatementLine {
    #[serde(default)]
    pub statement_line_id: Option<String>,
    #[serde(default)]
    pub posted_date: Option<String>,
    #[serde(default)]
    pub transaction_date: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub payee: Option<String>,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub is_reconciled: Option<bool>,
    #[serde(default)]
    pub is_deleted: Option<bool>,
    #[serde(default)]
    pub is_duplicate: Option<bool>,
    #[serde(default)]
    pub cheque_no: Option<String>,
    #[serde(default)]
    pub bank_transactions: Option<Vec<FinanceBankTxnRef>>,
    #[serde(default)]
    pub payments: Option<Vec<FinancePaymentRef>>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FinanceBankTxnRef {
    #[serde(default)]
    pub bank_transaction_id: Option<String>,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub contact: Option<FinanceContactRef>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FinancePaymentRef {
    #[serde(default)]
    pub payment_id: Option<String>,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub date: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FinanceContactRef {
    #[serde(default)]
    pub contact_id: Option<String>,
    #[serde(default)]
    pub contact_name: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CashValidationItem {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub statement_balance_date: Option<String>,
    #[serde(default)]
    pub statement_balance: Option<CashStatementBalance>,
    #[serde(default)]
    pub bank_statement: Option<CashBankStatementSummary>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CashStatementBalance {
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub debit_or_credit: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CashBankStatementSummary {
    #[serde(default)]
    pub current_statement: Option<CashCurrentStatement>,
    #[serde(default)]
    pub statement_lines: Option<CashStatementLinesSummary>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CashCurrentStatement {
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default)]
    pub start_balance: Option<f64>,
    #[serde(default)]
    pub end_balance: Option<f64>,
    #[serde(default)]
    pub import_source_type: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CashStatementLinesSummary {
    #[serde(default)]
    pub unreconciled_amount_pos: Option<f64>,
    #[serde(default)]
    pub unreconciled_amount_neg: Option<f64>,
    #[serde(default)]
    pub unreconciled_lines: Option<i64>,
    #[serde(default)]
    pub reconciled_amount_pos: Option<f64>,
    #[serde(default)]
    pub reconciled_amount_neg: Option<f64>,
    #[serde(default)]
    pub reconciled_lines: Option<i64>,
    #[serde(default)]
    pub earliest_unreconciled_transaction: Option<String>,
    #[serde(default)]
    pub latest_unreconciled_transaction: Option<String>,
    #[serde(default)]
    pub avg_days_unreconciled_pos: Option<f64>,
    #[serde(default)]
    pub avg_days_unreconciled_neg: Option<f64>,
}

pub fn format_statement_line(line: &StatementLine) -> String {
    let date = line
        .transaction_date
        .as_deref()
        .or(line.posted_date.as_deref())
        .unwrap_or("—");
    let amount = line
        .amount
        .map(|a| format!("{a:.2}"))
        .unwrap_or_else(|| "—".into());
    let payee = line.payee.as_deref().unwrap_or("—");
    let reference = line.reference.as_deref().unwrap_or("");
    let notes = line.notes.as_deref().unwrap_or("");
    let line_type = line.r#type.as_deref().unwrap_or("—");
    let recon = match line.is_reconciled {
        Some(true) => "recon:Y",
        Some(false) => "recon:N",
        None => "recon:?",
    };
    let mut flags = Vec::new();
    if line.is_deleted == Some(true) {
        flags.push("DELETED");
    }
    if line.is_duplicate == Some(true) {
        flags.push("DUP");
    }
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(","))
    };
    let id = line.statement_line_id.as_deref().unwrap_or("—");
    let mut matched = String::new();
    if let Some(ref txs) = line.bank_transactions {
        if !txs.is_empty() {
            let ids: Vec<&str> = txs
                .iter()
                .filter_map(|t| t.bank_transaction_id.as_deref())
                .collect();
            if !ids.is_empty() {
                matched.push_str(&format!(" bt:{}", ids.join(",")));
            }
        }
    }
    if let Some(ref pays) = line.payments {
        if !pays.is_empty() {
            let ids: Vec<&str> = pays
                .iter()
                .filter_map(|p| p.payment_id.as_deref())
                .collect();
            if !ids.is_empty() {
                matched.push_str(&format!(" pay:{}", ids.join(",")));
            }
        }
    }
    let detail = [reference, notes]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    let detail_str = if detail.is_empty() {
        String::new()
    } else {
        format!(" | {detail}")
    };

    format!(
        "{date} | {amount} | {payee} | {line_type} | {recon}{flag_str}{detail_str}{matched} | line:{id}"
    )
}

/// Flatten Bank Statements Plus into a human-readable report.
///
/// When `unreconciled_only` is true, drops reconciled (and deleted) lines.
pub fn format_bank_statements_plus(
    resp: &BankStatementAccountingResponse,
    unreconciled_only: bool,
) -> String {
    let name = resp.bank_account_name.as_deref().unwrap_or("—");
    let id = resp.bank_account_id.as_deref().unwrap_or("—");
    let ccy = resp.bank_account_currency_code.as_deref().unwrap_or("—");

    let mut out = Vec::new();
    out.push(format!("Bank account: {name} ({ccy}) id:{id}"));
    out.push(format!("Statements in range: {}", resp.statements.len()));

    let mut total_lines = 0usize;
    let mut shown = 0usize;
    let mut unrecon = 0usize;

    for stmt in &resp.statements {
        let sid = stmt.statement_id.as_deref().unwrap_or("—");
        let range = format!(
            "{} → {}",
            stmt.start_date.as_deref().unwrap_or("?"),
            stmt.end_date.as_deref().unwrap_or("?")
        );
        let src = stmt.import_source.as_deref().unwrap_or("—");
        let end_bal = stmt
            .end_balance
            .map(|b| format!("{b:.2}"))
            .unwrap_or_else(|| "—".into());

        let mut lines_out = Vec::new();
        for line in &stmt.statement_lines {
            total_lines += 1;
            let is_recon = line.is_reconciled.unwrap_or(false);
            if !is_recon {
                unrecon += 1;
            }
            if unreconciled_only && (is_recon || line.is_deleted == Some(true)) {
                continue;
            }
            lines_out.push(format_statement_line(line));
            shown += 1;
        }

        out.push(String::new());
        out.push(format!(
            "### Statement {sid} | {range} | source:{src} | end_bal:{end_bal} | {} line(s) shown",
            lines_out.len()
        ));
        if lines_out.is_empty() {
            out.push(if unreconciled_only {
                "  (no unreconciled lines in this statement)".into()
            } else {
                "  (no lines)".into()
            });
        } else {
            out.extend(lines_out);
        }
    }

    out.push(String::new());
    out.push(format!(
        "Totals: {total_lines} statement line(s), {unrecon} unreconciled, {shown} shown."
    ));
    out.push(
        "NOTE: Xero does not allow reconciling (ticking off) bank feed statement lines \
         via any public API. Use create_bank_transaction / create_payment to post the \
         matching books entry; match feed lines in the Xero Reconcile UI. \
         create_bank_transaction with is_reconciled=true only works when there is no \
         feed line to match (conversion-style import)."
            .into(),
    );

    out.join("\n")
}

pub fn format_cash_validation(items: &[CashValidationItem]) -> String {
    if items.is_empty() {
        return "No cash validation data returned.".into();
    }

    let mut out = Vec::new();
    out.push(format!("Cash validation for {} account(s):", items.len()));

    for item in items {
        let id = item.account_id.as_deref().unwrap_or("—");
        let bal_date = item.statement_balance_date.as_deref().unwrap_or("—");
        let bal = item
            .statement_balance
            .as_ref()
            .map(|b| {
                format!(
                    "{} {}",
                    b.value
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "—".into()),
                    b.debit_or_credit.as_deref().unwrap_or("")
                )
            })
            .unwrap_or_else(|| "—".into());

        out.push(String::new());
        out.push(format!(
            "Account {id} | statement_balance_date:{bal_date} | bal:{bal}"
        ));

        if let Some(ref cur) = item
            .bank_statement
            .as_ref()
            .and_then(|s| s.current_statement.as_ref())
        {
            out.push(format!(
                "  current statement: {} → {} | start:{} end:{} | source:{}",
                cur.start_date.as_deref().unwrap_or("?"),
                cur.end_date.as_deref().unwrap_or("?"),
                cur.start_balance
                    .map(|b| format!("{b:.2}"))
                    .unwrap_or_else(|| "—".into()),
                cur.end_balance
                    .map(|b| format!("{b:.2}"))
                    .unwrap_or_else(|| "—".into()),
                cur.import_source_type.as_deref().unwrap_or("—"),
            ));
        }

        if let Some(ref lines) = item
            .bank_statement
            .as_ref()
            .and_then(|s| s.statement_lines.as_ref())
        {
            out.push(format!(
                "  unreconciled: {} lines | +{} / {} | earliest:{} latest:{}",
                lines
                    .unreconciled_lines
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "—".into()),
                lines
                    .unreconciled_amount_pos
                    .map(|a| format!("{a:.2}"))
                    .unwrap_or_else(|| "—".into()),
                lines
                    .unreconciled_amount_neg
                    .map(|a| format!("{a:.2}"))
                    .unwrap_or_else(|| "—".into()),
                lines
                    .earliest_unreconciled_transaction
                    .as_deref()
                    .unwrap_or("—"),
                lines
                    .latest_unreconciled_transaction
                    .as_deref()
                    .unwrap_or("—"),
            ));
            out.push(format!(
                "  reconciled: {} lines | +{} / {}",
                lines
                    .reconciled_lines
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "—".into()),
                lines
                    .reconciled_amount_pos
                    .map(|a| format!("{a:.2}"))
                    .unwrap_or_else(|| "—".into()),
                lines
                    .reconciled_amount_neg
                    .map(|a| format!("{a:.2}"))
                    .unwrap_or_else(|| "—".into()),
            ));
        }
    }

    out.push(String::new());
    out.push(
        "NOTE: These are statement-line metrics from the Finance API. Ticking off \
         unreconciled feed lines still requires the Xero UI — the public API cannot \
         match statement lines to accounting transactions."
            .into(),
    );

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank_transaction(json: &str) -> BankTransaction {
        serde_json::from_str(json).expect("BankTransaction should deserialize")
    }

    /// Verbatim from the Xero BankTransactions *list* endpoint. The list response
    /// carries LineItems, so account code and tax type need no extra fetch.
    #[test]
    fn renders_account_code_and_tax_type_from_list_response() {
        let bt = bank_transaction(
            r#"{
                "BankTransactionID": "0e0b7b1e-0000-0000-0000-000000000001",
                "Type": "SPEND",
                "Date": "/Date(1775001600000+0000)/",
                "Status": "AUTHORISED",
                "Contact": { "Name": "Anthropic" },
                "SubTotal": 89.43,
                "TotalTax": 13.42,
                "Total": 102.85,
                "LineItems": [{
                    "Description": "",
                    "UnitAmount": 102.85,
                    "TaxType": "INPUT3",
                    "TaxAmount": 13.42,
                    "LineAmount": 102.85,
                    "AccountCode": "485",
                    "Quantity": 1.0
                }]
            }"#,
        );

        assert_eq!(
            format_bank_transaction(&bt),
            "/Date(1775001600000+0000)/ | SPEND | 102.85 | Anthropic | AUTHORISED \
             | recon:? | acct:485 | tax:INPUT3 | vat:13.42 | id:0e0b7b1e-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn split_transaction_lists_each_distinct_account_and_tax_type() {
        let bt = bank_transaction(
            r#"{
                "Type": "SPEND",
                "Date": "/Date(1775001600000+0000)/",
                "Status": "AUTHORISED",
                "Contact": { "Name": "Nedbank" },
                "TotalTax": 1.50,
                "Total": 111.50,
                "LineItems": [
                    { "TaxType": "INPUT3", "AccountCode": "404", "LineAmount": 11.50 },
                    { "TaxType": "NONE",   "AccountCode": "485", "LineAmount": 100.00 }
                ]
            }"#,
        );

        let out = format_bank_transaction(&bt);
        assert!(out.contains("acct:404,485"), "got: {out}");
        assert!(out.contains("tax:INPUT3,NONE"), "got: {out}");
    }

    #[test]
    fn repeated_values_across_lines_are_not_duplicated() {
        let bt = bank_transaction(
            r#"{
                "Type": "SPEND",
                "Contact": { "Name": "xAI" },
                "LineItems": [
                    { "TaxType": "INPUT3", "AccountCode": "485", "LineAmount": 10.00 },
                    { "TaxType": "INPUT3", "AccountCode": "485", "LineAmount": 20.00 }
                ]
            }"#,
        );

        let out = format_bank_transaction(&bt);
        assert!(out.contains("acct:485 "), "got: {out}");
        assert!(out.contains("tax:INPUT3 "), "got: {out}");
    }

    #[test]
    fn missing_line_items_and_tax_degrade_to_placeholder() {
        let bt = bank_transaction(
            r#"{ "Type": "SPEND-TRANSFER", "Total": 3000.00, "Status": "AUTHORISED" }"#,
        );

        assert_eq!(
            format_bank_transaction(&bt),
            "— | SPEND-TRANSFER | 3000.00 | — | AUTHORISED | recon:? | acct:— | tax:— | vat:— | id:—"
        );
    }

    #[test]
    fn statement_line_formatter_marks_unreconciled() {
        let line: StatementLine = serde_json::from_str(
            r#"{
                "statementLineId": "aaa-111",
                "transactionDate": "2026-06-01",
                "payee": "xAI LLC",
                "amount": -1696.09,
                "type": "DEBIT",
                "isReconciled": false,
                "reference": "API usage"
            }"#,
        )
        .expect("statement line");

        let out = format_statement_line(&line);
        assert!(out.contains("recon:N"), "got: {out}");
        assert!(out.contains("xAI LLC"), "got: {out}");
        assert!(out.contains("-1696.09"), "got: {out}");
        assert!(out.contains("line:aaa-111"), "got: {out}");
    }

    #[test]
    fn bank_statements_plus_can_filter_to_unreconciled() {
        let resp: BankStatementAccountingResponse = serde_json::from_str(
            r#"{
                "bankAccountId": "bank-1",
                "bankAccountName": "Business",
                "bankAccountCurrencyCode": "ZAR",
                "statements": [{
                    "statementId": "stmt-1",
                    "startDate": "2026-06-01",
                    "endDate": "2026-06-30",
                    "importSource": "STMTIMPORTSRC/CSV",
                    "endBalance": 1000.0,
                    "statementLines": [
                        {
                            "statementLineId": "open-1",
                            "transactionDate": "2026-06-02",
                            "payee": "Open",
                            "amount": -50.0,
                            "isReconciled": false
                        },
                        {
                            "statementLineId": "done-1",
                            "transactionDate": "2026-06-03",
                            "payee": "Done",
                            "amount": -10.0,
                            "isReconciled": true
                        }
                    ]
                }]
            }"#,
        )
        .expect("response");

        let filtered = format_bank_statements_plus(&resp, true);
        assert!(filtered.contains("Open"), "got: {filtered}");
        assert!(!filtered.contains("Done"), "got: {filtered}");
        assert!(filtered.contains("1 unreconciled"), "got: {filtered}");

        let all = format_bank_statements_plus(&resp, false);
        assert!(all.contains("Done"), "got: {all}");
    }

    // -----------------------------------------------------------------------
    // build_tax_type_payload
    //
    // Shaped after the live xAI SPEND of 1,696.09 on 2026-06-29: one line, coded
    // 485 Subscriptions at INPUT3, tax-inclusive, VAT 221.23.
    // -----------------------------------------------------------------------

    fn xai_txn() -> Value {
        serde_json::json!({
            "BankTransactionID": "9c2f1e40-1b3a-4d5e-8f60-2a1b3c4d5e6f",
            "Type": "SPEND",
            "Status": "AUTHORISED",
            "LineAmountTypes": "Inclusive",
            "IsReconciled": true,
            "Date": "/Date(1782691200000+0000)/",
            "Contact": { "ContactID": "aaaa1111-2222-3333-4444-555566667777", "Name": "xAI" },
            "LineItems": [{
                "LineItemID": "11112222-3333-4444-5555-666677778888",
                "Description": "xAI API",
                "Quantity": 1.0,
                "UnitAmount": 1696.09,
                "AccountCode": "485",
                "TaxType": "INPUT3",
                "TaxAmount": 221.23,
                "LineAmount": 1696.09
            }],
            "SubTotal": 1474.86,
            "TotalTax": 221.23,
            "Total": 1696.09
        })
    }

    #[test]
    fn sets_tax_type_and_drops_stale_tax_amount() {
        let out = build_tax_type_payload(&xai_txn(), "NONE").expect("should build");
        let line = &out["LineItems"][0];

        assert_eq!(line["TaxType"], "NONE");
        // Xero rejects a TaxAmount that doesn't match its own calculation, so the
        // pre-change 221.23 must not be sent.
        assert!(
            line.get("TaxAmount").is_none(),
            "stale TaxAmount must be dropped, got: {line}"
        );
        // The amount that left the bank is not ours to touch.
        assert_eq!(line["LineAmount"], 1696.09);
        assert_eq!(line["UnitAmount"], 1696.09);
        assert_eq!(out["LineAmountTypes"], "Inclusive");
        assert_eq!(
            out["BankTransactionID"],
            "9c2f1e40-1b3a-4d5e-8f60-2a1b3c4d5e6f"
        );
    }

    #[test]
    fn carries_line_item_id_so_xero_amends_rather_than_recreates() {
        let out = build_tax_type_payload(&xai_txn(), "NONE").unwrap();
        assert_eq!(
            out["LineItems"][0]["LineItemID"],
            "11112222-3333-4444-5555-666677778888"
        );
    }

    /// The failure that would cost real data: Xero deletes any existing line not sent
    /// back with its LineItemID. Refuse rather than issue such a request.
    #[test]
    fn refuses_when_a_line_has_no_line_item_id() {
        let mut txn = xai_txn();
        txn["LineItems"][0]
            .as_object_mut()
            .unwrap()
            .remove("LineItemID");

        let e = build_tax_type_payload(&txn, "NONE").expect_err("must refuse");
        assert!(matches!(e, ApiError::Invalid(_)), "got: {e:?}");
        assert!(e.to_string().contains("LineItemID"), "got: {e}");
    }

    /// Fields this crate doesn't model must survive. Round-tripping through LineItem
    /// would silently drop Tracking and recreate the line without it.
    #[test]
    fn preserves_fields_the_typed_struct_does_not_model() {
        let mut txn = xai_txn();
        txn["LineItems"][0]["Tracking"] = serde_json::json!([{
            "TrackingCategoryID": "dddd1111-2222-3333-4444-555566667777",
            "Name": "Region",
            "Option": "Cape Town"
        }]);

        let out = build_tax_type_payload(&txn, "NONE").unwrap();
        assert_eq!(out["LineItems"][0]["Tracking"][0]["Option"], "Cape Town");
        assert_eq!(out["LineItems"][0]["Description"], "xAI API");
    }

    #[test]
    fn every_line_of_a_split_transaction_is_retagged() {
        let mut txn = xai_txn();
        txn["LineItems"] = serde_json::json!([
            { "LineItemID": "aaa", "AccountCode": "485", "TaxType": "INPUT3",
              "LineAmount": 1000.00, "TaxAmount": 130.43 },
            { "LineItemID": "bbb", "AccountCode": "489", "TaxType": "INPUT",
              "LineAmount": 696.09, "TaxAmount": 85.47 },
        ]);

        let out = build_tax_type_payload(&txn, "NONE").unwrap();
        let lines = out["LineItems"].as_array().unwrap();

        assert_eq!(lines.len(), 2, "no line may be dropped");
        for line in lines {
            assert_eq!(line["TaxType"], "NONE");
            assert!(line.get("TaxAmount").is_none());
        }
        // Account coding is not this tool's business.
        assert_eq!(lines[0]["AccountCode"], "485");
        assert_eq!(lines[1]["AccountCode"], "489");
    }

    /// An Exclusive transaction must not be silently restated as Inclusive, which is
    /// what Xero assumes when LineAmountTypes is absent.
    #[test]
    fn pins_the_tax_basis_of_an_exclusive_transaction() {
        let mut txn = xai_txn();
        txn["LineAmountTypes"] = serde_json::json!("Exclusive");

        let out = build_tax_type_payload(&txn, "NONE").unwrap();
        assert_eq!(out["LineAmountTypes"], "Exclusive");
    }

    #[test]
    fn rejects_a_transaction_with_no_usable_lines() {
        let mut txn = xai_txn();
        txn["LineItems"] = serde_json::json!([]);
        assert!(build_tax_type_payload(&txn, "NONE").is_err());

        let mut txn = xai_txn();
        txn.as_object_mut().unwrap().remove("LineItems");
        assert!(build_tax_type_payload(&txn, "NONE").is_err());

        let mut txn = xai_txn();
        txn.as_object_mut().unwrap().remove("BankTransactionID");
        assert!(build_tax_type_payload(&txn, "NONE").is_err());
    }

    #[test]
    fn update_report_flags_a_total_that_moved() {
        let before = TxnSummary::read(&xai_txn());

        let mut wrong = xai_txn();
        wrong["Total"] = serde_json::json!(1474.86);
        wrong["TotalTax"] = serde_json::json!(0.0);
        wrong["LineItems"][0]["TaxType"] = serde_json::json!("NONE");

        let out = format_tax_type_update(
            &TaxTypeUpdate {
                before,
                after: Some(TxnSummary::read(&wrong)),
                payload: Value::Null,
            },
            false,
        );
        assert!(out.contains("WARNING"), "got: {out}");
        assert!(out.contains("1696.09"), "got: {out}");
    }

    #[test]
    fn update_report_is_quiet_when_only_the_tax_split_moved() {
        let before = TxnSummary::read(&xai_txn());

        let mut after = xai_txn();
        after["LineItems"][0]["TaxType"] = serde_json::json!("NONE");
        after["SubTotal"] = serde_json::json!(1696.09);
        after["TotalTax"] = serde_json::json!(0.0);
        // Total unchanged — this is the whole point.

        let out = format_tax_type_update(
            &TaxTypeUpdate {
                before,
                after: Some(TxnSummary::read(&after)),
                payload: Value::Null,
            },
            false,
        );
        assert!(!out.contains("WARNING"), "got: {out}");
        assert!(out.contains("tax:INPUT3"), "got: {out}");
        assert!(out.contains("tax:NONE"), "got: {out}");
    }
}
