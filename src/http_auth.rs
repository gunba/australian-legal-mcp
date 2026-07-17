use anyhow::{anyhow, bail, Context, Result};
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

const AUTH_MODE_ENV: &str = "LEGAL_MCP_HTTP_AUTH";
const TENANT_ID_ENV: &str = "LEGAL_MCP_ENTRA_TENANT_ID";
const SERVER_APP_ID_ENV: &str = "LEGAL_MCP_ENTRA_SERVER_APP_ID";
const AUDIENCES_ENV: &str = "LEGAL_MCP_ENTRA_AUDIENCES";
const SCOPE_ENV: &str = "LEGAL_MCP_ENTRA_SCOPE";
const SCOPE_URI_ENV: &str = "LEGAL_MCP_ENTRA_SCOPE_URI";
const ALLOWED_CLIENT_IDS_ENV: &str = "LEGAL_MCP_ENTRA_ALLOWED_CLIENT_IDS";
const EXTERNAL_URL_ENV: &str = "LEGAL_MCP_EXTERNAL_URL";
const API_KEYS_FILE_ENV: &str = "LEGAL_MCP_API_KEYS_FILE";
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_API_KEY_FILE_BYTES: u64 = 64 * 1024;
const MAX_API_KEYS: usize = 32;
const MAX_JWKS_BYTES: u64 = 1024 * 1024;
const JWKS_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
const JWKS_MAXIMUM_STALE: Duration = Duration::from_secs(24 * 60 * 60);
const JWKS_MINIMUM_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Default)]
pub(crate) struct HttpAuth {
    entra: Option<Arc<EntraAuthenticator>>,
    api_keys: Option<Arc<ApiKeyAuthenticator>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "auth_method", rename_all = "kebab-case")]
