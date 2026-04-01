use schemars::JsonSchema;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Invoices
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListInvoicesParams {
    #[schemars(description = "Page number (starts at 1). Each page returns up to 100 invoices.")]
    pub page: Option<u32>,

    #[schemars(description = "Filter by contact ID")]
    pub contact_id: Option<String>,

    #[schemars(
        description = "Comma-separated statuses to filter: DRAFT, SUBMITTED, AUTHORISED, PAID, VOIDED, DELETED"
    )]
    pub statuses: Option<String>,

    #[schemars(description = "Search term (searches invoice number, reference, contact name)")]
    pub search: Option<String>,

    #[schemars(description = "Custom where filter (Xero OData syntax, e.g. 'DueDate>=DateTime(2024,01,01)')")]
    pub where_filter: Option<String>,

    #[schemars(description = "Order results, e.g. 'Date DESC' or 'DueDate ASC'")]
    pub order: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetInvoiceParams {
    #[schemars(description = "Invoice ID (UUID) or invoice number")]
    pub invoice_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateInvoiceParams {
    #[schemars(description = "Invoice type: ACCREC (sales) or ACCPAY (bills)")]
    pub invoice_type: String,

    #[schemars(description = "Contact ID (UUID) for the invoice recipient")]
    pub contact_id: String,

    #[schemars(
        description = "Line items as JSON array. Each item: {\"Description\": \"...\", \"Quantity\": 1, \"UnitAmount\": 100.00, \"AccountCode\": \"200\"}"
    )]
    pub line_items: String,

    #[schemars(description = "Invoice date in YYYY-MM-DD format (defaults to today)")]
    pub date: Option<String>,

    #[schemars(description = "Due date in YYYY-MM-DD format")]
    pub due_date: Option<String>,

    #[schemars(description = "Reference string")]
    pub reference: Option<String>,

    #[schemars(description = "Status: DRAFT (default), SUBMITTED, or AUTHORISED")]
    pub status: Option<String>,

    #[schemars(description = "Currency code (e.g. NZD, USD, GBP). Uses org default if omitted.")]
    pub currency_code: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateInvoiceParams {
    #[schemars(description = "Invoice ID (UUID) to update")]
    pub invoice_id: String,

    #[schemars(description = "New status (e.g. SUBMITTED, AUTHORISED, VOIDED)")]
    pub status: Option<String>,

    #[schemars(description = "New due date in YYYY-MM-DD format")]
    pub due_date: Option<String>,

    #[schemars(description = "New reference string")]
    pub reference: Option<String>,

    #[schemars(description = "Updated line items as JSON array (replaces all existing line items)")]
    pub line_items: Option<String>,
}

