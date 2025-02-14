use anyhow::Context;
use near_crypto::{InMemorySigner, PublicKey, SecretKey, Signer};
use near_primitives::types::AccountId;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Account {
    #[serde(rename = "account_id")]
    pub id: AccountId,
    pub public_key: PublicKey,
    pub secret_key: SecretKey,
    // New transaction must have a nonce bigger than this.
    pub nonce: u64,
}

impl Account {
    pub fn new(id: AccountId, secret_key: SecretKey, nonce: u64) -> Self {
        Self { id, public_key: secret_key.public_key(), secret_key, nonce }
    }

    pub fn from_file(path: &Path) -> anyhow::Result<Account> {
        let content = fs::read_to_string(path)?;
        let account = serde_json::from_str(&content)
            .with_context(|| format!("failed reading file {path:?} as 'Account'"))?;
        Ok(account)
    }

    pub fn write_to_dir(&self, dir: &Path) -> anyhow::Result<()> {
        if !dir.exists() {
            std::fs::create_dir(dir)?;
        }

        let json = serde_json::to_string(self)?;
        let mut file_name = self.id.to_string();
        file_name.push_str(".json");
        let file_path = dir.join(file_name);
        fs::write(file_path, json)?;
        Ok(())
    }

    pub fn as_signer(&self) -> Signer {
        Signer::from(InMemorySigner::from_secret_key(self.id.clone(), self.secret_key.clone()))
    }
}

/// Tries to deserialize all json files in `dir` as [`Account`].
pub fn accounts_from_dir(dir: &Path) -> anyhow::Result<Vec<Account>> {
    let mut accounts = vec![];
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let file_extension = path.extension();
        if file_extension.is_none() || file_extension.unwrap() != "json" {
            continue;
        }
        match Account::from_file(&path) {
            Ok(account) => accounts.push(account),
            Err(err) => tracing::debug!("{err}"),
        }
    }

    Ok(accounts)
}