pub(crate) enum AuthPrincipal {
    Entra {
        tenant_id: String,
        object_id: String,
        client_id: String,
    },
    ApiKey {
        key_id: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthFailureKind {
    Missing,
    Invalid,
    InsufficientScope,
    ForbiddenClient,
}

#[derive(Debug)]
pub(crate) struct AuthFailure {
    pub(crate) kind: AuthFailureKind,
    pub(crate) reason: &'static str,
}

impl AuthFailure {
    fn invalid(reason: &'static str) -> Self {
        Self {
            kind: AuthFailureKind::Invalid,
            reason,
        }
    }
}

#[derive(Clone, Debug)]
struct EntraConfig {
    tenant_id: String,
    audiences: BTreeSet<String>,
    scope: String,
    scope_uri: String,
    allowed_client_ids: BTreeSet<String>,
    external_url: String,
    issuer: String,
    authorization_server: String,
    jwks_url: String,
    metadata_url: String,
}

struct JwksCache {
    set: Option<JwkSet>,
    fetched_at: Option<Instant>,
    next_refresh_at: Instant,
}

pub(crate) struct EntraAuthenticator {
    config: EntraConfig,
    client: Client,
    jwks: Mutex<JwksCache>,
    jwks_refresh: Mutex<()>,
}

struct ApiKeyAuthenticator {
    keys: BTreeMap<String, [u8; 32]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiKeyFile {
    version: u32,
    keys: Vec<ApiKeyRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiKeyRecord {
    id: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct EntraClaims {
    exp: u64,
    iat: u64,
    #[serde(default)]
    nbf: Option<u64>,
    sub: String,
    tid: String,
    oid: String,
    azp: String,
    scp: String,
    ver: String,
}

impl HttpAuth {
    pub(crate) fn from_env() -> Result<Self> {
        let mode = match std::env::var(AUTH_MODE_ENV) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => "disabled".to_string(),
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("{AUTH_MODE_ENV} must contain valid Unicode")
            }
        };
        match mode.as_str() {
            "disabled" => Ok(Self::default()),
            "api-key" => Ok(Self {
                entra: None,
                api_keys: Some(Arc::new(ApiKeyAuthenticator::from_env()?)),
            }),
            "entra" => Ok(Self {
                entra: Some(Arc::new(EntraAuthenticator::from_lookup(|name| {
                    std::env::var(name).ok()
                })?)),
                api_keys: None,
            }),
            "entra+api-key" => Ok(Self {
                entra: Some(Arc::new(EntraAuthenticator::from_lookup(|name| {
                    std::env::var(name).ok()
                })?)),
                api_keys: Some(Arc::new(ApiKeyAuthenticator::from_env()?)),
            }),
            _ => bail!(
                "{AUTH_MODE_ENV} must be exactly `disabled`, `api-key`, `entra`, or `entra+api-key`"
            ),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.entra.is_some() || self.api_keys.is_some()
    }

    pub(crate) fn prewarm(&self) -> Result<()> {
        let Some(authenticator) = self.entra.as_deref() else {
            return Ok(());
        };
        let set = authenticator.fetch_jwks()?;
        let mut cache = authenticator
            .jwks
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cache.set = Some(set);
        cache.fetched_at = Some(Instant::now());
        cache.next_refresh_at = Instant::now() + JWKS_MINIMUM_RETRY_INTERVAL;
        Ok(())
    }

    pub(crate) fn authorize(
        &self,
        authorization: Option<&str>,
        api_key: Option<&str>,
    ) -> std::result::Result<Option<AuthPrincipal>, AuthFailure> {
        if !self.is_enabled() {
            return Ok(None);
        }
        match (authorization, api_key) {
            (Some(_), Some(_)) => Err(AuthFailure::invalid("multiple-credentials")),
            (Some(value), None) => self
                .entra
                .as_deref()
                .ok_or_else(|| AuthFailure::invalid("bearer-authentication-disabled"))?
                .authorize(Some(value))
                .map(Some),
            (None, Some(value)) => self
                .api_keys
                .as_deref()
                .ok_or_else(|| AuthFailure::invalid("api-key-authentication-disabled"))?
                .authorize(value)
                .map(Some),
            (None, None) => Err(AuthFailure {
                kind: AuthFailureKind::Missing,
                reason: "credentials-missing",
            }),
        }
    }

    pub(crate) fn protected_resource_metadata(&self) -> Option<JsonValue> {
        self.entra
            .as_deref()
            .map(EntraAuthenticator::protected_resource_metadata)
    }

    pub(crate) fn metadata_path(&self) -> Option<&'static str> {
        self.entra
            .as_ref()
            .map(|_| "/.well-known/oauth-protected-resource/mcp")
    }

    pub(crate) fn challenge(&self, kind: AuthFailureKind) -> Option<String> {
        let mut challenges = Vec::new();
        if let Some(authenticator) = self.entra.as_deref() {
            let mut challenge = format!(
                "Bearer resource_metadata=\"{}\", scope=\"{}\"",
                authenticator.config.metadata_url, authenticator.config.scope_uri
            );
            match kind {
                AuthFailureKind::Missing | AuthFailureKind::ForbiddenClient => {}
                AuthFailureKind::Invalid => challenge.push_str(", error=\"invalid_token\""),
                AuthFailureKind::InsufficientScope => {
                    challenge.push_str(", error=\"insufficient_scope\"")
                }
            }
            challenges.push(challenge);
        }
        if self.api_keys.is_some()
            && matches!(kind, AuthFailureKind::Missing | AuthFailureKind::Invalid)
        {
            challenges.push("ApiKey realm=\"Australian Legal MCP\"".to_string());
        }
        (!challenges.is_empty()).then(|| challenges.join(", "))
    }
}

impl ApiKeyAuthenticator {
    fn from_env() -> Result<Self> {
        let path = std::env::var(API_KEYS_FILE_ENV)
            .map_err(|_| anyhow!("{API_KEYS_FILE_ENV} is required for API-key auth"))?;
        if path.is_empty() || path.trim() != path {
            bail!("{API_KEYS_FILE_ENV} must be nonempty and contain no surrounding whitespace");
        }
        Self::from_path(Path::new(&path))
    }

    fn from_path(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            bail!("{API_KEYS_FILE_ENV} must be an absolute path");
        }
        let path_metadata = fs::symlink_metadata(path)
            .with_context(|| format!("reading API-key verifier path {}", path.display()))?;
        if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
            bail!("API-key verifier path must be a regular non-symlink file");
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let file = options
            .open(path)
            .with_context(|| format!("opening API-key verifier file {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("reading API-key verifier metadata {}", path.display()))?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_API_KEY_FILE_BYTES {
            bail!("API-key verifier file must be a bounded nonempty regular file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                bail!("API-key verifier file must have exactly one link");
            }
            if metadata.mode() & 0o077 != 0 {
                bail!("API-key verifier file must not be accessible by group or other users");
            }
            if metadata.uid() != unsafe { libc::geteuid() } {
                bail!("API-key verifier file must be owned by the service user");
            }
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_API_KEY_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("reading API-key verifier file")?;
        if bytes.len() as u64 > MAX_API_KEY_FILE_BYTES {
            bail!("API-key verifier file exceeds its size limit");
        }
        let parsed: ApiKeyFile = serde_json::from_slice(&bytes)
            .context("parsing API-key verifier file as strict JSON")?;
        if parsed.version != 1 {
            bail!("API-key verifier file version must be exactly 1");
        }
        if parsed.keys.is_empty() || parsed.keys.len() > MAX_API_KEYS {
            bail!("API-key verifier file must contain between 1 and {MAX_API_KEYS} keys");
        }
        let mut keys = BTreeMap::new();
        let mut digests = BTreeSet::new();
        for record in parsed.keys {
            validate_api_key_id(&record.id)?;
            let digest = parse_sha256(&record.sha256)?;
            if !digests.insert(digest) {
                bail!("API-key verifier file contains a duplicate digest");
            }
            if keys.insert(record.id, digest).is_some() {
                bail!("API-key verifier file contains a duplicate key ID");
            }
        }
        Ok(Self { keys })
    }

    fn authorize(&self, value: &str) -> std::result::Result<AuthPrincipal, AuthFailure> {
        if value.len() > 128
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(AuthFailure::invalid("api-key-malformed"));
        }
        let Some((key_id, secret)) = value.split_once('.') else {
            return Err(AuthFailure::invalid("api-key-malformed"));
        };
        if validate_api_key_id(key_id).is_err()
            || secret.len() != 43
            || !secret
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(AuthFailure::invalid("api-key-malformed"));
        }
        let actual: [u8; 32] = Sha256::digest(value.as_bytes()).into();
        let mut authorized = false;
        for (candidate_id, expected) in &self.keys {
            let id_matches = candidate_id == key_id;
            authorized |= id_matches & constant_time_equal(expected, &actual);
        }
        if !authorized {
            return Err(AuthFailure::invalid("api-key-invalid"));
        }
        Ok(AuthPrincipal::ApiKey {
            key_id: key_id.to_string(),
        })
    }
}

fn validate_api_key_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        bail!("API-key ID must be 1-64 lowercase ASCII identifier characters");
    }
    Ok(())
}

