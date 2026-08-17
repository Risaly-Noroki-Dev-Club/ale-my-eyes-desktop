use crate::{AleError, Result};

const SERVICE: &str = "com.alemyeyes.cloud-api";
const ACCOUNT: &str = "default";
const BACKUP_ACCOUNT: &str = "backup";

/// Stores credentials outside the application configuration file.
pub trait SecretStore: Send + Sync {
    fn get_api_key(&self) -> Result<Option<String>>;
    fn set_api_key(&self, api_key: &str) -> Result<()>;
    fn delete_api_key(&self) -> Result<()>;

    fn get_backup_api_key(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn set_backup_api_key(&self, _api_key: &str) -> Result<()> {
        Ok(())
    }

    fn delete_backup_api_key(&self) -> Result<()> {
        Ok(())
    }
}

pub struct SystemSecretStore;

impl SystemSecretStore {
    fn entry(account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, account)
            .map_err(|error| AleError::ConfigError(format!("无法初始化系统凭据库: {error}")))
    }
}

impl SecretStore for SystemSecretStore {
    fn get_api_key(&self) -> Result<Option<String>> {
        match Self::entry(ACCOUNT)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(AleError::ConfigError(format!(
                "无法读取系统凭据库: {error}"
            ))),
        }
    }

    fn set_api_key(&self, api_key: &str) -> Result<()> {
        Self::entry(ACCOUNT)?
            .set_password(api_key)
            .map_err(|error| {
                AleError::ConfigError(format!("无法保存 API Key 到系统凭据库: {error}"))
            })
    }

    fn delete_api_key(&self) -> Result<()> {
        match Self::entry(ACCOUNT)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(AleError::ConfigError(format!(
                "无法删除系统凭据库中的 API Key: {error}"
            ))),
        }
    }

    fn get_backup_api_key(&self) -> Result<Option<String>> {
        match Self::entry(BACKUP_ACCOUNT)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(AleError::ConfigError(format!(
                "无法读取备用 API Key: {error}"
            ))),
        }
    }

    fn set_backup_api_key(&self, api_key: &str) -> Result<()> {
        Self::entry(BACKUP_ACCOUNT)?
            .set_password(api_key)
            .map_err(|error| AleError::ConfigError(format!("无法保存备用 API Key: {error}")))
    }

    fn delete_backup_api_key(&self) -> Result<()> {
        match Self::entry(BACKUP_ACCOUNT)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(AleError::ConfigError(format!(
                "无法删除备用 API Key: {error}"
            ))),
        }
    }
}
