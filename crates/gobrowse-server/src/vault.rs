use std::{net::IpAddr, path::Path, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gobrowse_core::sandbox::is_public_destination;
use rand::RngCore;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use secrecy::{ExposeSecret, SecretString};
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use url::Host;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::{config::VaultSettings, error::AppError};

pub(crate) const MCP_OAUTH_ACCESS_TOKEN: &str = "mcp_oauth_access_token";
pub(crate) const MCP_OAUTH_REFRESH_TOKEN: &str = "mcp_oauth_refresh_token";
pub(crate) const MCP_OAUTH_CLIENT_SECRET: &str = "mcp_oauth_client_secret";
pub(crate) const MCP_OAUTH_PKCE_VERIFIER: &str = "mcp_oauth_pkce_verifier";

const MCP_PURPOSES: [&str; 4] = [
    MCP_OAUTH_ACCESS_TOKEN,
    MCP_OAUTH_REFRESH_TOKEN,
    MCP_OAUTH_CLIENT_SECRET,
    MCP_OAUTH_PKCE_VERIFIER,
];

/// Validate and canonicalize the metadata policy for a stored secret.
///
/// Non-MCP purposes retain the existing purpose and host-list behavior. MCP OAuth
/// purposes are deliberately narrower: exactly one authority host is required,
/// and it must be syntactically canonical and a public literal when it is an IP.
pub(crate) fn validate_secret_metadata(
    purpose: &str,
    allowed_hosts: &[String],
) -> Result<Vec<String>, AppError> {
    if purpose.is_empty() || purpose.len() > 100 {
        return Err(AppError::Validation(
            "secret purpose must contain 1 to 100 characters".into(),
        ));
    }
    if purpose.starts_with("mcp_") {
        if !MCP_PURPOSES.contains(&purpose) {
            return Err(AppError::Validation(
                "unsupported MCP secret purpose".into(),
            ));
        }
        if allowed_hosts.len() != 1 {
            return Err(AppError::Validation(
                "MCP OAuth secrets require exactly one allowed host".into(),
            ));
        }
        return canonical_transmission_host(&allowed_hosts[0]).map(|host| vec![host]);
    }
    Ok(allowed_hosts.to_vec())
}

pub(crate) fn validate_legacy_hosts(allowed_hosts: &[String]) -> Result<Vec<String>, AppError> {
    if allowed_hosts.iter().any(|host| {
        host.len() > 253
            || (!host.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '-')
            }) && host.parse::<IpAddr>().is_err())
    }) {
        return Err(AppError::Validation(
            "allowed_hosts must contain at most 20 valid DNS names or IP addresses".into(),
        ));
    }
    Ok(allowed_hosts.to_vec())
}

/// Canonicalize one authority host without resolving DNS.
pub(crate) fn canonical_transmission_host(host: &str) -> Result<String, AppError> {
    if host.trim() != host {
        return Err(AppError::Validation(
            "allowed host must be one canonical authority host".into(),
        ));
    }
    if host.ends_with("..") {
        return Err(AppError::Validation(
            "allowed host must be one canonical authority host".into(),
        ));
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty()
        || host
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '?' | '#'))
    {
        return Err(AppError::Validation(
            "allowed host must be one canonical authority host".into(),
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_destination(ip) {
            return Err(AppError::Validation(
                "allowed host must be a public transmission host".into(),
            ));
        }
        return Ok(ip.to_string());
    }

    // Host::parse performs IDNA canonicalization for DNS names. We still apply
    // strict authority-label checks to reject userinfo, ports, wildcards, and
    // malformed labels before accepting the resulting ASCII form.
    let domain = match Host::parse(host) {
        Ok(Host::Domain(domain)) => domain,
        _ => {
            return Err(AppError::Validation(
                "allowed host must be one canonical authority host".into(),
            ));
        }
    };
    let domain = domain.to_ascii_lowercase();
    if domain.len() > 253
        || domain.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || !label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
                || label.starts_with('-')
                || label.ends_with('-')
        })
    {
        return Err(AppError::Validation(
            "allowed host must be one canonical authority host".into(),
        ));
    }
    Ok(domain)
}
const ALGORITHM: &str = "AES-256-GCM-ENVELOPE-V1";
const NONCE_LENGTH: usize = 12;

#[derive(Clone)]
pub struct Vault {
    cipher: Option<Arc<VaultCipher>>,
    previous_cipher: Option<Arc<VaultCipher>>,
}