fn parse_sha256(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("API-key verifier digest must be canonical lowercase SHA-256");
    }
    let mut digest = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).expect("validated digest is ASCII");
        digest[index] = u8::from_str_radix(text, 16).expect("validated digest is hexadecimal");
    }
    Ok(digest)
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

impl EntraAuthenticator {
    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Result<Self> {
        let required = |name: &str, lookup: &mut dyn FnMut(&str) -> Option<String>| {
            let value = lookup(name).ok_or_else(|| anyhow!("{name} is required for Entra auth"))?;
            if value.is_empty() || value.trim() != value {
                bail!("{name} must be nonempty and contain no surrounding whitespace");
            }
            Ok(value)
        };

        let tenant_id = required(TENANT_ID_ENV, &mut lookup)?;
        validate_uuid(&tenant_id, TENANT_ID_ENV)?;
        let server_app_id = required(SERVER_APP_ID_ENV, &mut lookup)?;
        validate_uuid(&server_app_id, SERVER_APP_ID_ENV)?;
        let audiences = parse_list(&required(AUDIENCES_ENV, &mut lookup)?, AUDIENCES_ENV, false)?;
        let scope = required(SCOPE_ENV, &mut lookup)?;
        validate_scope_name(&scope, SCOPE_ENV)?;
        let scope_uri = required(SCOPE_URI_ENV, &mut lookup)?;
        let resource_app_id = validate_scope_uri(&scope_uri)?;
        if scope_uri.rsplit_once('/').map(|(_, value)| value) != Some(scope.as_str()) {
            bail!("{SCOPE_URI_ENV} must end with the exact {SCOPE_ENV} value");
        }
        if resource_app_id != server_app_id {
            bail!("{SCOPE_URI_ENV} must identify the configured {SERVER_APP_ID_ENV}");
        }
        if !audiences.contains(&server_app_id)
            && !audiences.contains(&format!("api://{server_app_id}"))
        {
            bail!("{AUDIENCES_ENV} must include the configured server application ID");
        }
        let allowed_client_ids = parse_list(
            &required(ALLOWED_CLIENT_IDS_ENV, &mut lookup)?,
            ALLOWED_CLIENT_IDS_ENV,
            true,
        )?;
        for client_id in &allowed_client_ids {
            validate_uuid(client_id, ALLOWED_CLIENT_IDS_ENV)?;
        }
        let external_url = required(EXTERNAL_URL_ENV, &mut lookup)?;
        let external = validate_external_url(&external_url)?;
        let authority = format!("https://login.microsoftonline.com/{tenant_id}");
        let issuer = format!("{authority}/v2.0");
        let jwks_url = format!("{authority}/discovery/v2.0/keys");
        let authority_part = external
            .host_str()
            .ok_or_else(|| anyhow!("{EXTERNAL_URL_ENV} must contain a host"))?;
        let authority_part = match external.port() {
            Some(port) => format!("{authority_part}:{port}"),
            None => authority_part.to_string(),
        };
        let metadata_url =
            format!("https://{authority_part}/.well-known/oauth-protected-resource/mcp");
        let config = EntraConfig {
            tenant_id,
            audiences,
            scope,
            scope_uri,
            allowed_client_ids,
            external_url,
            issuer,
            authorization_server: issuer_from_authority(&authority),
            jwks_url,
            metadata_url,
        };
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("australian-legal-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building Entra JWKS client")?;
        Ok(Self {
            config,
            client,
            jwks: Mutex::new(JwksCache {
                set: None,
                fetched_at: None,
                next_refresh_at: Instant::now(),
            }),
            jwks_refresh: Mutex::new(()),
        })
    }

