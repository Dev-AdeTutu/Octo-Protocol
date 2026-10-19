//! Account lifecycle: personal-data export and account deletion (issue #466, migration 0032).
//!
//! Deletion tombstones the user rather than removing the row, so the (anonymised) audit trail and
//! the wallets' financial history survive. Callers must check on-chain balances first; the store
//! only guarantees that the wallet set it deletes is the one the caller checked.

use crate::audit_chain::{anonymise_chain, append_link};
use crate::models::{
    Address, ApiKey, AuditLog, PaymentLink, PaymentLinkPayment, Session, SponsoredTransaction,
    Transaction, User, Wallet, WebhookEndpoint, WhitelistedAddress, Withdrawal,
};
use crate::{AuditChainKey, Store, StoreError};
use uuid::Uuid;

/// Most rows returned per export section, so one request can't pull an unbounded result set.
pub const EXPORT_SECTION_LIMIT: i64 = 10_000;

/// Everything stored about one user, as raw rows. Holds secrets (password hash, sealed seeds,
/// webhook secrets, key hashes): callers must map it to secret-free views before returning it.
#[derive(Debug, Clone)]
pub struct AccountData {
    pub user: User,
    pub wallets: Vec<Wallet>,
    pub addresses: Vec<Address>,
    pub transactions: Vec<Transaction>,
    pub withdrawals: Vec<Withdrawal>,
    pub sponsored_transactions: Vec<SponsoredTransaction>,
    pub api_keys: Vec<ApiKey>,
    pub webhook_endpoints: Vec<WebhookEndpoint>,
    pub whitelisted_addresses: Vec<WhitelistedAddress>,
    pub payment_links: Vec<PaymentLink>,
    pub payment_link_payments: Vec<PaymentLinkPayment>,
    pub sessions: Vec<Session>,
    pub audit_logs: Vec<AuditLog>,
    /// Names of sections cut off at [`EXPORT_SECTION_LIMIT`].
    pub truncated: Vec<&'static str>,
}

/// What [`Store::delete_account`] changed.
#[derive(Debug, Clone, Copy)]
pub struct AccountDeletion {
    pub wallets_archived: u64,
    pub audit_rows_anonymised: u64,
}

/// Run one export query, fetching one extra row to detect truncation.
macro_rules! section {
    ($store:expr, $truncated:expr, $name:literal, $ty:ty, $sql:literal, $user:expr) => {{
        let mut rows = sqlx::query_as::<_, $ty>($sql)
            .bind($user)
            .bind(EXPORT_SECTION_LIMIT + 1)
            .fetch_all(&$store.pool)
            .await?;
        if rows.len() as i64 > EXPORT_SECTION_LIMIT {
            rows.truncate(EXPORT_SECTION_LIMIT as usize);
            $truncated.push($name);
        }
        rows
    }};
}

