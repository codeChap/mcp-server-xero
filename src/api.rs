use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::auth::{self, StoredTokens};

const TOKEN_URL: &str = "https://identity.xero.com/connect/token";
const CONNECTIONS_URL: &str = "https://api.xero.com/connections";
const BASE_URL: &str = "https://api.xero.com/api.xro/2.0";

/// Token expires in 30 minutes; refresh with 60s buffer.
const TOKEN_EXPIRY_BUFFER_SECS: u64 = 60;

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
                self.auth_client_credentials(&mut guard, scopes.as_deref()).await
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
            return Err(ApiError::Auth(format!("Token request failed ({status}): {body}")));
        }

        let token_resp: ClientCredentialsResponse = resp.json().await?;
        let expires_at =
            Instant::now() + Duration::from_secs(token_resp.expires_in.saturating_sub(TOKEN_EXPIRY_BUFFER_SECS));

        // Try to discover tenant ID.
        let tenant_id = match self
            .http
            .get(CONNECTIONS_URL)
            .header("Authorization", format!("Bearer {}", token_resp.access_token))
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

        let updated = auth::refresh_tokens(
            &self.http,
            &self.client_id,
            &self.client_secret,
            stored,
        )
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
        *mode = AuthMode::AuthorizationCode { tokens: updated.clone() };

        Ok((updated.access_token, Some(updated.tenant_id)))
    }

    /// Make an authenticated request to the Xero Accounting API.
    pub async fn request<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Req>,
    ) -> Result<Resp, ApiError> {
        let (token, tenant_id) = self.ensure_auth().await?;
        let url = format!("{BASE_URL}{path}");

        let mut builder = self
            .http
            .request(method, &url)
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

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(status = %status, "Xero API request failed");
            return Err(ApiError::Api { status, body });
        }

        Ok(response.json::<Resp>().await?)
    }

    /// POST with empty body, returning only success/failure (for email endpoints).
    pub async fn post_empty(&self, path: &str) -> Result<(), ApiError> {
        let (token, tenant_id) = self.ensure_auth().await?;
        let url = format!("{BASE_URL}{path}");

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

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(status = %status, "Xero API request failed");
            return Err(ApiError::Api { status, body });
        }

        Ok(())
    }

    /// Make a GET request returning raw JSON (for reports and flexible responses).
    pub async fn get_json(&self, path: &str) -> Result<serde_json::Value, ApiError> {
        self.request::<(), serde_json::Value>(Method::GET, path, None)
            .await
    }
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
    format!("{code} | {name} | {acct_type} | {status}")
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