    fn authorize(
        &self,
        authorization: Option<&str>,
    ) -> std::result::Result<AuthPrincipal, AuthFailure> {
        let token = bearer_token(authorization)?;
        let header = decode_header(token).map_err(|_| AuthFailure::invalid("malformed-jwt"))?;
        if header.alg != Algorithm::RS256 {
            return Err(AuthFailure::invalid("unsupported-jwt-algorithm"));
        }
        let kid = header
            .kid
            .as_deref()
            .filter(|kid| !kid.is_empty() && kid.len() <= 256 && kid.is_ascii())
            .ok_or_else(|| AuthFailure::invalid("missing-jwt-key-id"))?;
        let jwk = self
            .jwk_for(kid)
            .map_err(|_| AuthFailure::invalid("jwks-unavailable"))?
            .ok_or_else(|| AuthFailure::invalid("unknown-jwt-key-id"))?;
        if jwk
            .common
            .key_algorithm
            .is_some_and(|algorithm| algorithm != KeyAlgorithm::RS256)
            || jwk
                .common
                .public_key_use
                .as_ref()
                .is_some_and(|usage| usage != &PublicKeyUse::Signature)
        {
            return Err(AuthFailure::invalid("ineligible-jwk"));
        }
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| AuthFailure::invalid("invalid-jwk"))?;
        self.validate_signed_token(token, &key)
    }

    fn validate_signed_token(
        &self,
        token: &str,
        key: &DecodingKey,
    ) -> std::result::Result<AuthPrincipal, AuthFailure> {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&self.config.audiences.iter().collect::<Vec<_>>());
        validation.set_issuer(&[self.config.issuer.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 60;
        validation.reject_tokens_expiring_in_less_than = 10;
        let token = decode::<EntraClaims>(token, key, &validation)
            .map_err(|_| AuthFailure::invalid("jwt-validation-failed"))?;
        let claims = token.claims;
        if claims.ver != "2.0"
            || claims.tid != self.config.tenant_id
            || validate_uuid(&claims.oid, "token oid").is_err()
            || claims.sub.is_empty()
            || claims.exp == 0
            || claims.nbf.is_some_and(|nbf| nbf > claims.exp)
        {
            return Err(AuthFailure::invalid("invalid-entra-claims"));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthFailure::invalid("system-clock-invalid"))?
            .as_secs();
        if claims.iat > now.saturating_add(60) {
            return Err(AuthFailure::invalid("token-issued-in-future"));
        }
        if !claims
            .scp
            .split_ascii_whitespace()
            .any(|scope| scope == self.config.scope)
        {
            return Err(AuthFailure {
                kind: AuthFailureKind::InsufficientScope,
                reason: "required-scope-missing",
            });
        }
        if !self.config.allowed_client_ids.contains(&claims.azp) {
            return Err(AuthFailure {
                kind: AuthFailureKind::ForbiddenClient,
                reason: "client-not-allowed",
            });
        }
        Ok(AuthPrincipal::Entra {
            tenant_id: claims.tid,
            object_id: claims.oid,
            client_id: claims.azp,
        })
    }

    fn jwk_for(&self, kid: &str) -> Result<Option<Jwk>> {
        let now = Instant::now();
        let (cached, should_refresh) = {
            let cache = self.jwks.lock().unwrap_or_else(|error| error.into_inner());
            let cache_usable = jwks_cache_usable(cache.fetched_at, now);
            let cached = cache_usable
                .then(|| cache.set.as_ref().and_then(|set| set.find(kid)).cloned())
                .flatten();
            let expired = cache
                .fetched_at
                .is_none_or(|fetched| now.duration_since(fetched) >= JWKS_REFRESH_AFTER);
            (
                cached.clone(),
                now >= cache.next_refresh_at && (cached.is_none() || expired),
            )
        };
        if !should_refresh {
            return Ok(cached);
        }
        let _refresh = match self.jwks_refresh.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return Ok(cached),
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let now = Instant::now();
        {
            let mut cache = self.jwks.lock().unwrap_or_else(|error| error.into_inner());
            let cache_usable = jwks_cache_usable(cache.fetched_at, now);
            let current = cache_usable
                .then(|| cache.set.as_ref().and_then(|set| set.find(kid)).cloned())
                .flatten();
            let expired = cache
                .fetched_at
                .is_none_or(|fetched| now.duration_since(fetched) >= JWKS_REFRESH_AFTER);
            if now < cache.next_refresh_at || (current.is_some() && !expired) {
                return Ok(current);
            }
            cache.next_refresh_at = now + JWKS_MINIMUM_RETRY_INTERVAL;
        }
        match self.fetch_jwks() {
            Ok(set) => {
                let result = set.find(kid).cloned();
                let mut cache = self.jwks.lock().unwrap_or_else(|error| error.into_inner());
                cache.set = Some(set);
                cache.fetched_at = Some(Instant::now());
                Ok(result)
            }
            Err(_) if cached.is_some() => Ok(cached),
            Err(error) => Err(error),
        }
    }

    fn fetch_jwks(&self) -> Result<JwkSet> {
        let response = self
            .client
            .get(&self.config.jwks_url)
            .header("accept", "application/json")
            .send()
            .context("fetching Entra signing keys")?;
        if !response.status().is_success() {
            bail!("Entra signing-key endpoint returned {}", response.status());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_JWKS_BYTES)
        {
            bail!("Entra signing-key response exceeds size limit");
        }
        let bytes = response.bytes().context("reading Entra signing keys")?;
        if bytes.len() as u64 > MAX_JWKS_BYTES {
            bail!("Entra signing-key response exceeds size limit");
        }
        let set: JwkSet = serde_json::from_slice(&bytes).context("parsing Entra signing keys")?;
        if set.keys.is_empty() || set.keys.len() > 100 {
            bail!("Entra signing-key set has an invalid key count");
        }
        let mut kids = BTreeSet::new();
        for key in &set.keys {
            let kid = key
                .common
                .key_id
                .as_deref()
                .ok_or_else(|| anyhow!("Entra signing key is missing kid"))?;
            if kid.is_empty() || kid.len() > 256 || !kid.is_ascii() || !kids.insert(kid) {
                bail!("Entra signing-key set contains an invalid or duplicate kid");
            }
        }
        Ok(set)
    }

    fn protected_resource_metadata(&self) -> JsonValue {
        json!({
            "resource": self.config.external_url,
            "authorization_servers": [self.config.authorization_server],
            "scopes_supported": [self.config.scope_uri],
            "bearer_methods_supported": ["header"],
            "resource_name": "Australian Legal MCP"
        })
    }
}

