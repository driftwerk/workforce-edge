//! Per-tenant settings and secrets at rest: the `tenant_settings` row.
//!
//! What `edge.toml` holds for an install — LLM provider and key, grader,
//! token exchange, admin OIDC, limits, audit — a cloud tenant holds in one
//! row, and a self-hosted install may too. The file seeds the `default`
//! tenant's row on first boot, then the row owns it, as `edge_agents` does
//! for agents. Each section is a JSONB column holding the config struct
//! without its secrets; the secrets sit beside it in BYTEA columns sealed
//! under the install's master key (`EDGE_MASTER_KEY`), bound to the tenant
//! and column, so a dump of the table yields no key and a sealed value moved
//! to another tenant's row fails to open.
//!
//! Without a master key nothing here is used: the file governs, as it did
//! before the table existed.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use eyre::{eyre, Result, WrapErr};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio_postgres::Row;
use workforce_agent::llm::failover::{classify_error, ErrorKind};
use workforce_agent::llm::provider::{create_provider, ProviderKind};
use workforce_agent::llm::tiers::TierRegistry;
use workforce_crypto::{MasterKey, Sealed};

use crate::admin::AdminConfig;
use crate::config::{AuditConfig, GraderConfig, LlmConfig, TokenExchangeConfig};
use crate::outbound::OutboundPolicy;
use crate::tenant::{TenantId, TenantLimits, TenantPool, TenantSettings};

/// The `outbound` column's spelling of an [`OutboundPolicy`], matching its
/// check constraint and its serde form.
fn outbound_as_str(policy: OutboundPolicy) -> &'static str {
    match policy {
        OutboundPolicy::Private => "private",
        OutboundPolicy::Public => "public",
    }
}

fn parse_outbound(value: &str) -> Result<OutboundPolicy> {
    match value {
        "private" => Ok(OutboundPolicy::Private),
        "public" => Ok(OutboundPolicy::Public),
        other => Err(eyre!("unknown outbound mode {other:?}")),
    }
}

/// The last known state of a tenant's LLM key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmState {
    Ok,
    Failed,
    Unset,
}

/// What the admin banner shows about the tenant's key: its state, when it
/// was last established, and a fixed message per failure class. The message
/// is never a provider's own text — provider errors echo request details
/// and can echo headers — so no key material can reach the row or the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmStatus {
    pub state: LlmState,
    pub checked_at: DateTime<Utc>,
    pub message: String,
}

impl LlmStatus {
    pub fn ok() -> Self {
        Self {
            state: LlmState::Ok,
            checked_at: Utc::now(),
            message: String::new(),
        }
    }

    pub fn failed(failure: KeyFailure) -> Self {
        Self {
            state: LlmState::Failed,
            checked_at: Utc::now(),
            message: failure.message().to_string(),
        }
    }

    pub fn unset() -> Self {
        Self {
            state: LlmState::Unset,
            checked_at: Utc::now(),
            message: String::new(),
        }
    }
}

/// Why a key did not validate, as one of a fixed set of classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyFailure {
    Unauthorized,
    RateLimited,
    Unreachable,
    UnknownProvider,
    /// The provider takes no bearer key: a URL in the key's place (Ollama,
    /// vLLM), the process's own credential chain (Bedrock, Vertex) or a
    /// login. A tenant's key cannot name one.
    ProviderNotAllowed,
    /// The provider answered and refused the request for a reason other
    /// than the key: a model it does not serve, a billing hold, a key of a
    /// kind this edge cannot run on.
    Rejected,
}

impl KeyFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::Unauthorized => "The provider rejected the key as unauthorized.",
            Self::RateLimited => "The provider is rate-limiting this key; try again shortly.",
            Self::Unreachable => "The provider could not be reached.",
            Self::UnknownProvider => "The configured provider is not one this edge can run.",
            Self::ProviderNotAllowed => {
                "provider not allowed: it takes no API key, so a tenant cannot bring one."
            }
            Self::Rejected => "The provider refused the validation request.",
        }
    }

    /// The class of a provider error, by the same rules the failover engine
    /// applies to a turn's errors.
    pub fn classify(error: &str) -> Self {
        match classify_error(error) {
            ErrorKind::Auth => Self::Unauthorized,
            ErrorKind::RateLimited => Self::RateLimited,
            ErrorKind::Billing
            | ErrorKind::ClientError
            | ErrorKind::ContextOverflow
            | ErrorKind::Unsupported
            | ErrorKind::SubscriptionUnavailable => Self::Rejected,
            _ => Self::Unreachable,
        }
    }
}

/// Ceiling on the validation probe. The provider clients own their HTTP
/// client, so the probe's response body cannot be capped from here; the
/// timeout and the bearer-key rule ([`provider_takes_a_key`]) are what
/// bound it — a tenant can only ever point the probe at a provider's own
/// endpoint.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether `provider` is one a tenant may bring a key for: a provider that
/// authenticates with a bearer key. The rest take a URL in the key's place
/// (which would let a tenant's key point the edge at any address), run on
/// the process's own credential chain, or need a login.
pub fn provider_takes_a_key(provider: &str) -> Result<(), KeyFailure> {
    match ProviderKind::from_str_loose(provider) {
        None => Err(KeyFailure::UnknownProvider),
        Some(kind) if kind.takes_api_key() => Ok(()),
        Some(_) => Err(KeyFailure::ProviderNotAllowed),
    }
}