// ---------------------------------------------------------------------------
// Contacts
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListContactsParams {
    #[schemars(description = "Page number (starts at 1). Each page returns up to 100 contacts.")]
    pub page: Option<u32>,

    #[schemars(description = "Search term (searches name, email, account number)")]
    pub search: Option<String>,

    #[schemars(description = "Custom where filter (Xero OData syntax)")]
    pub where_filter: Option<String>,

    #[schemars(description = "Order results, e.g. 'Name ASC'")]
    pub order: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetContactParams {
    #[schemars(description = "Contact ID (UUID)")]
    pub contact_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateContactParams {
    #[schemars(description = "Contact name (required)")]
    pub name: String,

    #[schemars(description = "First name")]
    pub first_name: Option<String>,

    #[schemars(description = "Last name")]
    pub last_name: Option<String>,

    #[schemars(description = "Email address")]
    pub email: Option<String>,

    #[schemars(description = "Phone number")]
    pub phone: Option<String>,

    #[schemars(description = "Tax number / VAT number")]
    pub tax_number: Option<String>,

    #[schemars(description = "Account number")]
    pub account_number: Option<String>,

    #[schemars(description = "Company registration number")]
    pub company_number: Option<String>,

    #[schemars(description = "Street address line 1")]
    pub address_line1: Option<String>,

    #[schemars(description = "Street address line 2")]
    pub address_line2: Option<String>,

    #[schemars(description = "City / town")]
    pub city: Option<String>,

    #[schemars(description = "Region / province / state")]
    pub region: Option<String>,

    #[schemars(description = "Postal / ZIP code")]
    pub postal_code: Option<String>,

    #[schemars(description = "Country")]
    pub country: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateContactParams {
    #[schemars(description = "Contact ID (UUID) to update")]
    pub contact_id: String,

    #[schemars(description = "Updated name")]
    pub name: Option<String>,

    #[schemars(description = "Updated email")]
    pub email: Option<String>,

    #[schemars(description = "Updated phone number")]
    pub phone: Option<String>,

    #[schemars(description = "Updated first name")]
    pub first_name: Option<String>,

    #[schemars(description = "Updated last name")]
    pub last_name: Option<String>,

    #[schemars(description = "Updated tax number / VAT number")]
    pub tax_number: Option<String>,

    #[schemars(description = "Updated account number")]
    pub account_number: Option<String>,

    #[schemars(description = "Updated company registration number")]
    pub company_number: Option<String>,

    #[schemars(description = "Updated street address line 1")]
    pub address_line1: Option<String>,

    #[schemars(description = "Updated street address line 2")]
    pub address_line2: Option<String>,

    #[schemars(description = "Updated city / town")]
    pub city: Option<String>,

    #[schemars(description = "Updated region / province / state")]
    pub region: Option<String>,

    #[schemars(description = "Updated postal / ZIP code")]
    pub postal_code: Option<String>,

    #[schemars(description = "Updated country")]
    pub country: Option<String>,
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAccountsParams {
    #[schemars(description = "Custom where filter (e.g. 'Type==\"REVENUE\"' or 'Status==\"ACTIVE\"')")]
    pub where_filter: Option<String>,
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListItemsParams {
    #[schemars(description = "Custom where filter")]
    pub where_filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateItemParams {
    #[schemars(description = "Item code (required, unique identifier)")]
    pub code: String,

    #[schemars(description = "Item name")]
    pub name: Option<String>,

    #[schemars(description = "Sales description")]
    pub description: Option<String>,

    #[schemars(description = "Purchase description")]
    pub purchase_description: Option<String>,
}

// ---------------------------------------------------------------------------
// Payments
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListPaymentsParams {
    #[schemars(description = "Page number (starts at 1)")]
    pub page: Option<u32>,

    #[schemars(description = "Custom where filter")]
    pub where_filter: Option<String>,

    #[schemars(description = "Order results, e.g. 'Date DESC'")]
    pub order: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreatePaymentParams {
    #[schemars(description = "Invoice ID (UUID) to pay")]
    pub invoice_id: String,

    #[schemars(description = "Account code to pay from (e.g. bank account code)")]
    pub account_code: String,

    #[schemars(description = "Payment amount")]
    pub amount: f64,

    #[schemars(description = "Payment date in YYYY-MM-DD format")]
    pub date: Option<String>,

    #[schemars(description = "Payment reference")]
    pub reference: Option<String>,
}

// ---------------------------------------------------------------------------
// Credit Notes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCreditNotesParams {
    #[schemars(description = "Page number (starts at 1)")]
    pub page: Option<u32>,

    #[schemars(description = "Custom where filter")]
    pub where_filter: Option<String>,

    #[schemars(description = "Order results")]
    pub order: Option<String>,
}

// ---------------------------------------------------------------------------
// Quotes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListQuotesParams {
    #[schemars(description = "Page number (starts at 1)")]
    pub page: Option<u32>,

    #[schemars(description = "Filter by contact ID")]
    pub contact_id: Option<String>,

    #[schemars(description = "Filter by status: DRAFT, SENT, ACCEPTED, DECLINED")]
    pub status: Option<String>,

    #[schemars(description = "Order results, e.g. 'Date DESC'")]
    pub order: Option<String>,
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReportParams {
    #[schemars(description = "Report date in YYYY-MM-DD format (defaults to today)")]
    pub date: Option<String>,

    #[schemars(description = "Number of periods to show (for P&L and Balance Sheet)")]
    pub periods: Option<u32>,

    #[schemars(description = "Period timeframe: MONTH, QUARTER, YEAR")]
    pub timeframe: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgedReportParams {
    #[schemars(description = "Contact ID (UUID) to run the aged report for")]
    pub contact_id: String,

    #[schemars(description = "Report date in YYYY-MM-DD (defaults to today)")]
    pub date: Option<String>,
}

// ---------------------------------------------------------------------------
// Bank Transactions
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListBankTransactionsParams {
    #[schemars(description = "Page number (starts at 1)")]
    pub page: Option<u32>,

    #[schemars(description = "Custom where filter")]
    pub where_filter: Option<String>,

    #[schemars(description = "Order results, e.g. 'Date DESC'")]
    pub order: Option<String>,
}

// ---------------------------------------------------------------------------
// Tracking Categories
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTrackingCategoriesParams {
    #[schemars(description = "Custom where filter")]
    pub where_filter: Option<String>,
}

// ---------------------------------------------------------------------------
// Email
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EmailInvoiceParams {
    #[schemars(description = "Invoice ID (UUID) to email to the contact")]
    pub invoice_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendStatementParams {
    #[schemars(description = "Contact ID (UUID) to send statement of account to")]
    pub contact_id: String,
}