struct VaultCipher {
    master_key: Zeroizing<[u8; 32]>,
    key_version: i32,
}

pub struct EncryptedSecret {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub wrapped_data_key: Vec<u8>,
    pub wrap_nonce: Vec<u8>,
    pub key_version: i32,
}

#[derive(Debug, Error)]
enum VaultError {
    #[error("the credential vault is unavailable because no master key is configured")]
    Unavailable,
    #[error("the vault master key must decode to exactly 32 bytes")]
    InvalidKey,
    #[error("vault master-key file permissions must not allow group or other access")]
    InsecurePermissions,
    #[error("credential envelope is invalid")]
    InvalidEnvelope,
    #[error("the key required for this credential version is not configured")]
    UnknownKeyVersion,
    #[error("credential encryption failed")]
    Encryption,
    #[error("credential decryption failed")]
    Decryption,
    #[error("vault master-key file could not be read: {0}")]
    File(#[from] std::io::Error),
}

impl Vault {
    pub async fn from_settings(settings: &VaultSettings) -> Result<Self, AppError> {
        let encoded =
            read_key_source(&settings.master_key_file, &settings.master_key_base64).await?;
        let cipher = encoded
            .map(|encoded| decode_key(encoded.trim(), settings.key_version))
            .transpose()
            .map_err(internal)?
            .map(Arc::new);
        let previous_encoded = read_key_source(
            &settings.previous_master_key_file,
            &settings.previous_master_key_base64,
        )
        .await?;
        let previous_cipher = match (previous_encoded, settings.previous_key_version) {
            (Some(encoded), Some(version)) => Some(Arc::new(
                decode_key(encoded.trim(), version).map_err(internal)?,
            )),
            (None, None) => None,
            _ => {
                return Err(AppError::Validation(
                    "previous vault key source and version must be configured together".into(),
                ));
            }
        };
        Ok(Self {
            cipher,
            previous_cipher,
        })
    }

    pub fn is_available(&self) -> bool {
        self.cipher.is_some()
    }

    pub fn encrypt(
        &self,
        profile_id: Uuid,
        secret_id: &str,
        purpose: &str,
        plaintext: &SecretString,
    ) -> Result<EncryptedSecret, AppError> {
        let current_cipher = self
            .cipher
            .as_deref()
            .ok_or(VaultError::Unavailable)
            .map_err(validation)?;
        current_cipher
            .encrypt(
                profile_id,
                secret_id,
                purpose,
                plaintext.expose_secret().as_bytes(),
            )
            .map_err(internal)
    }

    pub async fn resolve(
        &self,
        pool: &PgPool,
        profile_id: Uuid,
        secret_id: &str,
    ) -> Result<SecretString, AppError> {
        self.resolve_scoped(pool, profile_id, secret_id, None).await
    }

    pub async fn resolve_for_provider(
        &self,
        pool: &PgPool,
        profile_id: Uuid,
        secret_id: &str,
        host: &str,
    ) -> Result<SecretString, AppError> {
        self.resolve_scoped(
            pool,
            profile_id,
            secret_id,
            Some(host.trim_end_matches('.').to_ascii_lowercase()),
        )
        .await
    }

    async fn resolve_scoped(
        &self,
        pool: &PgPool,
        profile_id: Uuid,
        secret_id: &str,
        provider_host: Option<String>,
    ) -> Result<SecretString, AppError> {
        let row = sqlx::query(
            "SELECT purpose,allowed_hosts,algorithm,encrypted_value,nonce,wrapped_data_key,wrap_nonce,key_version \
             FROM secret_references WHERE id = $1 AND profile_id = $2 AND backend = 'encrypted_database'",
        )
        .bind(secret_id)
        .bind(profile_id)
        .fetch_optional(pool)
        .await?
        .ok_or(crate::error::AppError::NotFound)?;
        if let Some(host) = provider_host
            && (row.get::<String, _>("purpose") != "provider_credential"
                || !row.get::<Vec<String>, _>("allowed_hosts").contains(&host))
        {
            return Err(AppError::Forbidden);
        }
        if row.get::<String, _>("algorithm") != ALGORITHM {
            return Err(internal(VaultError::InvalidEnvelope));
        }
        let key_version: i32 = row.get("key_version");
        let cipher = self
            .cipher_for(key_version)
            .ok_or_else(|| internal(VaultError::UnknownKeyVersion))?;
        let plaintext = cipher
            .decrypt(
                profile_id,
                secret_id,
                row.get("purpose"),
                &row.get::<Vec<u8>, _>("encrypted_value"),
                &row.get::<Vec<u8>, _>("nonce"),
                &row.get::<Vec<u8>, _>("wrapped_data_key"),
                &row.get::<Vec<u8>, _>("wrap_nonce"),
            )
            .map_err(internal)?;
        let value = String::from_utf8(plaintext.to_vec())
            .map_err(|_| internal(VaultError::InvalidEnvelope))?;
        Ok(SecretString::from(value))
    }