/// Validate `key` for the tenant's provider and model with the smallest call
/// the provider offers (`verify_credentials`: an auth-only probe where the
/// provider has one, a one-token completion otherwise). A provider that
/// takes no bearer key is refused before any call, whatever the install's
/// allowlist says.
pub async fn validate_llm_key(llm: &LlmConfig, key: &str) -> Result<(), KeyFailure> {
    provider_takes_a_key(&llm.provider)?;
    probe_key(llm, key).await
}

/// The probe behind [`validate_llm_key`], without its provider rule.
async fn probe_key(llm: &LlmConfig, key: &str) -> Result<(), KeyFailure> {
    let Some(kind) = ProviderKind::from_str_loose(&llm.provider) else {
        return Err(KeyFailure::UnknownProvider);
    };
    let tiers = TierRegistry::from_defaults().unwrap_or_else(|_| TierRegistry::new(None));
    let tier_key = llm
        .tier_key
        .clone()
        .unwrap_or_else(|| crate::host::default_tier_key(&llm.provider));
    let model = llm
        .default_model
        .as_deref()
        .map(|spec| tiers.resolve_model(spec, &tier_key));
    let provider = create_provider(kind, key, model.as_deref(), None, false)
        .map_err(|_| KeyFailure::Rejected)?;
    match tokio::time::timeout(PROBE_TIMEOUT, provider.verify_credentials()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(KeyFailure::classify(&format!("{e:#}"))),
        Err(_) => Err(KeyFailure::Unreachable),
    }
}

/// One tenant's settings row, secrets opened into the config structs they
/// belong to. A struct built from the file ([`Self::from_settings`]) has the
/// same shape, so the row and the file compare section by section.
#[derive(Clone, Debug)]
pub struct StoredSettings {
    /// `api_key` is the opened `llm_api_key` column, `None` when unset.
    pub llm: LlmConfig,
    pub llm_status: Option<LlmStatus>,
    pub grader: Option<GraderConfig>,
    /// `client_secret` is the opened `token_exchange_secret` column, empty
    /// when unset.
    pub token_exchange: Option<TokenExchangeConfig>,
    /// `oidc.client_secret` is the opened `admin_client_secret` column.
    pub admin: Option<AdminConfig>,
    pub limits: TenantLimits,
    pub audit: Option<AuditConfig>,
    /// How the tenant's own URLs are reached; see [`OutboundPolicy`].
    pub outbound: OutboundPolicy,
}

impl StoredSettings {
    /// A tenant's settings as its row would hold them.
    pub fn from_settings(settings: &TenantSettings) -> Self {
        Self {
            llm: settings.llm.clone(),
            llm_status: settings.llm_status.clone(),
            grader: settings.grader.clone(),
            token_exchange: settings.token_exchange.clone(),
            admin: settings.admin.clone(),
            limits: settings.limits,
            audit: settings.audit.clone(),
            outbound: settings.outbound,
        }
    }

    pub fn has_llm_key(&self) -> bool {
        self.llm
            .api_key
            .as_deref()
            .is_some_and(|key| !key.is_empty())
    }

    pub fn has_token_exchange_secret(&self) -> bool {
        self.token_exchange
            .as_ref()
            .is_some_and(|te| !te.client_secret.is_empty())
    }

    pub fn has_admin_client_secret(&self) -> bool {
        self.admin
            .as_ref()
            .and_then(|admin| admin.oidc.as_ref())
            .and_then(|oidc| oidc.client_secret.as_deref())
            .is_some_and(|secret| !secret.is_empty())
    }
}

/// The sealed columns, each bound into its own AAD so a value cannot be
/// moved between them.
#[derive(Clone, Copy)]
enum SecretColumn {
    LlmApiKey,
    TokenExchangeSecret,
    AdminClientSecret,
    VellevApiKey,
}

impl SecretColumn {
    fn name(self) -> &'static str {
        match self {
            Self::LlmApiKey => "llm_api_key",
            Self::TokenExchangeSecret => "token_exchange_secret",
            Self::AdminClientSecret => "admin_client_secret",
            Self::VellevApiKey => "vellev_api_key",
        }
    }
}

/// A tenant's `tenant_settings` row: read whole, written section by section,
/// secrets sealed on the way in and opened on the way out. Bound to one
/// tenant like every other edge store.
#[derive(Clone)]
pub struct TenantSettingsStore {
    pool: TenantPool,
    tenant: TenantId,
    master: Arc<MasterKey>,
}

const COLUMNS: &str =
    "llm, llm_api_key, llm_status, grader, token_exchange, token_exchange_secret, \
                       admin, admin_client_secret, limits, audit, outbound";

impl TenantSettingsStore {
    pub fn new(pool: TenantPool, tenant: TenantId, master: Arc<MasterKey>) -> Self {
        Self {
            pool,
            tenant,
            master,
        }
    }

    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    fn aad(&self, column: SecretColumn) -> Vec<u8> {
        format!("edge-settings/v1/{}/{}", self.tenant, column.name()).into_bytes()
    }

    fn seal(&self, column: SecretColumn, secret: &str) -> Result<Vec<u8>> {
        self.master
            .seal(&self.aad(column), secret.as_bytes())
            .map(|sealed| sealed.to_bytes())
            .wrap_err_with(|| format!("sealing {} for tenant {}", column.name(), self.tenant))
    }

    /// An empty secret is stored as no secret, so a cleared field never
    /// leaves a sealed empty string behind that reads as "set".
    fn seal_opt(&self, column: SecretColumn, secret: Option<&str>) -> Result<Option<Vec<u8>>> {
        secret
            .filter(|secret| !secret.is_empty())
            .map(|secret| self.seal(column, secret))
            .transpose()
    }