impl Store {
    /// Collect a live user's data for export. `NotFound` for unknown or deleted users.
    pub async fn export_account(&self, user_id: Uuid) -> Result<AccountData, StoreError> {
        let user = sqlx::query_as::<_, User>(
            "SELECT * FROM users WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;

        let mut truncated = Vec::new();
        let wallets = section!(self, truncated, "wallets", Wallet,
            "SELECT * FROM wallets WHERE user_id = $1 ORDER BY created_at, id LIMIT $2", user_id);
        let addresses = section!(self, truncated, "addresses", Address,
            "SELECT a.* FROM addresses a JOIN wallets w ON w.id = a.wallet_id
             WHERE w.user_id = $1 ORDER BY a.created_at, a.id LIMIT $2", user_id);
        let transactions = section!(self, truncated, "transactions", Transaction,
            "SELECT t.* FROM transactions t JOIN wallets w ON w.id = t.wallet_id
             WHERE w.user_id = $1 ORDER BY t.created_at, t.id LIMIT $2", user_id);
        let withdrawals = section!(self, truncated, "withdrawals", Withdrawal,
            "SELECT x.* FROM withdrawals x JOIN wallets w ON w.id = x.wallet_id
             WHERE w.user_id = $1 ORDER BY x.created_at, x.id LIMIT $2", user_id);
        let sponsored_transactions = section!(self, truncated, "sponsored_transactions",
            SponsoredTransaction,
            "SELECT s.* FROM sponsored_transactions s JOIN wallets w ON w.id = s.wallet_id
             WHERE w.user_id = $1 ORDER BY s.created_at, s.id LIMIT $2", user_id);
        let api_keys = section!(self, truncated, "api_keys", ApiKey,
            "SELECT k.* FROM api_keys k JOIN wallets w ON w.id = k.wallet_id
             WHERE w.user_id = $1 ORDER BY k.created_at, k.id LIMIT $2", user_id);
        let webhook_endpoints = section!(self, truncated, "webhook_endpoints", WebhookEndpoint,
            "SELECT e.* FROM webhook_endpoints e JOIN wallets w ON w.id = e.wallet_id
             WHERE w.user_id = $1 ORDER BY e.created_at, e.id LIMIT $2", user_id);
        let whitelisted_addresses = section!(self, truncated, "whitelisted_addresses",
            WhitelistedAddress,
            "SELECT a.* FROM whitelisted_addresses a JOIN wallets w ON w.id = a.wallet_id
             WHERE w.user_id = $1 ORDER BY a.created_at, a.id LIMIT $2", user_id);
        let payment_links = section!(self, truncated, "payment_links", PaymentLink,
            "SELECT l.* FROM payment_links l JOIN wallets w ON w.id = l.wallet_id
             WHERE w.user_id = $1 ORDER BY l.created_at, l.id LIMIT $2", user_id);
        let payment_link_payments = section!(self, truncated, "payment_link_payments",
            PaymentLinkPayment,
            "SELECT p.* FROM payment_link_payments p
             JOIN payment_links l ON l.id = p.payment_link_id
             JOIN wallets w ON w.id = l.wallet_id
             WHERE w.user_id = $1 ORDER BY p.created_at, p.id LIMIT $2", user_id);
        let sessions = section!(self, truncated, "sessions", Session,
            "SELECT * FROM sessions WHERE user_id = $1 ORDER BY created_at, id LIMIT $2", user_id);
        let audit_logs = section!(self, truncated, "audit_logs", AuditLog,
            "SELECT id, user_id, action, category, target, ip_address, created_at
             FROM audit_logs WHERE user_id = $1 ORDER BY created_at, id LIMIT $2", user_id);

        Ok(AccountData {
            user,
            wallets,
            addresses,
            transactions,
            withdrawals,
            sponsored_transactions,
            api_keys,
            webhook_endpoints,
            whitelisted_addresses,
            payment_links,
            payment_link_payments,
            sessions,
            audit_logs,
            truncated,
        })
    }

    /// Delete a user's account in one transaction: scrub the user row to a tombstone, revoke every
    /// credential, retire the wallets' integrations and anonymise the audit trail.
    ///
    /// `checked_wallets` are the wallet ids whose balances the caller verified as empty; if the
    /// user's wallet set differs (one was created since), nothing is changed and
    /// [`StoreError::Conflict`] is returned. Financial rows (transactions, withdrawals) are kept.
    pub async fn delete_account(
        &self,
        key: &AuditChainKey,
        user_id: Uuid,
        checked_wallets: &[Uuid],
    ) -> Result<AccountDeletion, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Row lock: two concurrent deletions (or a deletion racing a login) serialize here.
        let email: String = sqlx::query_scalar(
            "SELECT email FROM users WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        let mut current: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM wallets WHERE user_id = $1 FOR UPDATE")
                .bind(user_id)
                .fetch_all(&mut *tx)
                .await?;
        let mut checked = checked_wallets.to_vec();
        current.sort_unstable();
        checked.sort_unstable();
        if current != checked {
            return Err(StoreError::Conflict);
        }

        // The `.invalid` TLD can never receive mail or collide with a real signup address.
        sqlx::query(
            "UPDATE users
             SET email = 'deleted+' || id::text || '@users.invalid', username = NULL,
                 password_hash = '!deleted', deleted_at = now(),
                 session_epoch = session_epoch + 1, updated_at = now()
             WHERE id = $1",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE sessions SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM email_otps WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM auth_lockouts WHERE email = lower($1)")
            .bind(&email)
            .execute(&mut *tx)
            .await?;

        // Wallets stay (ingest keeps recording late deposits), but nothing can act on them.
        let wallets_archived = sqlx::query(
            "UPDATE wallets
             SET archived_at = COALESCE(archived_at, now()), label = NULL, description = NULL,
                 encrypted_backup = NULL, updated_at = now()
             WHERE user_id = $1",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "DELETE FROM api_keys WHERE wallet_id IN (SELECT id FROM wallets WHERE user_id = $1)",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE webhook_endpoints SET active = false, deleted_at = COALESCE(deleted_at, now())
             WHERE wallet_id IN (SELECT id FROM wallets WHERE user_id = $1)",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM whitelisted_addresses
             WHERE wallet_id IN (SELECT id FROM wallets WHERE user_id = $1)",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE addresses SET customer_ref = NULL, metadata = '{}'::jsonb
             WHERE wallet_id IN (SELECT id FROM wallets WHERE user_id = $1)",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE payment_links SET active = false, updated_at = now()
             WHERE wallet_id IN (SELECT id FROM wallets WHERE user_id = $1)",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        // Payers are third parties: their names and emails go with the merchant's account.
        sqlx::query(
            "UPDATE payment_link_payments SET payer_name = NULL, payer_email = NULL
             WHERE payment_link_id IN (
                 SELECT l.id FROM payment_links l JOIN wallets w ON w.id = l.wallet_id
                 WHERE w.user_id = $1
             )",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

        // Runs after deleted_at is set: the append-only trigger only permits it for deleted users.
        let audit_rows_anonymised = anonymise_chain(&mut tx, key, user_id).await?;
        append_link(&mut tx, key, user_id, "deleted their account", "authentication", None, None)
            .await?;

        tx.commit().await?;
        Ok(AccountDeletion {
            wallets_archived,
            audit_rows_anonymised,
        })
    }
}