    pub async fn rotate_profile(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        profile_id: Uuid,
    ) -> Result<u64, AppError> {
        let current = self
            .cipher
            .as_deref()
            .ok_or(VaultError::Unavailable)
            .map_err(validation)?;
        let existing_state: Option<i32> = sqlx::query_scalar(
            "SELECT current_key_version FROM vault_key_state WHERE profile_id=$1 FOR UPDATE",
        )
        .bind(profile_id)
        .fetch_optional(&mut **tx)
        .await?;
        let state_version = if let Some(version) = existing_state {
            version
        } else {
            let stored_version: Option<i32> = sqlx::query_scalar(
                "SELECT min(key_version) FROM secret_references \
                 WHERE profile_id=$1 AND backend='encrypted_database'",
            )
            .bind(profile_id)
            .fetch_one(&mut **tx)
            .await?;
            let version = stored_version.unwrap_or(current.key_version);
            sqlx::query(
                "INSERT INTO vault_key_state (profile_id,current_key_version) VALUES ($1,$2)",
            )
            .bind(profile_id)
            .bind(version)
            .execute(&mut **tx)
            .await?;
            version
        };
        if state_version != current.key_version && self.cipher_for(state_version).is_none() {
            return Err(validation(VaultError::UnknownKeyVersion));
        }
        sqlx::query(
            "UPDATE vault_key_state SET current_key_version=$1,updated_at=now() WHERE profile_id=$2",
        )
        .bind(current.key_version)
        .bind(profile_id)
        .execute(&mut **tx)
        .await?;
        let rows = sqlx::query(
            "SELECT id,purpose,algorithm,encrypted_value,nonce,wrapped_data_key,wrap_nonce,key_version \
             FROM secret_references WHERE profile_id=$1 AND backend='encrypted_database' AND key_version<>$2 \
             ORDER BY id FOR UPDATE",
        )
        .bind(profile_id)
        .bind(current.key_version)
        .fetch_all(&mut **tx)
        .await?;
        let mut rotated = 0_u64;
        for row in rows {
            if row.get::<String, _>("algorithm") != ALGORITHM {
                return Err(internal(VaultError::InvalidEnvelope));
            }
            let id: String = row.get("id");
            let purpose: String = row.get("purpose");
            let old = self
                .cipher_for(row.get("key_version"))
                .ok_or_else(|| validation(VaultError::UnknownKeyVersion))?;
            let plaintext = old
                .decrypt(
                    profile_id,
                    &id,
                    purpose.clone(),
                    &row.get::<Vec<u8>, _>("encrypted_value"),
                    &row.get::<Vec<u8>, _>("nonce"),
                    &row.get::<Vec<u8>, _>("wrapped_data_key"),
                    &row.get::<Vec<u8>, _>("wrap_nonce"),
                )
                .map_err(internal)?;
            let encrypted = current
                .encrypt(profile_id, &id, &purpose, &plaintext)
                .map_err(internal)?;
            sqlx::query(
                "UPDATE secret_references SET encrypted_value=$1,nonce=$2,key_version=$3,algorithm=$4, \
                 wrapped_data_key=$5,wrap_nonce=$6,updated_at=now() WHERE id=$7 AND profile_id=$8",
            )
            .bind(encrypted.ciphertext)
            .bind(encrypted.nonce)
            .bind(encrypted.key_version)
            .bind(ALGORITHM)
            .bind(encrypted.wrapped_data_key)
            .bind(encrypted.wrap_nonce)
            .bind(&id)
            .bind(profile_id)
            .execute(&mut **tx)
            .await?;
            rotated += 1;
        }
        Ok(rotated)
    }