fn jwks_cache_usable(fetched_at: Option<Instant>, now: Instant) -> bool {
    fetched_at.is_some_and(|fetched| now.saturating_duration_since(fetched) <= JWKS_MAXIMUM_STALE)
}

fn issuer_from_authority(authority: &str) -> String {
    format!("{authority}/v2.0")
}

fn bearer_token(authorization: Option<&str>) -> std::result::Result<&str, AuthFailure> {
    let Some(value) = authorization else {
        return Err(AuthFailure {
            kind: AuthFailureKind::Missing,
            reason: "authorization-missing",
        });
    };
    if value.len() > MAX_TOKEN_BYTES {
        return Err(AuthFailure::invalid("authorization-too-large"));
    }
    let Some((scheme, token)) = value.split_once(' ') else {
        return Err(AuthFailure::invalid("authorization-malformed"));
    };
    if !scheme.eq_ignore_ascii_case("Bearer")
        || token.is_empty()
        || token
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(AuthFailure::invalid("authorization-malformed"));
    }
    Ok(token)
}

fn parse_list(value: &str, name: &str, require_uuid: bool) -> Result<BTreeSet<String>> {
    let mut parsed = BTreeSet::new();
    for item in value.split(',') {
        if item.is_empty()
            || item.trim() != item
            || item.len() > 512
            || !item.is_ascii()
            || item.bytes().any(|byte| {
                byte.is_ascii_whitespace()
                    || byte.is_ascii_control()
                    || matches!(byte, b'"' | b'\\')
            })
        {
            bail!("{name} contains an invalid value");
        }
        if require_uuid {
            validate_uuid(item, name)?;
        }
        if !parsed.insert(item.to_string()) {
            bail!("{name} contains a duplicate value");
        }
    }
    if parsed.is_empty() {
        bail!("{name} must contain at least one value");
    }
    Ok(parsed)
}

fn validate_uuid(value: &str, name: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || [8, 13, 18, 23].iter().any(|index| bytes[*index] != b'-')
        || bytes.iter().enumerate().any(|(index, byte)| {
            !([8, 13, 18, 23].contains(&index)
                || byte.is_ascii_digit()
                || (b'a'..=b'f').contains(byte))
        })
    {
        bail!("{name} must contain canonical lowercase UUID values");
    }
    Ok(())
}