    fn open(&self, column: SecretColumn, bytes: Option<Vec<u8>>) -> Result<Option<String>> {
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let opened = Sealed::from_bytes(&bytes)
            .and_then(|sealed| self.master.open(&self.aad(column), &sealed))
            .wrap_err_with(|| {
                format!(
                    "opening {} for tenant {}: is EDGE_MASTER_KEY the key it was sealed under?",
                    column.name(),
                    self.tenant
                )
            })?;
        let secret = String::from_utf8(opened.to_vec()).wrap_err_with(|| {
            format!("{} for tenant {} is not text", column.name(), self.tenant)
        })?;
        Ok(Some(secret))
    }

    /// Whether the tenant has a settings row at all.
    pub async fn exists(&self) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM tenant_settings WHERE tenant_id = $1)",
                &[&self.tenant],
            )
            .await
            .wrap_err_with(|| format!("checking for the settings of tenant {}", self.tenant))?;
        conn.commit().await?;
        Ok(row.get(0))
    }

    /// The key the tenant's own vellev tenant is reached with, opened; `None`
    /// without a row or a key. Read on its own so building the tenant's store
    /// opens no other secret of the row.
    pub async fn vellev_api_key(&self) -> Result<Option<String>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_opt(
                "SELECT vellev_api_key FROM tenant_settings WHERE tenant_id = $1",
                &[&self.tenant],
            )
            .await
            .wrap_err_with(|| format!("reading the vellev key of tenant {}", self.tenant))?;
        conn.commit().await?;
        row.map(|row| self.open(SecretColumn::VellevApiKey, row.get(0)))
            .transpose()
            .map(Option::flatten)
    }

    pub async fn set_vellev_api_key(&self, key: &str) -> Result<()> {
        self.set_secret(SecretColumn::VellevApiKey, Some(key)).await
    }

    pub async fn load(&self) -> Result<Option<StoredSettings>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_opt(
                &format!("SELECT {COLUMNS} FROM tenant_settings WHERE tenant_id = $1"),
                &[&self.tenant],
            )
            .await
            .wrap_err_with(|| format!("reading settings for tenant {}", self.tenant))?;
        conn.commit().await?;
        row.map(|row| self.decode(&row)).transpose()
    }

    fn decode(&self, row: &Row) -> Result<StoredSettings> {
        let mut llm: LlmConfig = decode_json(row.get(0), "llm")?;
        llm.api_key = self.open(SecretColumn::LlmApiKey, row.get(1))?;
        let mut token_exchange: Option<TokenExchangeConfig> =
            decode_opt_json(row.get(4), "token_exchange")?;
        if let Some(te) = &mut token_exchange {
            te.client_secret = self
                .open(SecretColumn::TokenExchangeSecret, row.get(5))?
                .unwrap_or_default();
        }
        let mut admin: Option<AdminConfig> = decode_opt_json(row.get(6), "admin")?;
        if let Some(oidc) = admin.as_mut().and_then(|admin| admin.oidc.as_mut()) {
            oidc.client_secret = self.open(SecretColumn::AdminClientSecret, row.get(7))?;
        }
        Ok(StoredSettings {
            llm,
            llm_status: decode_opt_json(row.get(2), "llm_status")?,
            grader: decode_opt_json(row.get(3), "grader")?,
            token_exchange,
            admin,
            limits: decode_json(row.get(8), "limits")?,
            audit: decode_opt_json(row.get(9), "audit")?,
            outbound: parse_outbound(row.get(10))?,
        })
    }

    /// Write the non-secret sections, creating the row if absent. The sealed
    /// columns and `llm_status` are left as they are: secrets change only
    /// through their own setters, so a settings edit can never drop a key.
    pub async fn save(&self, settings: &StoredSettings) -> Result<()> {
        let conn = self.pool.conn(&self.tenant).await?;
        conn.execute(
            "INSERT INTO tenant_settings \
                 (tenant_id, llm, grader, token_exchange, admin, limits, audit, outbound) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (tenant_id) DO UPDATE SET \
                 llm = EXCLUDED.llm, grader = EXCLUDED.grader, \
                 token_exchange = EXCLUDED.token_exchange, admin = EXCLUDED.admin, \
                 limits = EXCLUDED.limits, audit = EXCLUDED.audit, \
                 outbound = EXCLUDED.outbound, updated_at = now()",
            &[
                &self.tenant,
                &encode_json(&settings.llm)?,
                &encode_opt_json(&settings.grader)?,
                &encode_opt_json(&settings.token_exchange)?,
                &encode_opt_json(&settings.admin)?,
                &encode_json(&settings.limits)?,
                &encode_opt_json(&settings.audit)?,
                &outbound_as_str(settings.outbound),
            ],
        )
        .await
        .wrap_err_with(|| format!("saving settings for tenant {}", self.tenant))?;
        conn.commit().await
    }

    /// First boot: write the file's sections, secrets sealed, unless the row
    /// exists — once it does, the row owns the settings and the file's are
    /// ignored. Returns whether seeding ran.
    pub async fn seed_if_empty(&self, settings: &TenantSettings) -> Result<bool> {
        let stored = StoredSettings::from_settings(settings);
        let conn = self.pool.conn(&self.tenant).await?;
        let inserted = conn
            .execute(
                "INSERT INTO tenant_settings \
                     (tenant_id, llm, llm_api_key, llm_status, grader, token_exchange, \
                      token_exchange_secret, admin, admin_client_secret, limits, audit, outbound) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 ON CONFLICT (tenant_id) DO NOTHING",
                &[
                    &self.tenant,
                    &encode_json(&stored.llm)?,
                    &self.seal_opt(SecretColumn::LlmApiKey, stored.llm.api_key.as_deref())?,
                    &encode_opt_json(&stored.llm_status)?,
                    &encode_opt_json(&stored.grader)?,
                    &encode_opt_json(&stored.token_exchange)?,
                    &self.seal_opt(
                        SecretColumn::TokenExchangeSecret,
                        stored
                            .token_exchange
                            .as_ref()
                            .map(|te| te.client_secret.as_str()),
                    )?,
                    &encode_opt_json(&stored.admin)?,
                    &self.seal_opt(
                        SecretColumn::AdminClientSecret,
                        stored
                            .admin
                            .as_ref()
                            .and_then(|admin| admin.oidc.as_ref())
                            .and_then(|oidc| oidc.client_secret.as_deref()),
                    )?,
                    &encode_json(&stored.limits)?,
                    &encode_opt_json(&stored.audit)?,
                    &outbound_as_str(stored.outbound),
                ],
            )
            .await
            .wrap_err_with(|| format!("seeding settings for tenant {}", self.tenant))?;
        conn.commit().await?;
        if inserted == 1 {
            tracing::info!(tenant = %self.tenant, "seeded tenant_settings from edge.toml");
        }
        Ok(inserted == 1)
    }

    /// Set the tenant's outbound policy — the platform's call, never the
    /// tenant's own ([`crate::server`]'s settings save leaves it as it is).
    /// The row must exist; a caller seeds it first.
    pub async fn set_outbound(&self, policy: OutboundPolicy) -> Result<()> {
        let conn = self.pool.conn(&self.tenant).await?;
        let updated = conn
            .execute(
                "UPDATE tenant_settings SET outbound = $2, updated_at = now() WHERE tenant_id = $1",
                &[&self.tenant, &outbound_as_str(policy)],
            )
            .await
            .wrap_err_with(|| format!("storing the outbound policy of tenant {}", self.tenant))?;
        conn.commit().await?;
        if updated == 0 {
            return Err(eyre!(
                "tenant {} has no settings row to hold its outbound policy",
                self.tenant
            ));
        }
        Ok(())
    }

    /// Set the tenant's admin section alone — the platform provisioning a
    /// sign-in for a tenant that has none, whose own settings API is behind
    /// the very admin it lacks. The secrets beside it are untouched, as with
    /// [`Self::save`]. The row must exist; a caller seeds it first.
    pub async fn set_admin(&self, admin: &AdminConfig) -> Result<()> {
        let conn = self.pool.conn(&self.tenant).await?;
        let updated = conn
            .execute(
                "UPDATE tenant_settings SET admin = $2, updated_at = now() WHERE tenant_id = $1",
                &[&self.tenant, &encode_json(admin)?],
            )
            .await
            .wrap_err_with(|| format!("storing the admin section of tenant {}", self.tenant))?;
        conn.commit().await?;
        if updated == 0 {
            return Err(eyre!(
                "tenant {} has no settings row to hold its admin section",
                self.tenant
            ));
        }
        Ok(())
    }

    /// Whether the row seals an admin OIDC client secret: `None` without a
    /// row. Answered without opening any column, so a row sealed under
    /// another key still answers.
    pub async fn holds_admin_client_secret(&self) -> Result<Option<bool>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_opt(
                "SELECT admin_client_secret IS NOT NULL FROM tenant_settings WHERE tenant_id = $1",
                &[&self.tenant],
            )
            .await
            .wrap_err_with(|| {
                format!("checking the admin client secret of tenant {}", self.tenant)
            })?;
        conn.commit().await?;
        Ok(row.map(|row| row.get(0)))
    }

    /// Drop the row, secrets and all; the file governs the tenant again.
    /// Returns whether there was one.
    pub async fn delete(&self) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let deleted = conn
            .execute(
                "DELETE FROM tenant_settings WHERE tenant_id = $1",
                &[&self.tenant],
            )
            .await
            .wrap_err_with(|| format!("deleting settings for tenant {}", self.tenant))?;
        conn.commit().await?;
        Ok(deleted == 1)
    }

    pub async fn set_llm_key(&self, key: &str) -> Result<()> {
        self.set_secret(SecretColumn::LlmApiKey, Some(key)).await
    }

    pub async fn clear_llm_key(&self) -> Result<()> {
        self.set_secret(SecretColumn::LlmApiKey, None).await
    }

    pub async fn set_token_exchange_secret(&self, secret: Option<&str>) -> Result<()> {
        self.set_secret(SecretColumn::TokenExchangeSecret, secret)
            .await
    }

    pub async fn set_admin_client_secret(&self, secret: Option<&str>) -> Result<()> {
        self.set_secret(SecretColumn::AdminClientSecret, secret)
            .await
    }

    /// A secret goes into an existing row only: there is no partial row to
    /// hang it on, so a caller seeds the row first.
    async fn set_secret(&self, column: SecretColumn, secret: Option<&str>) -> Result<()> {
        let sealed = self.seal_opt(column, secret)?;
        let conn = self.pool.conn(&self.tenant).await?;
        let updated = conn
            .execute(
                &format!(
                    "UPDATE tenant_settings SET {} = $2, updated_at = now() WHERE tenant_id = $1",
                    column.name()
                ),
                &[&self.tenant, &sealed],
            )
            .await
            .wrap_err_with(|| format!("storing {} for tenant {}", column.name(), self.tenant))?;
        conn.commit().await?;
        if updated == 0 {
            return Err(eyre!(
                "tenant {} has no settings row to hold {}",
                self.tenant,
                column.name()
            ));
        }
        Ok(())
    }

    /// Record the key's state. Returns whether a row held it: a tenant that
    /// has never saved settings has none, and its status lives in memory.
    pub async fn set_llm_status(&self, status: &LlmStatus) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let updated = conn
            .execute(
                "UPDATE tenant_settings SET llm_status = $2 WHERE tenant_id = $1",
                &[&self.tenant, &encode_json(status)?],
            )
            .await
            .wrap_err_with(|| format!("storing llm status for tenant {}", self.tenant))?;
        conn.commit().await?;
        Ok(updated == 1)
    }
}