    pub async fn fence_current_key(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        profile_id: Uuid,
    ) -> Result<(), AppError> {
        let current = self
            .cipher
            .as_deref()
            .ok_or(VaultError::Unavailable)
            .map_err(validation)?;
        sqlx::query(
            "INSERT INTO vault_key_state (profile_id,current_key_version) VALUES ($1,$2) \
             ON CONFLICT (profile_id) DO NOTHING",
        )
        .bind(profile_id)
        .bind(current.key_version)
        .execute(&mut **tx)
        .await?;
        let version: i32 = sqlx::query_scalar(
            "SELECT current_key_version FROM vault_key_state WHERE profile_id=$1 FOR UPDATE",
        )
        .bind(profile_id)
        .fetch_one(&mut **tx)
        .await?;
        if version != current.key_version {
            return Err(AppError::Conflict(
                "this instance does not hold the active vault key version",
            ));
        }
        Ok(())
    }

    fn cipher_for(&self, key_version: i32) -> Option<&VaultCipher> {
        self.cipher
            .as_deref()
            .filter(|cipher| cipher.key_version == key_version)
            .or_else(|| {
                self.previous_cipher
                    .as_deref()
                    .filter(|cipher| cipher.key_version == key_version)
            })
    }
}

impl VaultCipher {
    fn encrypt(
        &self,
        profile_id: Uuid,
        secret_id: &str,
        purpose: &str,
        plaintext: &[u8],
    ) -> Result<EncryptedSecret, VaultError> {
        let mut data_key = Zeroizing::new([0_u8; 32]);
        rand::rng().fill_bytes(data_key.as_mut());
        let value_nonce = random_nonce();
        let wrap_nonce = random_nonce();
        let value_cipher = aead_key(data_key.as_ref())?;
        let master_cipher = aead_key(self.master_key.as_ref())?;
        let mut ciphertext = plaintext.to_vec();
        value_cipher
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(value_nonce),
                Aad::from(value_aad(profile_id, secret_id, purpose).as_bytes()),
                &mut ciphertext,
            )
            .map_err(|_| VaultError::Encryption)?;
        let mut wrapped_data_key = data_key.to_vec();
        master_cipher
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(wrap_nonce),
                Aad::from(wrap_aad(profile_id, secret_id, purpose, self.key_version).as_bytes()),
                &mut wrapped_data_key,
            )
            .map_err(|_| VaultError::Encryption)?;
        data_key.zeroize();
        Ok(EncryptedSecret {
            ciphertext,
            nonce: value_nonce.to_vec(),
            wrapped_data_key,
            wrap_nonce: wrap_nonce.to_vec(),
            key_version: self.key_version,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn decrypt(
        &self,
        profile_id: Uuid,
        secret_id: &str,
        purpose: String,
        ciphertext: &[u8],
        nonce: &[u8],
        wrapped_data_key: &[u8],
        wrap_nonce: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let value_nonce: [u8; NONCE_LENGTH] =
            nonce.try_into().map_err(|_| VaultError::InvalidEnvelope)?;
        let key_nonce: [u8; NONCE_LENGTH] = wrap_nonce
            .try_into()
            .map_err(|_| VaultError::InvalidEnvelope)?;
        let master_cipher = aead_key(self.master_key.as_ref())?;
        let mut data_key_envelope = Zeroizing::new(wrapped_data_key.to_vec());
        let data_key = master_cipher
            .open_in_place(
                Nonce::assume_unique_for_key(key_nonce),
                Aad::from(wrap_aad(profile_id, secret_id, &purpose, self.key_version).as_bytes()),
                &mut data_key_envelope,
            )
            .map_err(|_| VaultError::Decryption)?;
        let value_cipher = aead_key(data_key)?;
        let mut value_envelope = Zeroizing::new(ciphertext.to_vec());
        let plaintext_length = value_cipher
            .open_in_place(
                Nonce::assume_unique_for_key(value_nonce),
                Aad::from(value_aad(profile_id, secret_id, &purpose).as_bytes()),
                &mut value_envelope,
            )
            .map_err(|_| VaultError::Decryption)?
            .len();
        value_envelope.truncate(plaintext_length);
        Ok(value_envelope)
    }
}

fn decode_key(encoded: &str, key_version: i32) -> Result<VaultCipher, VaultError> {
    let mut decoded = Zeroizing::new(
        STANDARD
            .decode(encoded)
            .map_err(|_| VaultError::InvalidKey)?,
    );
    let key: [u8; 32] = decoded
        .as_slice()
        .try_into()
        .map_err(|_| VaultError::InvalidKey)?;
    decoded.zeroize();
    Ok(VaultCipher {
        master_key: Zeroizing::new(key),
        key_version,
    })
}

async fn validate_key_file_permissions(path: &Path) -> Result<(), VaultError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if tokio::fs::metadata(path).await?.permissions().mode() & 0o077 != 0 {
            return Err(VaultError::InsecurePermissions);
        }
    }
    Ok(())
}