fn validate_scope_name(value: &str, name: &str) -> Result<()> {
    if value.len() > 128
        || !value.is_ascii()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("{name} must be an ASCII scope name without a resource prefix");
    }
    Ok(())
}

fn validate_scope_uri(value: &str) -> Result<String> {
    if value.len() > 512
        || !value.is_ascii()
        || value.bytes().any(|byte| {
            byte.is_ascii_whitespace() || byte.is_ascii_control() || matches!(byte, b'"' | b'\\')
        })
        || !value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        bail!("{SCOPE_URI_ENV} is malformed");
    }
    let Some((resource, scope)) = value.rsplit_once('/') else {
        bail!("{SCOPE_URI_ENV} must contain the full resource and delegated scope");
    };
    let Some(resource_app_id) = resource.strip_prefix("api://") else {
        bail!("{SCOPE_URI_ENV} must use an api:// application ID URI");
    };
    if scope.is_empty() {
        bail!("{SCOPE_URI_ENV} must contain a delegated scope");
    }
    validate_uuid(resource_app_id, SCOPE_URI_ENV)?;
    Ok(resource_app_id.to_string())
}

fn validate_external_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("parsing {EXTERNAL_URL_ENV}"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.path() != "/mcp"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.as_str() != value
    {
        bail!("{EXTERNAL_URL_ENV} must be a canonical HTTPS URL ending exactly in /mcp");
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;
    use std::collections::BTreeMap;

    const PRIVATE_KEY: &[u8] = br#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDUfL7yZaWJLVpn
ZekPKQ9A+wfF9G+Vw8jFVmWZkbN8Wwotvq1cSvMEty5pA6MPkT2Ckpe9l+m1UeKd
EYRDTkpeMm4JF2d7Go5rVR9iwONIlK9ArCnw97RSsrVvnwFBPKclr656bMr3zs6p
2k4SUT6nyqUeBHvILrE2pUImdVIIQOIpeK9Ak+WEbMWhIsfsA9mvlgytbPlvY+Ca
JCYf3fIskt0unqpNXNsgxb4RnG8AKO3Z3rjy2XEeZ3GYZoMfhlKbRjPXF6h8+hol
+g6AZX3Ol/MslP+THN9sCFvHpBhX/yZyfmERONdi0oW9yRRNPmELynRcxua7zo8R
x33miTlFAgMBAAECggEAYGlZmlJUkbFqW4/5908CBNHh7QfDuYhkCGXzI4LglYQZ
Ujg6IK5BJdqXzD5CNkYISr6I6xWKjSLiV7Ii+QcE50iqdKWR5mFeTYUKAJzUg5Iz
En0LarJ5tywu9r6GqzsB/C+CUzoZveawDpFm6xjB/RANa1lNcL7+2XSSVzDUT7mM
dKG7/MeQJ0cDBuM/CNfmmh77nmFbHJlWtAoLmCa1PDKKXCi37uV5LbystgpSH3wJ
gzIexhicwsH1KW/1nxXvbPsy1ZneBQBQuA6WWgxYrxzmGPWgTkv26yzZGmb9x5z7
AotxK+eEIon0Q7mZiF1UmObHSvm1wwaF6yFDRf4T4QKBgQD+Opnh7iwwibi1d8zL
aI/7o8tKWNpIDTyg2YXSbNfQgnIBa9R1wfOPID9zQwN4BSR8lLQM7YpDQlz2ewel
I64iBEjs78M0qFZDUyRRFDkxPor9WHpia+KPbd1jme6tcfNPLj9FGJgpmS8FBYZ6
R+Q2Qw/NjQW6QqUGDi7J7rORCwKBgQDV97OdDg2Ckw6cZ4r3qD1k0AgBpamLukWp
kENrBzxJuTaieE/aHjOTU5qFDMxGB+mS0Vw+Tfmu94g+RVdv3U2hcnbWxZxTDpOJ
M2aGLl73RLmCsRWk5u7IdWxK+FTbPfkVvP5n3+V7fiusrjke7at2jzD5KxI7CJZY
lRASvlBw7wKBgEyz54u37VMzqivuGjbgtFhK4eHrjuggPkOVfX+wYSjCwpzVKMPi
oZZ0N1CSTnCetJR11SD1ZjrGwf+HvRXA/x+6RTpfWHkBtQ0Y/6MKw/qskQjA8iPR
wwhdMGeFoPJpp+wi2uoA8p/SXNJaCWnJWPxaHWF6A9lflCSQkONSBpFjAoGBAMYv
dBex74OVcMAgDBEOrScWpYPZDSzWMTY44Kle/1GOE3Pzmor+1GjO1F4Ol5r6MzgB
Yb53/SA6OODs22tLAV/cJQUT7pLj+nXnvTvl8aJ47peGLPUbzeqxEMh0Mi0Mvw2K
i95s/SMgn5WHnnLuU5YyVXtFkNJLRu7vyv6BdwLTAoGBALvLDMhSC6qbofBJMmFG
/JWTrZQ6oUHt7KT6C8OuC34VgIC5qzW+vsjJ53532lFAd4WzY4G1yX6KM0dIonLQ
HOGlJzD0L7KUg6hViyYLq3dksEgdJzbSpA3s47lBOnMWxi0Yi9ErBLoKU+QbPra+
T2P4BECW5ZXnRgrcGpHb6JF+
-----END PRIVATE KEY-----"#;
    const PUBLIC_KEY: &[u8] = br#"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA1Hy+8mWliS1aZ2XpDykP
QPsHxfRvlcPIxVZlmZGzfFsKLb6tXErzBLcuaQOjD5E9gpKXvZfptVHinRGEQ05K
XjJuCRdnexqOa1UfYsDjSJSvQKwp8Pe0UrK1b58BQTynJa+uemzK987OqdpOElE+
p8qlHgR7yC6xNqVCJnVSCEDiKXivQJPlhGzFoSLH7APZr5YMrWz5b2PgmiQmH93y
LJLdLp6qTVzbIMW+EZxvACjt2d648tlxHmdxmGaDH4ZSm0Yz1xeofPoaJfoOgGV9
zpfzLJT/kxzfbAhbx6QYV/8mcn5hETjXYtKFvckUTT5hC8p0XMbmu86PEcd95ok5
RQIDAQAB
-----END PUBLIC KEY-----"#;

    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const CLIENT: &str = "22222222-2222-2222-2222-222222222222";
    const AUDIENCE: &str = "33333333-3333-3333-3333-333333333333";

    fn authenticator() -> EntraAuthenticator {
        let values = BTreeMap::from([
            (TENANT_ID_ENV, TENANT),
            (SERVER_APP_ID_ENV, AUDIENCE),
            (AUDIENCES_ENV, AUDIENCE),
            (SCOPE_ENV, "legal.read"),
            (
                SCOPE_URI_ENV,
                "api://33333333-3333-3333-3333-333333333333/legal.read",
            ),
            (ALLOWED_CLIENT_IDS_ENV, CLIENT),
            (EXTERNAL_URL_ENV, "https://legal.example/mcp"),
        ]);
        EntraAuthenticator::from_lookup(|name| values.get(name).map(|value| (*value).to_string()))
            .expect("valid test Entra configuration")
    }

    #[derive(Serialize)]
    struct TestClaims<'a> {
        exp: u64,
        iat: u64,
        nbf: u64,
        iss: String,
        aud: &'a str,
        sub: &'a str,
        tid: &'a str,
        oid: &'a str,
        azp: &'a str,
        scp: &'a str,
        ver: &'a str,
    }

    fn signed_token(scope: &str, client: &str, audience: &str) -> String {
        signed_token_with(
            scope,
            client,
            audience,
            &format!("https://login.microsoftonline.com/{TENANT}/v2.0"),
            TENANT,
            3600,
        )
    }

    fn signed_token_with(
        scope: &str,
        client: &str,
        audience: &str,
        issuer: &str,
        tenant: &str,
        expires_in: i64,
    ) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let claims = TestClaims {
            exp: now.saturating_add_signed(expires_in),
            iat: now,
            nbf: now.saturating_sub(1),
            iss: issuer.to_string(),
            aud: audience,
            sub: "subject",
            tid: tenant,
            oid: "44444444-4444-4444-4444-444444444444",
            azp: client,
            scp: scope,
            ver: "2.0",
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(PRIVATE_KEY).expect("private key"),
        )
        .expect("signed JWT")
    }

    #[test]
    fn entra_configuration_is_strict_and_metadata_is_canonical() {
        let auth = authenticator();
        assert_eq!(
            auth.protected_resource_metadata(),
            json!({
                "resource": "https://legal.example/mcp",
                "authorization_servers": [format!("https://login.microsoftonline.com/{TENANT}/v2.0")],
                "scopes_supported": ["api://33333333-3333-3333-3333-333333333333/legal.read"],
                "bearer_methods_supported": ["header"],
                "resource_name": "Australian Legal MCP"
            })
        );
        assert!(validate_external_url("http://legal.example/mcp").is_err());
        assert!(validate_external_url("https://legal.example/mcp/").is_err());
        assert!(validate_uuid("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA", TENANT_ID_ENV).is_err());
    }

    #[test]
    fn bearer_parser_rejects_ambiguous_or_oversized_values() {
        assert_eq!(bearer_token(Some("Bearer abc")).unwrap(), "abc");
        assert_eq!(
            bearer_token(None).unwrap_err().kind,
            AuthFailureKind::Missing
        );
        for value in ["abc", "Basic abc", "Bearer", "Bearer a b", "Bearer a\tb"] {
            assert_eq!(
                bearer_token(Some(value)).unwrap_err().kind,
                AuthFailureKind::Invalid
            );
        }
    }

    #[test]
    fn signed_entra_token_requires_exact_audience_scope_and_client() {
        let auth = authenticator();
        let key = DecodingKey::from_rsa_pem(PUBLIC_KEY).expect("public key");
        let principal = auth
            .validate_signed_token(&signed_token("other legal.read", CLIENT, AUDIENCE), &key)
            .expect("valid token");
        assert!(matches!(
            principal,
            AuthPrincipal::Entra {
                ref tenant_id,
                ref client_id,
                ..
            } if tenant_id == TENANT && client_id == CLIENT
        ));

        assert_eq!(
            auth.validate_signed_token(&signed_token("other", CLIENT, AUDIENCE), &key)
                .unwrap_err()
                .kind,
            AuthFailureKind::InsufficientScope
        );
        assert_eq!(
            auth.validate_signed_token(
                &signed_token(
                    "legal.read",
                    "55555555-5555-5555-5555-555555555555",
                    AUDIENCE,
                ),
                &key,
            )
            .unwrap_err()
            .kind,
            AuthFailureKind::ForbiddenClient
        );
        assert_eq!(
            auth.validate_signed_token(&signed_token("legal.read", CLIENT, "wrong"), &key)
                .unwrap_err()
                .kind,
            AuthFailureKind::Invalid
        );
        for token in [
            signed_token_with(
                "legal.read",
                CLIENT,
                AUDIENCE,
                "https://login.microsoftonline.com/55555555-5555-5555-5555-555555555555/v2.0",
                TENANT,
                3600,
            ),
            signed_token_with(
                "legal.read",
                CLIENT,
                AUDIENCE,
                &format!("https://login.microsoftonline.com/{TENANT}/v2.0"),
                "55555555-5555-5555-5555-555555555555",
                3600,
            ),
            signed_token_with(
                "legal.read",
                CLIENT,
                AUDIENCE,
                &format!("https://login.microsoftonline.com/{TENANT}/v2.0"),
                TENANT,
                -3600,
            ),
        ] {
            assert_eq!(
                auth.validate_signed_token(&token, &key).unwrap_err().kind,
                AuthFailureKind::Invalid
            );
        }
    }

    #[test]
    fn api_key_file_and_combined_authentication_are_strict() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("api-keys.json");
        let token = format!("automation.{}", "A".repeat(43));
        let digest = format!("{:x}", Sha256::digest(token.as_bytes()));
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version": 1,
                "keys": [{"id": "automation", "sha256": digest}]
            }))
            .expect("serialize verifier file"),
        )
        .expect("write verifier file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400))
                .expect("protect verifier file");
        }
        let api_keys =
            Arc::new(ApiKeyAuthenticator::from_path(&path).expect("valid verifier file"));
        let auth = HttpAuth {
            entra: Some(Arc::new(authenticator())),
            api_keys: Some(api_keys),
        };

        assert_eq!(
            auth.authorize(None, Some(&token)).expect("valid key"),
            Some(AuthPrincipal::ApiKey {
                key_id: "automation".to_string()
            })
        );
        for invalid in [
            "automation.short",
            &format!("automation.{}", "B".repeat(43)),
            &format!("unknown.{}", "A".repeat(43)),
        ] {
            assert_eq!(
                auth.authorize(None, Some(invalid)).unwrap_err().kind,
                AuthFailureKind::Invalid
            );
        }
        assert_eq!(
            auth.authorize(Some("Bearer token"), Some(&token))
                .unwrap_err()
                .reason,
            "multiple-credentials"
        );
        let challenge = auth
            .challenge(AuthFailureKind::Missing)
            .expect("combined challenge");
        assert!(challenge.contains("Bearer resource_metadata="));
        assert!(challenge.contains("ApiKey realm="));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o440))
                .expect("weaken verifier permissions");
            assert!(ApiKeyAuthenticator::from_path(&path).is_err());
        }
    }

    #[test]
    fn jwks_cache_has_a_hard_stale_limit() {
        let now = Instant::now();
        assert!(jwks_cache_usable(Some(now), now));
        let stale = now
            .checked_sub(JWKS_MAXIMUM_STALE + Duration::from_secs(1))
            .expect("monotonic clock has sufficient range");
        assert!(!jwks_cache_usable(Some(stale), now));
        assert!(!jwks_cache_usable(None, now));
    }
}