fn encode_json<T: Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).wrap_err("encoding a settings section")
}

fn encode_opt_json<T: Serialize>(value: &Option<T>) -> Result<Option<serde_json::Value>> {
    value.as_ref().map(encode_json).transpose()
}

fn decode_json<T: DeserializeOwned>(value: serde_json::Value, section: &str) -> Result<T> {
    serde_json::from_value(value)
        .wrap_err_with(|| format!("decoding the stored {section} settings (schema drift?)"))
}

fn decode_opt_json<T: DeserializeOwned>(
    value: Option<serde_json::Value>,
    section: &str,
) -> Result<Option<T>> {
    value.map(|value| decode_json(value, section)).transpose()
}

/// The tenant's live key state, as its hosts observe it turn by turn and the
/// admin's key endpoints set it. Held in memory for the banner and written
/// through to the row (when the install has one) only on a change, so a
/// healthy tenant's turns cost no settings writes.
pub struct LlmStatusReporter {
    store: Option<TenantSettingsStore>,
    current: Mutex<LlmStatus>,
}

impl LlmStatusReporter {
    pub fn new(store: Option<TenantSettingsStore>, initial: Option<LlmStatus>) -> Self {
        Self {
            store,
            current: Mutex::new(initial.unwrap_or_else(LlmStatus::unset)),
        }
    }

