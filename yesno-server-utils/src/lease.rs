//! Conditional archive-writer ownership.
//!
//! Remote stores use create-or-CAS on `writer.pb`; local archives use the
//! kernel's advisory file lock because object-store's local backend deliberately
//! does not implement conditional update. State publication is independently
//! CAS-fenced, and data objects are immutable, so a writer that outlives its
//! lease can neither advance a cursor nor overwrite the replacement's history.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use prost::Message;

use crate::archive::{pb, ArchiveError, ArchiveStore, SCHEMA_VERSION, WRITER_OBJECT};

/// Holds local kernel ownership for the lifetime of one sidecar run.
#[derive(Debug)]
pub struct WriterLease {
    store: ArchiveStore,
    local_lock: Option<File>,
}

impl WriterLease {
    /// The opaque owner id written into every state publication.
    pub fn owner_id(&self) -> Vec<u8> {
        self.store.owner_id.lock().unwrap().clone()
    }

    /// Renew a remote lease. Local ownership is tied to the live file handle.
    pub async fn renew(&self, ttl_secs: u64) -> Result<(), ArchiveError> {
        self.store.renew_writer_lease(ttl_secs).await
    }

    /// Relinquish ownership early. A crashed remote writer simply expires.
    pub async fn release(mut self) -> Result<(), ArchiveError> {
        if self.local_lock.take().is_some() {
            self.store.owner_id.lock().unwrap().clear();
            return Ok(());
        }
        self.store.expire_writer_lease().await?;
        self.store.owner_id.lock().unwrap().clear();
        Ok(())
    }
}

fn now_millis() -> Result<u64, ArchiveError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .map_err(|_| "system time does not fit archive lease timestamp")?)
}

fn lease_message(owner_id: Vec<u8>, ttl_secs: u64) -> Result<pb::ArchiveWriterLease, ArchiveError> {
    let expires_unix_millis = now_millis()?
        .checked_add(ttl_secs.saturating_mul(1000))
        .ok_or("archive writer lease expiration overflowed")?;
    Ok(pb::ArchiveWriterLease {
        schema_version: SCHEMA_VERSION,
        owner_id,
        expires_unix_millis,
    })
}

/// Generate an opaque writer identity from the operating system RNG.
pub fn new_writer_id() -> Result<Vec<u8>, ArchiveError> {
    let mut id = vec![0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut id)?;
    Ok(id)
}

