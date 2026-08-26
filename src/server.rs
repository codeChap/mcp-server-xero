use reqwest::Method;
use rmcp::{
    handler::server::tool::ToolRouter, handler::server::wrapper::Parameters, model::*, tool,
    tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;

use crate::api::*;
use crate::params::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok(text: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn err(e: ApiError) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(e.to_string())]))
}

fn build_query(
    base: &str,
    page: Option<u32>,
    where_filter: Option<&str>,
    order: Option<&str>,
    extra: &[(&str, &str)],
) -> String {
    let mut parts = Vec::new();
    if let Some(p) = page {
        parts.push(format!("page={p}"));
    }
    if let Some(w) = where_filter {
        parts.push(format!("where={}", urlencoding::encode(w)));
    }
    if let Some(o) = order {
        parts.push(format!("order={}", urlencoding::encode(o)));
    }
    for (k, v) in extra {
        parts.push(format!("{k}={}", urlencoding::encode(v)));
    }
    if parts.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", parts.join("&"))
    }
}

fn report_to_text(val: &Value) -> String {
    // Xero reports have a nested structure: Reports[0].Rows[].Cells[].Value
    let mut lines = Vec::new();

    if let Some(reports) = val.get("Reports").and_then(|v| v.as_array()) {
        for report in reports {
            if let Some(title) = report.get("ReportTitles").and_then(|v| v.as_array()) {
                for t in title {
                    if let Some(s) = t.as_str() {
                        lines.push(s.to_string());
                    }
                }
                lines.push(String::new());
            }
            if let Some(rows) = report.get("Rows").and_then(|v| v.as_array()) {
                for row in rows {
                    let row_type = row.get("RowType").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(cells) = row.get("Cells").and_then(|v| v.as_array()) {
                        let values: Vec<&str> = cells
                            .iter()
                            .filter_map(|c| c.get("Value").and_then(|v| v.as_str()))
                            .collect();
                        if !values.is_empty() {
                            let prefix = if row_type == "Header" { "## " } else { "  " };
                            lines.push(format!("{prefix}{}", values.join(" | ")));
                        }
                    }
                    // Handle nested section rows.
                    if let Some(sub_rows) = row.get("Rows").and_then(|v| v.as_array()) {
                        if let Some(title) = row.get("Title").and_then(|v| v.as_str()) {
                            lines.push(format!("\n### {title}"));
                        }
                        for sub in sub_rows {
                            if let Some(cells) = sub.get("Cells").and_then(|v| v.as_array()) {
                                let values: Vec<&str> = cells
                                    .iter()
                                    .filter_map(|c| c.get("Value").and_then(|v| v.as_str()))
                                    .collect();
                                if !values.is_empty() {
                                    let sub_type =
                                        sub.get("RowType").and_then(|v| v.as_str()).unwrap_or("");
                                    let prefix = if sub_type == "SummaryRow" { "**" } else { "  " };
                                    let suffix = if sub_type == "SummaryRow" { "**" } else { "" };
                                    lines.push(format!("{prefix}{}{suffix}", values.join(" | ")));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if lines.is_empty() {
        serde_json::to_string_pretty(val).unwrap_or_default()
    } else {
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct XeroServer {
    client: Arc<XeroClient>,
    tool_router: ToolRouter<Self>,
}

impl XeroServer {
    async fn fetch_report(
        &self,
        endpoint: &str,
        p: &ReportParams,
    ) -> Result<CallToolResult, McpError> {
        let mut extra = Vec::new();
        let date_str;
        let periods_str;
        let timeframe_str;

        if let Some(ref d) = p.date {
            date_str = d.clone();
            extra.push(("date", date_str.as_str()));
        }
        if let Some(periods) = p.periods {
            periods_str = periods.to_string();
            extra.push(("periods", periods_str.as_str()));
        }
        if let Some(ref tf) = p.timeframe {
            timeframe_str = tf.clone();
            extra.push(("timeframe", timeframe_str.as_str()));
        }

        let path = build_query(endpoint, None, None, None, &extra);

        match self.client.get_json(&path).await {
            Ok(val) => ok(report_to_text(&val)),
            Err(e) => err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router]
impl XeroServer {
    pub fn new(client: XeroClient) -> Self {
        Self {
            client: Arc::new(client),
            tool_router: Self::tool_router(),
        }
    }

    // -----------------------------------------------------------------------
    // Invoices
    // -----------------------------------------------------------------------

    #[tool(
        description = "List invoices from Xero. Supports filtering by status, contact, search term, \
                           and custom OData where clauses. Returns paginated results (100 per page)."
    )]
    async fn list_invoices(
        &self,
        Parameters(p): Parameters<ListInvoicesParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_invoices");

        let mut extra = Vec::new();
        let mut where_parts = Vec::new();

        if let Some(ref cid) = p.contact_id {
            where_parts.push(format!("Contact.ContactID=Guid(\"{cid}\")"));
        }
        if let Some(ref statuses) = p.statuses {
            let status_filter: Vec<String> = statuses
                .split(',')
                .map(|s| format!("Status==\"{}\"", s.trim()))
                .collect();
            where_parts.push(format!("({})", status_filter.join("||")));
        }
        if let Some(ref w) = p.where_filter {
            where_parts.push(w.clone());
        }

        let combined_where = if where_parts.is_empty() {
            None
        } else {
            Some(where_parts.join("&&"))
        };

        if let Some(ref search) = p.search {
            extra.push(("searchTerm", search.as_str()));
        }

        let path = build_query(
            "/Invoices",
            p.page,
            combined_where.as_deref(),
            p.order.as_deref(),
            &extra,
        );

        match self
            .client
            .request::<(), InvoicesResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.invoices.is_empty() {
                    return ok("No invoices found.".into());
                }
                let lines: Vec<String> = resp.invoices.iter().map(format_invoice).collect();
                ok(format!(
                    "Found {} invoices:\n{}",
                    resp.invoices.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Get a single invoice by ID or invoice number, including full line item details."
    )]
    async fn get_invoice(
        &self,
        Parameters(p): Parameters<GetInvoiceParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(id = %p.invoice_id, "get_invoice");

        let path = format!("/Invoices/{}", p.invoice_id);
        match self
            .client
            .request::<(), InvoicesResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.invoices)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(text)
            }
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Create a new invoice in Xero (created as DRAFT by default). \
                           Requires invoice type (ACCREC for sales, ACCPAY for bills), \
                           a contact ID, and line items."
    )]
    async fn create_invoice(
        &self,
        Parameters(p): Parameters<CreateInvoiceParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(invoice_type = %p.invoice_type, "create_invoice");

        if p.invoice_type != "ACCREC" && p.invoice_type != "ACCPAY" {
            return Err(McpError::invalid_params(
                "invoice_type must be ACCREC (sales invoice) or ACCPAY (bill)",
                None,
            ));
        }

        let line_items: Vec<Value> = serde_json::from_str(&p.line_items)
            .map_err(|e| McpError::invalid_params(format!("Invalid line_items JSON: {e}"), None))?;

        let mut invoice = json!({
            "Type": p.invoice_type,
            "Contact": { "ContactID": p.contact_id },
            "LineItems": line_items,
        });

        if let Some(ref date) = p.date {
            invoice["Date"] = json!(date);
        }
        if let Some(ref due) = p.due_date {
            invoice["DueDate"] = json!(due);
        }
        if let Some(ref reference) = p.reference {
            invoice["Reference"] = json!(reference);
        }
        if let Some(ref status) = p.status {
            invoice["Status"] = json!(status);
        }
        if let Some(ref currency) = p.currency_code {
            invoice["CurrencyCode"] = json!(currency);
        }

        match self
            .client
            .request::<_, InvoicesResponse>(Method::POST, "/Invoices", Some(&invoice))
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.invoices)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(format!("Invoice created:\n{text}"))
            }
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Update an existing invoice. Can change status, due date, reference, or replace line items."
    )]
    async fn update_invoice(
        &self,
        Parameters(p): Parameters<UpdateInvoiceParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(id = %p.invoice_id, "update_invoice");

        let mut invoice = json!({ "InvoiceID": p.invoice_id });

        if let Some(ref status) = p.status {
            invoice["Status"] = json!(status);
        }
        if let Some(ref due) = p.due_date {
            invoice["DueDate"] = json!(due);
        }
        if let Some(ref reference) = p.reference {
            invoice["Reference"] = json!(reference);
        }
        if let Some(ref items) = p.line_items {
            let parsed: Vec<Value> = serde_json::from_str(items).map_err(|e| {
                McpError::invalid_params(format!("Invalid line_items JSON: {e}"), None)
            })?;
            invoice["LineItems"] = json!(parsed);
        }

        let path = format!("/Invoices/{}", p.invoice_id);
        match self
            .client
            .request::<_, InvoicesResponse>(Method::POST, &path, Some(&invoice))
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.invoices)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(format!("Invoice updated:\n{text}"))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Contacts
    // -----------------------------------------------------------------------

    #[tool(
        description = "List contacts from Xero. Supports search and custom filters. Returns paginated results (100 per page)."
    )]
    async fn list_contacts(
        &self,
        Parameters(p): Parameters<ListContactsParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_contacts");

        let mut extra = Vec::new();
        if let Some(ref search) = p.search {
            extra.push(("searchTerm", search.as_str()));
        }

        let path = build_query(
            "/Contacts",
            p.page,
            p.where_filter.as_deref(),
            p.order.as_deref(),
            &extra,
        );

        match self
            .client
            .request::<(), ContactsResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.contacts.is_empty() {
                    return ok("No contacts found.".into());
                }
                let lines: Vec<String> = resp.contacts.iter().map(format_contact).collect();
                ok(format!(
                    "Found {} contacts:\n{}",
                    resp.contacts.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    #[tool(description = "Get a single contact by ID, including full address and phone details.")]
    async fn get_contact(
        &self,
        Parameters(p): Parameters<GetContactParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(id = %p.contact_id, "get_contact");

        let path = format!("/Contacts/{}", p.contact_id);
        match self
            .client
            .request::<(), ContactsResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.contacts)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(text)
            }
            Err(e) => err(e),
        }
    }

    #[tool(description = "Create a new contact in Xero.")]
    async fn create_contact(
        &self,
        Parameters(p): Parameters<CreateContactParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(name = %p.name, "create_contact");

        let mut contact = json!({ "Name": p.name });

        if let Some(ref first) = p.first_name {
            contact["FirstName"] = json!(first);
        }
        if let Some(ref last) = p.last_name {
            contact["LastName"] = json!(last);
        }
        if let Some(ref email) = p.email {
            contact["EmailAddress"] = json!(email);
        }
        if let Some(ref phone) = p.phone {
            contact["Phones"] = json!([{ "PhoneType": "DEFAULT", "PhoneNumber": phone }]);
        }
        if let Some(ref tax) = p.tax_number {
            contact["TaxNumber"] = json!(tax);
        }
        if let Some(ref acct) = p.account_number {
            contact["AccountNumber"] = json!(acct);
        }
        if let Some(ref cn) = p.company_number {
            contact["CompanyNumber"] = json!(cn);
        }

        // Build street address if any address field is provided
        if p.address_line1.is_some()
            || p.address_line2.is_some()
            || p.city.is_some()
            || p.region.is_some()
            || p.postal_code.is_some()
            || p.country.is_some()
        {
            let mut addr = json!({ "AddressType": "STREET" });
            if let Some(ref v) = p.address_line1 {
                addr["AddressLine1"] = json!(v);
            }
            if let Some(ref v) = p.address_line2 {
                addr["AddressLine2"] = json!(v);
            }
            if let Some(ref v) = p.city {
                addr["City"] = json!(v);
            }
            if let Some(ref v) = p.region {
                addr["Region"] = json!(v);
            }
            if let Some(ref v) = p.postal_code {
                addr["PostalCode"] = json!(v);
            }
            if let Some(ref v) = p.country {
                addr["Country"] = json!(v);
            }
            contact["Addresses"] = json!([addr]);
        }

        match self
            .client
            .request::<_, ContactsResponse>(Method::POST, "/Contacts", Some(&contact))
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.contacts)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(format!("Contact created:\n{text}"))
            }
            Err(e) => err(e),
        }
    }

    #[tool(description = "Update an existing contact.")]
    async fn update_contact(
        &self,
        Parameters(p): Parameters<UpdateContactParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(id = %p.contact_id, "update_contact");

        let mut contact = json!({ "ContactID": p.contact_id });

        if let Some(ref name) = p.name {
            contact["Name"] = json!(name);
        }
        if let Some(ref email) = p.email {
            contact["EmailAddress"] = json!(email);
        }
        if let Some(ref phone) = p.phone {
            contact["Phones"] = json!([{ "PhoneType": "DEFAULT", "PhoneNumber": phone }]);
        }
        if let Some(ref first) = p.first_name {
            contact["FirstName"] = json!(first);
        }
        if let Some(ref last) = p.last_name {
            contact["LastName"] = json!(last);
        }
        if let Some(ref tax) = p.tax_number {
            contact["TaxNumber"] = json!(tax);
        }
        if let Some(ref acct) = p.account_number {
            contact["AccountNumber"] = json!(acct);
        }
        if let Some(ref cn) = p.company_number {
            contact["CompanyNumber"] = json!(cn);
        }

        // Build street address if any address field is provided
        if p.address_line1.is_some()
            || p.address_line2.is_some()
            || p.city.is_some()
            || p.region.is_some()
            || p.postal_code.is_some()
            || p.country.is_some()
        {
            let mut addr = json!({ "AddressType": "STREET" });
            if let Some(ref v) = p.address_line1 {
                addr["AddressLine1"] = json!(v);
            }
            if let Some(ref v) = p.address_line2 {
                addr["AddressLine2"] = json!(v);
            }
            if let Some(ref v) = p.city {
                addr["City"] = json!(v);
            }
            if let Some(ref v) = p.region {
                addr["Region"] = json!(v);
            }
            if let Some(ref v) = p.postal_code {
                addr["PostalCode"] = json!(v);
            }
            if let Some(ref v) = p.country {
                addr["Country"] = json!(v);
            }
            contact["Addresses"] = json!([addr]);
        }

        let path = format!("/Contacts/{}", p.contact_id);
        match self
            .client
            .request::<_, ContactsResponse>(Method::POST, &path, Some(&contact))
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.contacts)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(format!("Contact updated:\n{text}"))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Accounts (Chart of Accounts)
    // -----------------------------------------------------------------------

    #[tool(
        description = "List the chart of accounts. Filter by type (REVENUE, EXPENSE, ASSET, etc.) or status using where_filter."
    )]
    async fn list_accounts(
        &self,
        Parameters(p): Parameters<ListAccountsParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_accounts");

        let path = build_query("/Accounts", None, p.where_filter.as_deref(), None, &[]);

        match self
            .client
            .request::<(), AccountsResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.accounts.is_empty() {
                    return ok("No accounts found.".into());
                }
                let lines: Vec<String> = resp.accounts.iter().map(format_account).collect();
                ok(format!(
                    "Found {} accounts:\n{}",
                    resp.accounts.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Items
    // -----------------------------------------------------------------------

    #[tool(description = "List inventory/service items.")]
    async fn list_items(
        &self,
        Parameters(p): Parameters<ListItemsParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_items");

        let path = build_query("/Items", None, p.where_filter.as_deref(), None, &[]);

        match self
            .client
            .request::<(), ItemsResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.items.is_empty() {
                    return ok("No items found.".into());
                }
                let lines: Vec<String> = resp
                    .items
                    .iter()
                    .map(|i| {
                        let code = i.code.as_deref().unwrap_or("—");
                        let name = i.name.as_deref().unwrap_or("—");
                        let desc = i.description.as_deref().unwrap_or("");
                        format!("{code} | {name} | {desc}")
                    })
                    .collect();
                ok(format!(
                    "Found {} items:\n{}",
                    resp.items.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    #[tool(description = "Create a new inventory/service item.")]
    async fn create_item(
        &self,
        Parameters(p): Parameters<CreateItemParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(code = %p.code, "create_item");

        let mut item = json!({ "Code": p.code });

        if let Some(ref name) = p.name {
            item["Name"] = json!(name);
        }
        if let Some(ref desc) = p.description {
            item["Description"] = json!(desc);
        }
        if let Some(ref pdesc) = p.purchase_description {
            item["PurchaseDescription"] = json!(pdesc);
        }

        match self
            .client
            .request::<_, ItemsResponse>(Method::POST, "/Items", Some(&item))
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.items)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(format!("Item created:\n{text}"))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Payments
    // -----------------------------------------------------------------------

    #[tool(description = "List payments. Supports filtering and ordering.")]
    async fn list_payments(
        &self,
        Parameters(p): Parameters<ListPaymentsParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_payments");

        let path = build_query(
            "/Payments",
            p.page,
            p.where_filter.as_deref(),
            p.order.as_deref(),
            &[],
        );

        match self
            .client
            .request::<(), PaymentsResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.payments.is_empty() {
                    return ok("No payments found.".into());
                }
                let lines: Vec<String> = resp.payments.iter().map(format_payment).collect();
                ok(format!(
                    "Found {} payments:\n{}",
                    resp.payments.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    #[tool(description = "Create a payment against an invoice.")]
    async fn create_payment(
        &self,
        Parameters(p): Parameters<CreatePaymentParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(invoice = %p.invoice_id, amount = %p.amount, "create_payment");

        let mut payment = json!({
            "Invoice": { "InvoiceID": p.invoice_id },
            "Account": { "Code": p.account_code },
            "Amount": p.amount,
        });

        if let Some(ref date) = p.date {
            payment["Date"] = json!(date);
        }
        if let Some(ref reference) = p.reference {
            payment["Reference"] = json!(reference);
        }

        match self
            .client
            .request::<_, PaymentsResponse>(Method::PUT, "/Payments", Some(&payment))
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.payments)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(format!("Payment created:\n{text}"))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Credit Notes
    // -----------------------------------------------------------------------

    #[tool(description = "List credit notes. Supports filtering and ordering.")]
    async fn list_credit_notes(
        &self,
        Parameters(p): Parameters<ListCreditNotesParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_credit_notes");

        let path = build_query(
            "/CreditNotes",
            p.page,
            p.where_filter.as_deref(),
            p.order.as_deref(),
            &[],
        );

        match self
            .client
            .request::<(), CreditNotesResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.credit_notes.is_empty() {
                    return ok("No credit notes found.".into());
                }
                let lines: Vec<String> = resp
                    .credit_notes
                    .iter()
                    .map(|cn| {
                        let num = cn.credit_note_number.as_deref().unwrap_or("—");
                        let status = cn.status.as_deref().unwrap_or("—");
                        let total = cn.total.map(|t| format!("{t:.2}")).unwrap_or("—".into());
                        let contact = cn
                            .contact
                            .as_ref()
                            .and_then(|c| c.name.as_deref())
                            .unwrap_or("—");
                        format!("{num} | {status} | {total} | {contact}")
                    })
                    .collect();
                ok(format!(
                    "Found {} credit notes:\n{}",
                    resp.credit_notes.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Quotes
    // -----------------------------------------------------------------------

    #[tool(description = "List quotes. Filter by contact or status.")]
    async fn list_quotes(
        &self,
        Parameters(p): Parameters<ListQuotesParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_quotes");

        let mut where_parts = Vec::new();
        if let Some(ref cid) = p.contact_id {
            where_parts.push(format!("Contact.ContactID=Guid(\"{cid}\")"));
        }
        if let Some(ref status) = p.status {
            where_parts.push(format!("Status==\"{status}\""));
        }

        let combined = if where_parts.is_empty() {
            None
        } else {
            Some(where_parts.join("&&"))
        };

        let path = build_query(
            "/Quotes",
            p.page,
            combined.as_deref(),
            p.order.as_deref(),
            &[],
        );

        match self
            .client
            .request::<(), QuotesResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.quotes.is_empty() {
                    return ok("No quotes found.".into());
                }
                let lines: Vec<String> = resp
                    .quotes
                    .iter()
                    .map(|q| {
                        let num = q.quote_number.as_deref().unwrap_or("—");
                        let status = q.status.as_deref().unwrap_or("—");
                        let total = q.total.map(|t| format!("{t:.2}")).unwrap_or("—".into());
                        let contact = q
                            .contact
                            .as_ref()
                            .and_then(|c| c.name.as_deref())
                            .unwrap_or("—");
                        format!("{num} | {status} | {total} | {contact}")
                    })
                    .collect();
                ok(format!(
                    "Found {} quotes:\n{}",
                    resp.quotes.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Tax Rates
    // -----------------------------------------------------------------------

    #[tool(description = "List all tax rates configured in this Xero organisation.")]
    async fn list_tax_rates(&self) -> Result<CallToolResult, McpError> {
        debug!("list_tax_rates");

        match self
            .client
            .request::<(), TaxRatesResponse>(Method::GET, "/TaxRates", None)
            .await
        {
            Ok(resp) => {
                if resp.tax_rates.is_empty() {
                    return ok("No tax rates found.".into());
                }
                let lines: Vec<String> = resp
                    .tax_rates
                    .iter()
                    .map(|tr| {
                        let name = tr.name.as_deref().unwrap_or("—");
                        let tax_type = tr.tax_type.as_deref().unwrap_or("—");
                        let rate = tr
                            .effective_rate
                            .map(|r| format!("{r:.2}%"))
                            .unwrap_or("—".into());
                        let status = tr.status.as_deref().unwrap_or("—");
                        format!("{name} | {tax_type} | {rate} | {status}")
                    })
                    .collect();
                ok(format!(
                    "Found {} tax rates:\n{}",
                    resp.tax_rates.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Organisation
    // -----------------------------------------------------------------------

    #[tool(
        description = "Get details about the connected Xero organisation (name, currency, country, etc.)."
    )]
    async fn get_organisation(&self) -> Result<CallToolResult, McpError> {
        debug!("get_organisation");

        match self
            .client
            .request::<(), OrganisationsResponse>(Method::GET, "/Organisation", None)
            .await
        {
            Ok(resp) => {
                let text = serde_json::to_string_pretty(&resp.organisations)
                    .unwrap_or_else(|_| format!("{resp:?}"));
                ok(text)
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Tracking Categories
    // -----------------------------------------------------------------------

    #[tool(description = "List tracking categories and their options.")]
    async fn list_tracking_categories(
        &self,
        Parameters(p): Parameters<ListTrackingCategoriesParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_tracking_categories");

        let path = build_query(
            "/TrackingCategories",
            None,
            p.where_filter.as_deref(),
            None,
            &[],
        );

        match self
            .client
            .request::<(), TrackingCategoriesResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.tracking_categories.is_empty() {
                    return ok("No tracking categories found.".into());
                }
                let lines: Vec<String> = resp
                    .tracking_categories
                    .iter()
                    .map(|tc| {
                        let name = tc.name.as_deref().unwrap_or("—");
                        let status = tc.status.as_deref().unwrap_or("—");
                        let opts: Vec<&str> = tc
                            .options
                            .as_deref()
                            .unwrap_or(&[])
                            .iter()
                            .filter_map(|o| o.name.as_deref())
                            .collect();
                        format!("{name} ({status}): [{}]", opts.join(", "))
                    })
                    .collect();
                ok(format!(
                    "Found {} tracking categories:\n{}",
                    resp.tracking_categories.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Bank Transactions
    // -----------------------------------------------------------------------

    #[tool(
        description = "List bank transactions. Supports filtering and ordering. \
                           Each line reports the account code(s) and tax type(s) it was coded to, \
                           the VAT amount, and the transaction id; split transactions list every \
                           distinct value. Filter on tax with a where_filter such as 'TotalTax>0'. \
                           The id is what set_bank_transaction_tax_type takes."
    )]
    async fn list_bank_transactions(
        &self,
        Parameters(p): Parameters<ListBankTransactionsParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("list_bank_transactions");

        // Always page. Xero omits LineItems from an unpaged collection response, which
        // would blank out the account code and tax type on every row.
        let path = build_query(
            "/BankTransactions",
            Some(p.page.unwrap_or(1)),
            p.where_filter.as_deref(),
            p.order.as_deref(),
            &[],
        );

        match self
            .client
            .request::<(), BankTransactionsResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => {
                if resp.bank_transactions.is_empty() {
                    return ok("No bank transactions found.".into());
                }
                let lines: Vec<String> = resp
                    .bank_transactions
                    .iter()
                    .map(format_bank_transaction)
                    .collect();
                ok(format!(
                    "Found {} bank transactions:\n{}",
                    resp.bank_transactions.len(),
                    lines.join("\n")
                ))
            }
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Set the tax type on every line of an existing bank transaction, \
                           e.g. tax_type='NONE' to remove VAT from an entry that should not \
                           carry it. Every other field is preserved. On a tax-inclusive \
                           transaction the total does not change — only the split between the \
                           expense account and the VAT account. Use dry_run=true to preview \
                           the payload; writing requires confirm='YES-WRITE'. \
                           NOTE: Xero rejects this with a ValidationException for any \
                           transaction already reconciled against a bank statement — the API \
                           grants no way around it (the docs do not mention this; it was found \
                           by trying). Recoding reconciled entries in bulk is only possible in \
                           the Xero UI via Accounting > Advanced > Find & Recode."
    )]
    async fn set_bank_transaction_tax_type(
        &self,
        Parameters(p): Parameters<SetBankTransactionTaxTypeParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(id = %p.bank_transaction_id, tax_type = %p.tax_type, "set_bank_transaction_tax_type");

        let dry_run = p.dry_run.unwrap_or(false);
        if !dry_run && p.confirm.as_deref() != Some("YES-WRITE") {
            return ok(
                "Refused: this writes to live accounting records. Pass confirm='YES-WRITE' \
                 to proceed, or dry_run=true to preview the change without sending it."
                    .into(),
            );
        }

        match self
            .client
            .set_bank_transaction_tax_type(&p.bank_transaction_id, &p.tax_type, dry_run)
            .await
        {
            Ok(u) => ok(format_tax_type_update(&u, dry_run)),
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Pull bank statement lines for a bank account over a date range \
                       (Finance API Bank Statements Plus). Shows payee, amount, date, \
                       isReconciled, and any already-matched bank transaction / payment \
                       IDs. Default unreconciled_only=true so you see what is still open. \
                       bank_account_id is the AccountID UUID (list_accounts where \
                       Type==\"BANK\"). Max range 12 months. \
                       PARTNER SCOPE: finance.bankstatementsplus.read is restricted to \
                       Xero Financial Services partners. A normal app gets 401 \
                       AuthorizationUnsuccessful (requesting the scope on authorize can \
                       500). Without partnership, use list_bank_transactions with \
                       where_filter IsReconciled==false for the books-side queue. \
                       HARD LIMIT: even with Finance access, Xero does not allow ticking \
                       off feed lines via API — only reading them."
    )]
    async fn list_bank_statement_lines(
        &self,
        Parameters(p): Parameters<ListBankStatementLinesParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(
            bank = %p.bank_account_id,
            from = %p.from_date,
            to = %p.to_date,
            "list_bank_statement_lines"
        );

        let unreconciled_only = p.unreconciled_only.unwrap_or(true);
        let summary_only = p.summary_only.unwrap_or(true);
        let summary_str = if summary_only { "true" } else { "false" };

        let path = format!(
            "/BankStatementsPlus/statements?BankAccountID={}&FromDate={}&ToDate={}&SummaryOnly={}",
            urlencoding::encode(&p.bank_account_id),
            urlencoding::encode(&p.from_date),
            urlencoding::encode(&p.to_date),
            summary_str,
        );

        match self
            .client
            .finance_request::<(), BankStatementAccountingResponse>(Method::GET, &path, None)
            .await
        {
            Ok(resp) => ok(format_bank_statements_plus(&resp, unreconciled_only)),
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Bank reconciliation summary per bank account (Finance API Cash \
                       Validation): unreconciled line counts/amounts, earliest/latest \
                       unreconciled dates, statement balance. PARTNER SCOPE: \
                       finance.cashvalidation.read needs Xero Financial Services \
                       partnership (normal apps get 401). Read-only; cannot tick off lines."
    )]
    async fn get_cash_validation(
        &self,
        Parameters(p): Parameters<GetCashValidationParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("get_cash_validation");

        let path = if let Some(ref d) = p.balance_date {
            format!("/CashValidation?balanceDate={}", urlencoding::encode(d))
        } else {
            "/CashValidation".to_string()
        };

        match self
            .client
            .finance_request::<(), Vec<CashValidationItem>>(Method::GET, &path, None)
            .await
        {
            Ok(items) => ok(format_cash_validation(&items)),
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Create a spend or receive money bank transaction (Accounting API). \
                       Use for coding money that left/entered the bank without an invoice, \
                       or when importing history. line_items is a JSON array. \
                       Writing requires confirm='YES-WRITE'; dry_run=true previews. \
                       is_reconciled=true posts already reconciled (Xero invents a \
                       statement line) — ONLY when there is no bank-feed line for this \
                       amount. Matching an existing feed line to 'tick it off' is not \
                       possible via API; leave is_reconciled false and match in the UI. \
                       For invoice payments use create_payment instead."
    )]
    async fn create_bank_transaction(
        &self,
        Parameters(p): Parameters<CreateBankTransactionParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(
            r#type = %p.transaction_type,
            bank = %p.bank_account_code,
            "create_bank_transaction"
        );

        let dry_run = p.dry_run.unwrap_or(false);
        if !dry_run && p.confirm.as_deref() != Some("YES-WRITE") {
            return ok(
                "Refused: this writes to live accounting records. Pass confirm='YES-WRITE' \
                 to proceed, or dry_run=true to preview the payload without sending it."
                    .into(),
            );
        }

        let line_items: Value = match serde_json::from_str(&p.line_items) {
            Ok(v) => v,
            Err(e) => {
                return ok(format!(
                    "Invalid line_items JSON: {e}. Expected array like \
                     [{{\"Description\":\"xAI\",\"Quantity\":1,\"UnitAmount\":100,\"AccountCode\":\"485\",\"TaxType\":\"NONE\"}}]"
                ));
            }
        };
        if !line_items.is_array() {
            return ok("line_items must be a JSON array of line objects.".into());
        }

        let txn_type = p.transaction_type.to_uppercase();
        let allowed = [
            "SPEND",
            "RECEIVE",
            "SPEND-PREPAYMENT",
            "RECEIVE-PREPAYMENT",
            "SPEND-OVERPAYMENT",
            "RECEIVE-OVERPAYMENT",
        ];
        if !allowed.iter().any(|t| *t == txn_type) {
            return ok(format!(
                "Invalid transaction_type '{txn_type}'. Allowed: {}",
                allowed.join(", ")
            ));
        }

        let mut txn = json!({
            "Type": txn_type,
            "Contact": { "ContactID": p.contact_id },
            "BankAccount": { "Code": p.bank_account_code },
            "LineItems": line_items,
            "LineAmountTypes": p.line_amount_types.as_deref().unwrap_or("Inclusive"),
        });
        if let Some(ref date) = p.date {
            txn["Date"] = json!(date);
        }
        if let Some(ref reference) = p.reference {
            txn["Reference"] = json!(reference);
        }
        if p.is_reconciled == Some(true) {
            txn["IsReconciled"] = json!(true);
        }

        // Xero expects an envelope: { "BankTransactions": [ ... ] }
        let payload = json!({ "BankTransactions": [txn] });

        if dry_run {
            return ok(format!(
                "DRY RUN — nothing was sent to Xero.\nPayload that would be PUT:\n{}",
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "—".into())
            ));
        }

        match self
            .client
            .request::<_, BankTransactionsResponse>(
                Method::PUT,
                "/BankTransactions",
                Some(&payload),
            )
            .await
        {
            Ok(resp) => {
                if resp.bank_transactions.is_empty() {
                    return ok("Created, but Xero returned no bank transactions.".into());
                }
                let lines: Vec<String> = resp
                    .bank_transactions
                    .iter()
                    .map(format_bank_transaction)
                    .collect();
                ok(format!("Bank transaction created:\n{}", lines.join("\n")))
            }
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Email
    // -----------------------------------------------------------------------

    #[tool(
        description = "Email an AUTHORISED invoice to the contact's email address. \
                           The invoice must be in AUTHORISED status before it can be emailed."
    )]
    async fn email_invoice(
        &self,
        Parameters(p): Parameters<EmailInvoiceParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(id = %p.invoice_id, "email_invoice");

        let path = format!("/Invoices/{}/Email", p.invoice_id);
        match self.client.post_empty(&path).await {
            Ok(()) => ok(format!("Invoice {} emailed successfully.", p.invoice_id)),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Send a statement of account email to a contact, \
                           summarising all their outstanding invoices.")]
    async fn send_statement(
        &self,
        Parameters(p): Parameters<SendStatementParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(id = %p.contact_id, "send_statement");

        let path = format!("/Contacts/{}/SendStatementEmail", p.contact_id);
        match self.client.post_empty(&path).await {
            Ok(()) => ok(format!("Statement emailed to contact {}.", p.contact_id)),
            Err(e) => err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Reports
    // -----------------------------------------------------------------------

    #[tool(description = "Get the Profit and Loss report.")]
    async fn report_profit_and_loss(
        &self,
        Parameters(p): Parameters<ReportParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("report_profit_and_loss");
        self.fetch_report("/Reports/ProfitAndLoss", &p).await
    }

    #[tool(description = "Get the Balance Sheet report.")]
    async fn report_balance_sheet(
        &self,
        Parameters(p): Parameters<ReportParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("report_balance_sheet");
        self.fetch_report("/Reports/BalanceSheet", &p).await
    }

    #[tool(description = "Get the Trial Balance report.")]
    async fn report_trial_balance(
        &self,
        Parameters(p): Parameters<ReportParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!("report_trial_balance");
        self.fetch_report("/Reports/TrialBalance", &p).await
    }

    #[tool(description = "Get Aged Receivables report for a specific contact.")]
    async fn report_aged_receivables(
        &self,
        Parameters(p): Parameters<AgedReportParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(contact = %p.contact_id, "report_aged_receivables");

        let mut extra = vec![("contactId", p.contact_id.as_str())];
        let date_str;

        if let Some(ref d) = p.date {
            date_str = d.clone();
            extra.push(("date", date_str.as_str()));
        }

        let path = build_query(
            "/Reports/AgedReceivablesByContact",
            None,
            None,
            None,
            &extra,
        );

        match self.client.get_json(&path).await {
            Ok(val) => ok(report_to_text(&val)),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Get Aged Payables report for a specific contact.")]
    async fn report_aged_payables(
        &self,
        Parameters(p): Parameters<AgedReportParams>,
    ) -> Result<CallToolResult, McpError> {
        debug!(contact = %p.contact_id, "report_aged_payables");

        let mut extra = vec![("contactId", p.contact_id.as_str())];
        let date_str;

        if let Some(ref d) = p.date {
            date_str = d.clone();
            extra.push(("date", date_str.as_str()));
        }

        let path = build_query("/Reports/AgedPayablesByContact", None, None, None, &extra);

        match self.client.get_json(&path).await {
            Ok(val) => ok(report_to_text(&val)),
            Err(e) => err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// MCP ServerHandler
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for XeroServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("xero", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Xero Accounting MCP server. Tools: list/get/create/update invoices, \
                 list/get/create/update contacts, list accounts, list/create items, \
                 list/create payments, list credit notes, list quotes, list tax rates, \
                 get organisation, list tracking categories, list bank transactions, \
                 set bank transaction tax type, create bank transaction (spend/receive), \
                 list bank statement lines (Finance API Bank Statements Plus), \
                 get cash validation (recon summary), and financial reports \
                 (P&L, balance sheet, trial balance, aged receivables, aged payables). \
                 IMPORTANT: Xero does not expose reconciling/ticking bank-feed statement \
                 lines via the public API. list_bank_statement_lines is read-only; \
                 create_bank_transaction/create_payment post books entries; final match \
                 of feed lines is done in the Xero Reconcile UI.",
            )
    }
}