    pub fn current(&self) -> LlmStatus {
        self.current.lock().expect("llm status lock").clone()
    }

    /// Record `status`, in memory at once and in the row best-effort: a
    /// failed write is logged, since the state it records is the turn's or
    /// the operator's, not this reporter's.
    pub async fn set(&self, status: LlmStatus) {
        *self.current.lock().expect("llm status lock") = status.clone();
        if let Some(store) = &self.store {
            if let Err(e) = store.set_llm_status(&status).await {
                tracing::warn!(tenant = %store.tenant, error = %e, "llm status write failed");
            }
        }
    }

    /// A turn's model call succeeded: whatever the key's state was, it is
    /// good now.
    pub async fn turn_succeeded(&self) {
        if self.current().state != LlmState::Ok {
            self.set(LlmStatus::ok()).await;
        }
    }

    /// A turn failed with `error`. Only an authentication failure says
    /// anything about the key, so only that class flips the state; a
    /// provider outage leaves it as it was.
    pub async fn turn_failed(&self, error: &str) {
        if KeyFailure::classify(error) != KeyFailure::Unauthorized {
            return;
        }
        let failed = LlmStatus::failed(KeyFailure::Unauthorized);
        let current = self.current();
        if current.state != LlmState::Failed || current.message != failed.message {
            self.set(failed).await;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::admin::AdminOidc;
    use crate::config::EdgeConfig;
    use crate::tenant::rls_tests::with_bound_pool;
    use crate::tenant::{run_edge_migrations, TenantStore};
    use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
    use workforce_postgres::test_support::with_test_pool;
    use workforce_postgres::PostgresPool;

    pub(crate) fn test_master_key() -> Arc<MasterKey> {
        Arc::new(MasterKey::from_bytes(&[7u8; 32]))
    }

    /// A file with every section the row holds, each carrying a secret.
    pub(crate) fn full_config() -> EdgeConfig {
        toml::from_str(
            r#"
            [server]
            bind = "0.0.0.0:8080"
            rate_limit_per_min = 120
            user_rate_limit_per_min = 12
            max_concurrent_turns = 3

            [postgres]
            url = "postgres://edge/edge"

            [vellev]
            url = "http://vellev:18080"
            api_key = "k"
            tenant = "harbor"

            [llm]
            provider = "anthropic"
            api_key = "sk-ant-file-key-123456"
            default_model = "standard"

            [grader]
            enabled = true
            sample_rate = 0.5

            [token_exchange]
            issuer = "https://kc.example/realms/x"
            client_id = "edge"
            client_secret = "te-secret-abc"

            [audit]
            enabled = true
            retention_days = 3

            [admin]
            session_secret = "0123456789abcdef0123456789abcdef"
            allowed_emails = ["ops@harbor.example"]
            [admin.oidc]
            issuer = "https://kc.example/realms/ops"
            client_id = "edge-admin"
            client_secret = "oidc-secret-xyz"
            "#,
        )
        .expect("a valid test config")
    }

    async fn migrated(pool: &PostgresPool) {
        run_session_migrations(pool).await.unwrap();
        run_edge_migrations(pool).await.unwrap();
    }

    async fn raw_secret(pool: &PostgresPool, tenant: &str, column: &str) -> Option<Vec<u8>> {
        pool.get()
            .await
            .unwrap()
            .query_one(
                &format!("SELECT {column} FROM tenant_settings WHERE tenant_id = $1"),
                &[&tenant],
            )
            .await
            .unwrap()
            .get(0)
    }

    #[tokio::test]
    async fn secrets_are_sealed_at_rest_and_opened_on_load() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrated(&pool).await;
            let store = TenantSettingsStore::new(
                TenantPool::new(pool.clone()),
                TenantId::default_tenant(),
                test_master_key(),
            );
            let file = TenantSettings::from_edge_toml(&full_config());
            assert!(store.load().await.unwrap().is_none());
            assert!(store.seed_if_empty(&file).await.unwrap());

            for (column, secret) in [
                ("llm_api_key", "sk-ant-file-key-123456"),
                ("token_exchange_secret", "te-secret-abc"),
                ("admin_client_secret", "oidc-secret-xyz"),
            ] {
                let raw = raw_secret(&pool, "default", column)
                    .await
                    .unwrap_or_else(|| panic!("{column} is set"));
                assert_ne!(raw, secret.as_bytes());
                assert!(
                    !raw.windows(secret.len()).any(|w| w == secret.as_bytes()),
                    "{column} holds the plaintext"
                );
            }
            let json: serde_json::Value = pool
                .get()
                .await
                .unwrap()
                .query_one(
                    "SELECT llm || coalesce(grader, '{}') || coalesce(token_exchange, '{}') \
                            || coalesce(admin, '{}') || coalesce(admin -> 'oidc', '{}') \
                     FROM tenant_settings WHERE tenant_id = 'default'",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            for secret in [
                "api_key",
                "client_secret",
                "session_secret",
                "password_hash",
            ] {
                assert!(json.get(secret).is_none(), "{secret} serialised into JSONB");
            }

            let stored = store.load().await.unwrap().expect("seeded");
            assert_eq!(
                stored.llm.api_key.as_deref(),
                Some("sk-ant-file-key-123456")
            );
            assert_eq!(stored.llm.default_model.as_deref(), Some("standard"));
            assert_eq!(
                stored.token_exchange.as_ref().unwrap().client_secret,
                "te-secret-abc"
            );
            assert_eq!(
                stored
                    .admin
                    .as_ref()
                    .unwrap()
                    .oidc
                    .as_ref()
                    .unwrap()
                    .client_secret
                    .as_deref(),
                Some("oidc-secret-xyz")
            );
            assert!(
                stored.admin.as_ref().unwrap().session_secret.is_none(),
                "the session secret stays in the file"
            );
            assert_eq!(stored.limits.max_concurrent_turns, 3);
            assert_eq!(stored.audit.as_ref().unwrap().retention_days, 3);
            assert_eq!(stored.outbound, OutboundPolicy::Private);
            assert!(stored.has_llm_key());
            assert!(stored.has_token_exchange_secret());
            assert!(stored.has_admin_client_secret());

            let other = TenantSettingsStore::new(
                TenantPool::new(pool.clone()),
                TenantId::default_tenant(),
                Arc::new(MasterKey::from_bytes(&[8u8; 32])),
            );
            let err = other.load().await.unwrap_err();
            assert!(format!("{err:#}").contains("EDGE_MASTER_KEY"), "{err:#}");
            assert!(!format!("{err:#}").contains("sk-ant"), "{err:#}");
        })
        .await;
    }