impl ArchiveStore {
    /// Acquire exclusive writer ownership, replacing an expired remote lease by
    /// compare-and-swap. A live owner is never displaced.
    pub async fn acquire_writer(
        &self,
        owner_id: Vec<u8>,
        ttl_secs: u64,
    ) -> Result<WriterLease, ArchiveError> {
        if owner_id.len() != 16 || ttl_secs == 0 {
            return Err(
                "archive writer identity must be 16 bytes and lease TTL must be positive".into(),
            );
        }
        if let Some(root) = &self.local_prefix {
            std::fs::create_dir_all(root)?;
            let path = root.join(WRITER_OBJECT);
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            file.try_lock().map_err(|error| {
                std::io::Error::other(format!("archive prefix already has a live writer: {error}"))
            })?;
            let lease = lease_message(owner_id.clone(), ttl_secs)?.encode_to_vec();
            file.set_len(0)?;
            file.rewind()?;
            file.write_all(&lease)?;
            file.sync_all()?;
            *self.owner_id.lock().unwrap() = owner_id;
            *self.lease_ttl_secs.lock().unwrap() = ttl_secs;
            return Ok(WriterLease {
                store: self.clone(),
                local_lock: Some(file),
            });
        }

        let path = self.path(WRITER_OBJECT);
        let mode = match self.inner.get(&path).await {
            Ok(result) => {
                let version = UpdateVersion {
                    e_tag: result.meta.e_tag.clone(),
                    version: result.meta.version.clone(),
                };
                let current = pb::ArchiveWriterLease::decode(result.bytes().await?)?;
                if current.schema_version != SCHEMA_VERSION || current.owner_id.len() != 16 {
                    return Err(
                        "archive writer lease is malformed or uses an unsupported schema".into(),
                    );
                }
                if current.expires_unix_millis > now_millis()? {
                    return Err("archive prefix already has a live writer lease".into());
                }
                PutMode::Update(version)
            }
            Err(object_store::Error::NotFound { .. }) => PutMode::Create,
            Err(error) => return Err(error.into()),
        };
        let lease = lease_message(owner_id.clone(), ttl_secs)?;
        let result = self
            .inner
            .put_opts(
                &path,
                PutPayload::from(lease.encode_to_vec()),
                PutOptions::from(mode),
            )
            .await
            .map_err(|error| match error {
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. } => {
                    "archive writer lease changed while it was being acquired".into()
                }
                other => ArchiveError::from(other),
            })?;
        *self.lease_version.lock().unwrap() = Some(result.into());
        *self.owner_id.lock().unwrap() = owner_id;
        *self.lease_ttl_secs.lock().unwrap() = ttl_secs;
        Ok(WriterLease {
            store: self.clone(),
            local_lock: None,
        })
    }

    /// Fence state with the lease owner before any data object is accepted.
    pub async fn claim_state(
        &self,
        local_path: &std::path::Path,
        state: &mut pb::ArchiveState,
    ) -> Result<(), ArchiveError> {
        state.writer_id = self.owner_id.lock().unwrap().clone();
        self.publish_state(local_path, state).await
    }

    pub(crate) async fn renew_writer_lease(&self, ttl_secs: u64) -> Result<(), ArchiveError> {
        if self.local_prefix.is_some() {
            return Ok(());
        }
        let owner = self.owner_id.lock().unwrap().clone();
        let version = self
            .lease_version
            .lock()
            .unwrap()
            .clone()
            .ok_or("archive writer lease has no conditional version")?;
        let lease = lease_message(owner, ttl_secs)?;
        let result = self
            .inner
            .put_opts(
                &self.path(WRITER_OBJECT),
                PutPayload::from(lease.encode_to_vec()),
                PutOptions::from(PutMode::Update(version)),
            )
            .await
            .map_err(|error| match error {
                object_store::Error::Precondition { .. } => {
                    "archive writer lease was fenced by another owner".into()
                }
                other => ArchiveError::from(other),
            })?;
        *self.lease_version.lock().unwrap() = Some(result.into());
        Ok(())
    }

    async fn expire_writer_lease(&self) -> Result<(), ArchiveError> {
        let owner = self.owner_id.lock().unwrap().clone();
        let version = self
            .lease_version
            .lock()
            .unwrap()
            .clone()
            .ok_or("archive writer lease has no conditional version")?;
        let lease = pb::ArchiveWriterLease {
            schema_version: SCHEMA_VERSION,
            owner_id: owner,
            expires_unix_millis: now_millis()?,
        };
        match self
            .inner
            .put_opts(
                &self.path(WRITER_OBJECT),
                PutPayload::from(lease.encode_to_vec()),
                PutOptions::from(PutMode::Update(version)),
            )
            .await
        {
            Ok(result) => {
                *self.lease_version.lock().unwrap() = Some(result.into());

                Ok(())
            }
            Err(object_store::Error::Precondition { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;

    use super::*;
    use crate::archive::{new_state, pb};

    fn state() -> pb::ArchiveState {
        new_state(
            vec![9; 16],
            1,
            vec![pb::WalCursor {
                shard: 0,
                archived_lsn: 0,
                history_fingerprint: Vec::new(),
            }],
        )
    }

    #[tokio::test]
    async fn a_live_remote_writer_cannot_be_displaced() {
        let inner = Arc::new(InMemory::new());
        let first = ArchiveStore::new(inner.clone(), ObjectPath::ROOT);
        let second = ArchiveStore::new(inner, ObjectPath::ROOT);
        let lease = first.acquire_writer(vec![1; 16], 30).await.unwrap();
        let error = second.acquire_writer(vec![2; 16], 30).await.unwrap_err();
        assert!(error.to_string().contains("live writer lease"));
        lease.release().await.unwrap();
    }

    #[tokio::test]
    async fn a_replacement_lease_fences_state_before_the_new_owner_claims_it() {
        let inner = Arc::new(InMemory::new());
        let first = ArchiveStore::new(inner.clone(), ObjectPath::ROOT);
        let second = ArchiveStore::new(inner, ObjectPath::ROOT);
        let tmp = tempfile::tempdir().unwrap();

        let first_lease = first.acquire_writer(vec![1; 16], 30).await.unwrap();
        let mut first_state = state();
        first
            .claim_state(&tmp.path().join("first.pb"), &mut first_state)
            .await
            .unwrap();
        first_lease.release().await.unwrap();

        let second_lease = second.acquire_writer(vec![2; 16], 30).await.unwrap();
        first_state.event_sequence = 1;
        *first.owner_id.lock().unwrap() = vec![1; 16];
        let error = first
            .publish_state(&tmp.path().join("stale.pb"), &first_state)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("fenced"));

        let mut second_state = second.load_state().await.unwrap().unwrap();
        second
            .claim_state(&tmp.path().join("second.pb"), &mut second_state)
            .await
            .unwrap();
        second_lease.release().await.unwrap();
    }
}