async fn read_key_source(
    file: &Option<std::path::PathBuf>,
    value: &Option<SecretString>,
) -> Result<Option<Zeroizing<String>>, AppError> {
    match (file, value) {
        (Some(path), None) => {
            validate_key_file_permissions(path)
                .await
                .map_err(internal)?;
            Ok(Some(Zeroizing::new(
                tokio::fs::read_to_string(path)
                    .await
                    .map_err(VaultError::from)
                    .map_err(internal)?,
            )))
        }
        (None, Some(value)) => Ok(Some(Zeroizing::new(value.expose_secret().to_owned()))),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(AppError::Validation(
            "configure only one vault master-key source per key version".into(),
        )),
    }
}

fn value_aad(profile_id: Uuid, secret_id: &str, purpose: &str) -> String {
    format!("gobrowse:vault:value:v1:{profile_id}:{secret_id}:{purpose}")
}

fn wrap_aad(profile_id: Uuid, secret_id: &str, purpose: &str, key_version: i32) -> String {
    format!("gobrowse:vault:dek:v1:{key_version}:{profile_id}:{secret_id}:{purpose}")
}

fn random_nonce() -> [u8; NONCE_LENGTH] {
    let mut nonce = [0_u8; NONCE_LENGTH];
    rand::rng().fill_bytes(&mut nonce);
    nonce
}

fn aead_key(key: &[u8]) -> Result<LessSafeKey, VaultError> {
    UnboundKey::new(&AES_256_GCM, key)
        .map(LessSafeKey::new)
        .map_err(|_| VaultError::InvalidKey)
}

pub fn algorithm() -> &'static str {
    ALGORITHM
}

fn internal(error: VaultError) -> AppError {
    AppError::Internal(anyhow::Error::new(error))
}

