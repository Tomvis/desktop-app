//! Persistent Sendspin identity and pairing records.
//!
//! Sendspin encrypts every connection with Noise. The client's identity
//! (Curve25519 key + pairing PSK) is its `client_id`, so it must survive
//! restarts; the long-term PSKs the server hands out on pairing are kept in a
//! small JSON store. Both live next to `settings.json`.

use parking_lot::Mutex;
use sendspin::protocol::pairing::{PairingRecord, PairingStore, StoreError};
use sendspin::{ClientCredentials, Psk};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const CREDENTIALS_FILE: &str = "sendspin_credentials.bin";
const PAIRINGS_FILE: &str = "sendspin_pairings.json";

fn storage_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("music-assistant-companion"))
}

/// Write a file atomically, readable only by the current user.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// Load the client credentials, generating and persisting them on first use.
pub fn credentials() -> Result<ClientCredentials, String> {
    static CACHE: OnceLock<ClientCredentials> = OnceLock::new();
    if let Some(creds) = CACHE.get() {
        return Ok(creds.clone());
    }
    let dir = storage_dir().ok_or("no config directory for Sendspin credentials")?;
    let path = dir.join(CREDENTIALS_FILE);
    let creds = match std::fs::read(&path) {
        Ok(bytes) => ClientCredentials::from_bytes(&bytes).map_err(|e| {
            format!(
                "Sendspin credentials at {} are corrupt: {e}",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let creds = ClientCredentials::generate()
                .map_err(|e| format!("Failed to generate Sendspin credentials: {e}"))?;
            write_private(&path, &creds.to_bytes())
                .map_err(|e| format!("Failed to save Sendspin credentials: {e}"))?;
            log::info!(
                "[Sendspin] Generated new client identity {}",
                creds.client_id()
            );
            creds
        }
        Err(e) => return Err(format!("Failed to read Sendspin credentials: {e}")),
    };
    Ok(CACHE.get_or_init(|| creds).clone())
}

/// The Sendspin `client_id` (base64url public key); also the MA player id.
pub fn client_id() -> Result<String, String> {
    credentials().map(|c| c.client_id())
}

/// The process-wide pairing store.
pub fn pairing_store() -> Arc<dyn PairingStore> {
    static STORE: OnceLock<Arc<FilePairingStore>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            Arc::new(FilePairingStore::open(
                storage_dir().map(|d| d.join(PAIRINGS_FILE)),
            ))
        })
        .clone()
}

#[derive(Serialize, Deserialize)]
struct StoredRecord {
    psk: String,
    server_id: String,
    used: bool,
}

/// JSON-file-backed [`PairingStore`]. Writes are small and rare (one per
/// pairing), so they happen synchronously.
pub struct FilePairingStore {
    path: Option<PathBuf>,
    records: Mutex<Vec<PairingRecord>>,
}

impl FilePairingStore {
    fn open(path: Option<PathBuf>) -> Self {
        let records = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<StoredRecord>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                Some(PairingRecord {
                    psk: Psk::from_b64url(&r.psk).ok()?,
                    server_id: r.server_id,
                    used: r.used,
                })
            })
            .collect();
        Self {
            path,
            records: Mutex::new(records),
        }
    }

    fn save(&self, records: &[PairingRecord]) -> Result<(), StoreError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let stored: Vec<StoredRecord> = records
            .iter()
            .map(|r| StoredRecord {
                psk: r.psk.to_b64url(),
                server_id: r.server_id.clone(),
                used: r.used,
            })
            .collect();
        let json = serde_json::to_vec_pretty(&stored).map_err(|_| StoreError::StorageExhausted)?;
        write_private(path, &json).map_err(|e| {
            log::error!("[Sendspin] Failed to save pairing records: {e}");
            StoreError::StorageExhausted
        })
    }
}

impl PairingStore for FilePairingStore {
    fn records(&self) -> Vec<PairingRecord> {
        self.records.lock().clone()
    }

    fn add_record(&self, record: PairingRecord) -> Result<(), StoreError> {
        let mut records = self.records.lock();
        let psk_id = record.psk_id();
        if records
            .iter()
            .any(|r| r.psk_id() == psk_id && r.server_id != record.server_id)
        {
            return Err(StoreError::AlreadyExists);
        }
        // A record for a server replaces that server's previous record.
        let mut updated: Vec<PairingRecord> = records
            .iter()
            .filter(|r| r.server_id != record.server_id)
            .cloned()
            .collect();
        log::info!("[Sendspin] Paired with server {}", record.server_id);
        updated.push(record);
        self.save(&updated)?;
        *records = updated;
        Ok(())
    }

    fn remove_record(&self, psk_id: &str) -> Result<(), StoreError> {
        let mut records = self.records.lock();
        if !records.iter().any(|r| r.psk_id() == psk_id) {
            return Err(StoreError::NotFound);
        }
        let updated: Vec<PairingRecord> = records
            .iter()
            .filter(|r| r.psk_id() != psk_id)
            .cloned()
            .collect();
        self.save(&updated)?;
        *records = updated;
        Ok(())
    }

    fn mark_used(&self, psk_id: &str) {
        let mut records = self.records.lock();
        let mut changed = false;
        for r in records.iter_mut() {
            if r.psk_id() == psk_id && !r.used {
                r.used = true;
                changed = true;
            }
        }
        if changed {
            let _ = self.save(&records);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(server_id: &str) -> PairingRecord {
        PairingRecord {
            psk: Psk::generate().unwrap(),
            server_id: server_id.to_string(),
            used: false,
        }
    }

    #[test]
    fn records_round_trip_and_replace_per_server() {
        let dir = std::env::temp_dir().join(format!("ss-store-{}", std::process::id()));
        let path = dir.join(PAIRINGS_FILE);
        let store = FilePairingStore::open(Some(path.clone()));
        store.add_record(record("server-a")).unwrap();
        let replacement = record("server-a");
        let replacement_id = replacement.psk_id();
        store.add_record(replacement).unwrap();
        store.add_record(record("server-b")).unwrap();
        store.mark_used(&replacement_id);

        let reopened = FilePairingStore::open(Some(path));
        let records = reopened.records();
        assert_eq!(records.len(), 2);
        let a = records.iter().find(|r| r.server_id == "server-a").unwrap();
        assert_eq!(a.psk_id(), replacement_id);
        assert!(a.used);
        reopened.remove_record(&replacement_id).unwrap();
        assert_eq!(reopened.records().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