    #[tokio::test]
    async fn seeding_runs_on_first_boot_only_and_saves_keep_secrets() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrated(&pool).await;
            let store = TenantSettingsStore::new(
                TenantPool::new(pool.clone()),
                TenantId::default_tenant(),
                test_master_key(),
            );
            let file = TenantSettings::from_edge_toml(&full_config());
            assert!(store.seed_if_empty(&file).await.unwrap());

            let mut edited = store.load().await.unwrap().unwrap();
            edited.limits.max_concurrent_turns = 9;
            edited.llm.default_model = Some("cheap".into());
            edited.llm.api_key = Some("ignored-by-save".into());
            edited.outbound = OutboundPolicy::Public;
            store.save(&edited).await.unwrap();
            assert!(
                !format!("{edited:?}").contains("ignored-by-save")
                    && !format!("{edited:?}").contains("te-secret")
                    && !format!("{edited:?}").contains("oidc-secret"),
                "the settings never debug-print a secret: {edited:?}"
            );

            let mut changed_file = file.clone();
            changed_file.limits.max_concurrent_turns = 1;
            assert!(
                !store.seed_if_empty(&changed_file).await.unwrap(),
                "a second boot never reseeds"
            );

            let stored = store.load().await.unwrap().unwrap();
            assert_eq!(
                stored.limits.max_concurrent_turns, 9,
                "the row owns the settings"
            );
            assert_eq!(stored.llm.default_model.as_deref(), Some("cheap"));
            assert_eq!(stored.outbound, OutboundPolicy::Public);
            assert_eq!(
                stored.llm.api_key.as_deref(),
                Some("sk-ant-file-key-123456"),
                "a settings save never touches the key"
            );

            store.set_llm_key("sk-ant-new-key").await.unwrap();
            let ok = LlmStatus::ok();
            assert!(store.set_llm_status(&ok).await.unwrap());
            let stored = store.load().await.unwrap().unwrap();
            assert_eq!(stored.llm.api_key.as_deref(), Some("sk-ant-new-key"));
            assert_eq!(stored.llm_status, Some(ok));