fn validation(error: VaultError) -> AppError {
    AppError::Validation(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_vault() -> Vault {
        Vault {
            cipher: Some(Arc::new(VaultCipher {
                master_key: Zeroizing::new([42; 32]),
                key_version: 1,
            })),
            previous_cipher: None,
        }
    }

    #[test]
    fn envelopes_are_random_and_bound_to_metadata() {
        let vault = test_vault();
        let profile = Uuid::now_v7();
        let secret = SecretString::from("sensitive-token".to_owned());
        let first = vault
            .encrypt(profile, "secret-a", "provider_credential", &secret)
            .unwrap();
        let second = vault
            .encrypt(profile, "secret-a", "provider_credential", &secret)
            .unwrap();
        assert_ne!(first.ciphertext, second.ciphertext);
        let cipher = vault.cipher.as_deref().unwrap();
        let decrypted = cipher
            .decrypt(
                profile,
                "secret-a",
                "provider_credential".into(),
                &first.ciphertext,
                &first.nonce,
                &first.wrapped_data_key,
                &first.wrap_nonce,
            )
            .unwrap();
        assert_eq!(decrypted.as_slice(), b"sensitive-token");
        assert!(
            cipher
                .decrypt(
                    profile,
                    "secret-b",
                    "provider_credential".into(),
                    &first.ciphertext,
                    &first.nonce,
                    &first.wrapped_data_key,
                    &first.wrap_nonce,
                )
                .is_err()
        );
    }

    #[test]
    fn mcp_purposes_require_one_canonical_public_host() {
        for purpose in MCP_PURPOSES {
            assert_eq!(
                validate_secret_metadata(purpose, &["EXAMPLE.COM.".into()]).unwrap(),
                vec!["example.com"]
            );
        }
        assert_eq!(
            validate_secret_metadata(MCP_OAUTH_ACCESS_TOKEN, &["2001:4860:4860::8888".into()])
                .unwrap(),
            vec!["2001:4860:4860::8888"]
        );
        assert_eq!(
            validate_secret_metadata(MCP_OAUTH_ACCESS_TOKEN, &["EXAMPLE.com.".into()]).unwrap(),
            vec!["example.com"]
        );
        assert!(
            validate_secret_metadata(MCP_OAUTH_ACCESS_TOKEN, &["example.com..".into()]).is_err()
        );
        assert_eq!(
            validate_secret_metadata(MCP_OAUTH_ACCESS_TOKEN, &["例え.テスト".into()]).unwrap(),
            vec!["xn--r8jz45g.xn--zckzah"]
        );
    }

    #[test]
    fn mcp_policy_rejects_unknown_purposes_and_unsafe_hosts() {
        for purpose in ["mcp_oauth_token", "mcp_api_key", "mcp_"] {
            assert!(validate_secret_metadata(purpose, &["example.com".into()]).is_err());
        }
        for host in [
            "https://example.com",
            "user@example.com",
            "example.com:443",
            "example.com/path",
            "*.example.com",
            "bad..example.com",
            "127.0.0.1",
            "169.254.169.254",
            "192.168.1.1",
            "[2001:4860:4860::8888]",
        ] {
            assert!(
                validate_secret_metadata(MCP_OAUTH_ACCESS_TOKEN, &[host.into()]).is_err(),
                "host should be rejected: {host}"
            );
        }
        assert!(validate_secret_metadata(MCP_OAUTH_ACCESS_TOKEN, &[]).is_err());
        assert!(
            validate_secret_metadata(
                MCP_OAUTH_ACCESS_TOKEN,
                &["example.com".into(), "other.com".into()]
            )
            .is_err()
        );
        for hosts in [
            vec![" example.com".into()],
            vec!["example.com ".into()],
            vec!["".into()],
            vec!["example.com".into(), "example.com".into()],
        ] {
            assert!(
                validate_secret_metadata(MCP_OAUTH_ACCESS_TOKEN, &hosts).is_err(),
                "raw MCP host entries must be strictly validated: {hosts:?}"
            );
        }
    }

    #[test]
    fn non_mcp_purpose_behavior_remains_unrestricted_by_mcp_policy() {
        assert_eq!(
            validate_secret_metadata(
                "provider_credential",
                &["internal.example".into(), "other.example".into()]
            )
            .unwrap(),
            vec!["internal.example", "other.example"]
        );
        assert_eq!(
            validate_secret_metadata("provider_credential", &["2001:db8::1".into()]).unwrap(),
            vec!["2001:db8::1"]
        );
    }
    #[tokio::test]
    async fn from_settings_validates_current_and_previous_key_material() {
        let current = STANDARD.encode([7_u8; 32]);
        let previous = STANDARD.encode([8_u8; 32]);
        let settings = VaultSettings {
            master_key_base64: Some(SecretString::from(current)),
            key_version: 7,
            previous_master_key_base64: Some(SecretString::from(previous)),
            previous_key_version: Some(6),
            ..VaultSettings::default()
        };
        let vault = Vault::from_settings(&settings)
            .await
            .expect("valid current and previous keys");
        assert!(vault.is_available());
        assert!(vault.cipher_for(7).is_some());
        assert!(vault.cipher_for(6).is_some());
    }

    #[tokio::test]
    async fn from_settings_rejects_invalid_key_material_without_panicking() {
        for value in [
            "not-base64",
            &STANDARD.encode([1_u8; 31]),
            &STANDARD.encode([1_u8; 33]),
        ] {
            let settings = VaultSettings {
                master_key_base64: Some(SecretString::from((*value).to_owned())),
                ..VaultSettings::default()
            };
            assert!(Vault::from_settings(&settings).await.is_err());
        }
        let settings = VaultSettings {
            master_key_base64: Some(SecretString::from(STANDARD.encode([1_u8; 32]))),
            previous_key_version: Some(2),
            ..VaultSettings::default()
        };
        assert!(Vault::from_settings(&settings).await.is_err());
    }

    #[tokio::test]
    async fn from_settings_rejects_unreadable_and_insecure_key_files() {
        let missing = std::env::temp_dir().join(format!("gobrowse-missing-{}", Uuid::now_v7()));
        let missing_settings = VaultSettings {
            master_key_file: Some(missing),
            ..VaultSettings::default()
        };
        assert!(Vault::from_settings(&missing_settings).await.is_err());

        #[cfg(unix)]
        {
            use std::{fs, os::unix::fs::PermissionsExt};

            let path = std::env::temp_dir().join(format!("gobrowse-insecure-{}", Uuid::now_v7()));
            fs::write(&path, STANDARD.encode([3_u8; 32])).expect("write test key");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
                .expect("set insecure test key permissions");
            let settings = VaultSettings {
                master_key_file: Some(path.clone()),
                ..VaultSettings::default()
            };
            assert!(Vault::from_settings(&settings).await.is_err());
            fs::remove_file(path).expect("remove test key");
        }
    }
}