            assert_eq!(store.holds_admin_client_secret().await.unwrap(), Some(true));
            store.clear_llm_key().await.unwrap();
            store.set_token_exchange_secret(Some("te-2")).await.unwrap();
            store.set_admin_client_secret(None).await.unwrap();
            store.set_outbound(OutboundPolicy::Private).await.unwrap();
            let mut admin = stored.admin.clone().unwrap();
            admin.allowed_emails = vec!["new@harbor.example".into()];
            store.set_admin(&admin).await.unwrap();
            let stored = store.load().await.unwrap().unwrap();
            assert!(!stored.has_llm_key());
            assert_eq!(
                stored.token_exchange.as_ref().unwrap().client_secret,
                "te-2"
            );
            assert!(!stored.has_admin_client_secret());
            assert_eq!(
                store.holds_admin_client_secret().await.unwrap(),
                Some(false)
            );
            assert_eq!(
                stored.admin.as_ref().unwrap().allowed_emails,
                ["new@harbor.example"],
                "the admin section is written on its own"
            );
            assert_eq!(
                stored.llm.default_model.as_deref(),
                Some("cheap"),
                "and nothing else"
            );
            assert_eq!(stored.outbound, OutboundPolicy::Private);

            let fresh = TenantSettingsStore::new(
                TenantPool::new(pool.clone()),
                TenantId::parse("acme").unwrap(),
                test_master_key(),
            );
            TenantStore::new(pool.clone())
                .create(&TenantId::parse("acme").unwrap(), "Acme")
                .await
                .unwrap();
            assert_eq!(fresh.holds_admin_client_secret().await.unwrap(), None);
            let err = fresh.set_llm_key("sk-x").await.unwrap_err();
            assert!(
                format!("{err:#}").contains("no settings row"),
                "a secret needs a row: {err:#}"
            );
            let err = fresh.set_admin(&admin).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("no settings row"),
                "so does the admin section: {err:#}"
            );
            let err = fresh
                .set_outbound(OutboundPolicy::Public)
                .await
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("no settings row"),
                "so does the policy: {err:#}"
            );
            assert!(!fresh.set_llm_status(&LlmStatus::ok()).await.unwrap());
        })
        .await;
    }

    #[tokio::test]
    async fn the_reporter_flips_on_an_auth_failure_and_back_on_success() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrated(&pool).await;
            let store = TenantSettingsStore::new(
                TenantPool::new(pool.clone()),
                TenantId::default_tenant(),
                test_master_key(),
            );
            store
                .seed_if_empty(&TenantSettings::from_edge_toml(&full_config()))
                .await
                .unwrap();
            let reporter = LlmStatusReporter::new(Some(store.clone()), None);
            assert_eq!(reporter.current().state, LlmState::Unset);

            reporter
                .turn_failed("vLLM API error (503 Service Unavailable): down")
                .await;
            assert_eq!(
                reporter.current().state,
                LlmState::Unset,
                "an outage says nothing about the key"
            );
            reporter
                .turn_failed("Anthropic API error (401 Unauthorized): invalid x-api-key")
                .await;
            let failed = reporter.current();
            assert_eq!(failed.state, LlmState::Failed);
            assert_eq!(failed.message, KeyFailure::Unauthorized.message());
            assert!(!failed.message.contains("x-api-key"));
            assert_eq!(
                store.load().await.unwrap().unwrap().llm_status,
                Some(failed),
                "the flip reaches the row"
            );

            reporter.turn_succeeded().await;
            assert_eq!(reporter.current().state, LlmState::Ok);
            assert_eq!(
                store
                    .load()
                    .await
                    .unwrap()
                    .unwrap()
                    .llm_status
                    .unwrap()
                    .state,
                LlmState::Ok
            );
        })
        .await;
    }

    #[test]
    fn failure_classes_carry_fixed_messages_only() {
        assert_eq!(
            KeyFailure::classify("API error (401 Unauthorized): bad key sk-ant-secret"),
            KeyFailure::Unauthorized
        );
        assert_eq!(
            KeyFailure::classify("API error (429 Too Many Requests)"),
            KeyFailure::RateLimited
        );
        assert_eq!(
            KeyFailure::classify("API error (404 Not Found): model x"),
            KeyFailure::Rejected
        );
        assert_eq!(
            KeyFailure::classify("HTTP request failed: connection refused"),
            KeyFailure::Unreachable
        );
        let status = LlmStatus::failed(KeyFailure::Unauthorized);
        assert!(!status.message.contains("sk-"));
        assert_eq!(status.state, LlmState::Failed);
    }

    /// A tenant may bring a key for a bearer-key provider only: one that
    /// takes a URL in the key's place, runs on the process's credentials or
    /// needs a login is refused before any call, and an unknown one too.
    #[tokio::test]
    async fn only_bearer_key_providers_take_a_tenants_key() {
        for provider in [
            "anthropic",
            "cerebras",
            "gemini",
            "openrouter",
            "grok",
            "zai",
        ] {
            assert_eq!(provider_takes_a_key(provider), Ok(()), "{provider}");
        }
        for provider in [
            "vllm",
            "ollama",
            "bedrock",
            "vertex-anthropic",
            "claude_code",
            "chatgpt",
            "codex",
        ] {
            assert_eq!(
                provider_takes_a_key(provider),
                Err(KeyFailure::ProviderNotAllowed),
                "{provider}"
            );
        }
        assert_eq!(
            provider_takes_a_key("no-such-provider"),
            Err(KeyFailure::UnknownProvider)
        );
        assert!(KeyFailure::ProviderNotAllowed
            .message()
            .starts_with("provider not allowed"));

        // The whole validation refuses without dialling: a URL-keyed provider
        // pointed at a closed port never gets the connection error it would.
        let llm = LlmConfig {
            provider: "vllm".into(),
            api_key: None,
            allowed_providers: vec![],
            tier_key: None,
            default_model: Some("m".into()),
            failover: vec![],
            retry: Default::default(),
        };
        assert_eq!(
            validate_llm_key(&llm, "http://127.0.0.1:1")
                .await
                .unwrap_err(),
            KeyFailure::ProviderNotAllowed
        );
    }

    /// The probe against a live endpoint: a 200 passes, a 401 is the
    /// unauthorized class, and an unknown provider never dials out. The
    /// vLLM provider takes its base URL in the key's place, which is what
    /// lets the probe reach a mock without a hook in the provider — so the
    /// probe is exercised below the provider rule that refuses vLLM.
    #[tokio::test]
    async fn a_key_validates_against_the_provider_with_one_probe() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            })))
            .expect(1)
            .mount(&server)
            .await;
        let llm = LlmConfig {
            provider: "vllm".into(),
            api_key: None,
            allowed_providers: vec![],
            tier_key: None,
            default_model: Some("m".into()),
            failover: vec![],
            retry: Default::default(),
        };
        probe_key(&llm, &server.uri()).await.unwrap();

        let denied = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
            .mount(&denied)
            .await;
        assert_eq!(
            probe_key(&llm, &denied.uri()).await.unwrap_err(),
            KeyFailure::Unauthorized
        );

        let unknown = LlmConfig {
            provider: "no-such-provider".into(),
            ..llm
        };
        assert_eq!(
            validate_llm_key(&unknown, "k").await.unwrap_err(),
            KeyFailure::UnknownProvider
        );
    }

    /// The row's policy keeps one tenant's settings from another's
    /// connection, and the store's own predicate agrees.
    #[tokio::test]
    async fn another_tenant_cannot_read_a_settings_row_under_rls() {
        with_bound_pool(|superuser, bound| async move {
            let registry = TenantStore::new(superuser.clone());
            let a = TenantId::parse("a").unwrap();
            let b = TenantId::parse("b").unwrap();
            registry.create(&a, "A").await.unwrap();
            registry.create(&b, "B").await.unwrap();
            let tenants = TenantPool::new(bound);
            let store_a = TenantSettingsStore::new(tenants.clone(), a.clone(), test_master_key());
            let store_b = TenantSettingsStore::new(tenants.clone(), b.clone(), test_master_key());
            let mut settings = TenantSettings::from_edge_toml(&full_config());
            settings.id = a.clone();
            assert!(store_a.seed_if_empty(&settings).await.unwrap());

            assert!(store_a.load().await.unwrap().is_some());
            assert!(store_b.load().await.unwrap().is_none());
            let as_b = tenants.conn(&b).await.unwrap();
            let n: i64 = as_b
                .query_one("SELECT count(*) FROM tenant_settings", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(n, 0, "no predicate, no rows");
            let forged = as_b
                .execute(
                    "UPDATE tenant_settings SET llm_api_key = NULL WHERE tenant_id = 'a'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(
                forged, 0,
                "another tenant's row is not even visible to update"
            );
            as_b.commit().await.unwrap();
            assert!(store_a.load().await.unwrap().unwrap().has_llm_key());

            let moved = superuser
                .get()
                .await
                .unwrap()
                .query_one(
                    "SELECT llm_api_key FROM tenant_settings WHERE tenant_id = 'a'",
                    &[],
                )
                .await
                .unwrap()
                .get::<_, Vec<u8>>(0);
            let mut b_settings = settings.clone();
            b_settings.id = b.clone();
            b_settings.llm.api_key = None;
            store_b.seed_if_empty(&b_settings).await.unwrap();
            superuser
                .get()
                .await
                .unwrap()
                .execute(
                    "UPDATE tenant_settings SET llm_api_key = $1 WHERE tenant_id = 'b'",
                    &[&moved],
                )
                .await
                .unwrap();
            let err = store_b.load().await.unwrap_err();
            assert!(
                format!("{err:#}").contains("opening llm_api_key for tenant b"),
                "a key sealed for one tenant does not open for another: {err:#}"
            );
        })
        .await;
    }

    #[test]
    fn an_admin_section_serialises_without_its_secrets() {
        let admin = AdminConfig {
            oidc: Some(AdminOidc {
                issuer: "https://kc.example/realms/ops".into(),
                audience: None,
                client_id: Some("edge-admin".into()),
                client_secret: Some("oidc-secret".into()),
                mode: None,
            }),
            password_hash: Some("$argon2id$hash".into()),
            allowed_emails: vec!["ops@example.com".into()],
            allowed_domains: vec![],
            session_secret: Some("0123456789abcdef0123456789abcdef".into()),
            public_url: None,
            session_hours: 12,
        };
        let json = serde_json::to_string(&admin).unwrap();
        assert!(!json.contains("oidc-secret"));
        assert!(!json.contains("argon2id"));
        assert!(!json.contains("0123456789abcdef"));
        assert!(json.contains("edge-admin"));
        let back: AdminConfig = serde_json::from_str(&json).unwrap();
        assert!(back.oidc.unwrap().client_secret.is_none());
        assert!(back.password_hash.is_none());
    }
}
