//! Durable work storage in a workspace: projects, immutable revisions, job
//! results, renders, presentations and feedback.
//!
//! New work lives under `<workspace>/.tunemend/`. Large files are stored
//! once by content hash under `objects/` and are complete before the database
//! row that refers to them is committed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bundle::{ProjectInfo, Revision};

pub const STORE_DIRECTORY: &str = ".tunemend";

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self(format!("storage: {error}"))
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self(format!("file: {error}"))
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self(format!("stored data: {error}"))
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// A recorded tool result, returned again when the same job is repeated.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRecord {
    pub tool: String,
    pub request_hash: String,
    pub response: Value,
}

pub struct Store {
    connection: Connection,
    directory: PathBuf,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    info TEXT NOT NULL,
    settings TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS revisions (
    project TEXT NOT NULL,
    id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (project, id)
);
CREATE TABLE IF NOT EXISTS documents (
    project TEXT NOT NULL,
    collection TEXT NOT NULL,
    id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (project, collection, id)
);
CREATE TABLE IF NOT EXISTS jobs (
    job_id TEXT PRIMARY KEY,
    tool TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    response TEXT NOT NULL
);
";

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Store {
    /// Path of the store directory for a workspace, whether or not it exists.
    pub fn directory_for(workspace: &Path) -> PathBuf {
        let current = workspace.join(STORE_DIRECTORY);
        let legacy = workspace.join(".retro-midi");
        // Keep existing object paths and running review links valid. Do not move
        // or merge stores during startup; the current directory takes priority.
        if !current.exists() && legacy.join("store.sqlite3").is_file() {
            legacy
        } else {
            current
        }
    }

    pub fn exists(workspace: &Path) -> bool {
        Self::directory_for(workspace)
            .join("store.sqlite3")
            .is_file()
    }

    /// Opens the store, creating it when `create` is set.
    pub fn open(workspace: &Path, create: bool) -> StoreResult<Self> {
        let directory = Self::directory_for(workspace);
        if !create && !Self::exists(workspace) {
            return Err(StoreError(format!(
                "no work is stored in {} yet",
                workspace.display()
            )));
        }
        std::fs::create_dir_all(directory.join("objects"))?;
        let connection = Connection::open(directory.join("store.sqlite3"))?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection,
            directory,
        })
    }

    pub fn object_path(&self, hash: &str) -> PathBuf {
        self.directory.join("objects").join(hash)
    }

    /// Writes `bytes` under their hash unless already present and returns the hash.
    pub fn put_object(&self, bytes: &[u8]) -> StoreResult<String> {
        let hash = sha256_hex(bytes);
        let path = self.object_path(&hash);
        if path.is_file() {
            if std::fs::read(&path)? != bytes {
                return Err(StoreError(format!(
                    "stored object {hash} differs from its content hash; source was not accepted"
                )));
            }
        } else {
            let temporary = self
                .directory
                .join("objects")
                .join(format!("{hash}.partial-{}", std::process::id()));
            std::fs::write(&temporary, bytes)?;
            // Windows FlushFileBuffers requires a handle opened for writing.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&temporary)?
                .sync_all()?;
            std::fs::rename(&temporary, &path)?;
        }
        Ok(hash)
    }

    pub fn job(&self, job_id: &str) -> StoreResult<Option<JobRecord>> {
        job_in(&self.connection, job_id)
    }

    /// Runs `write` in one immediate transaction; nothing is kept if it fails.
    pub fn write<T>(&mut self, write: impl FnOnce(&Writer) -> StoreResult<T>) -> StoreResult<T> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = write(&Writer {
            transaction: &transaction,
        })?;
        transaction.commit()?;
        Ok(value)
    }

    pub fn reader(&self) -> Reader<'_> {
        Reader {
            connection: &self.connection,
        }
    }
}

fn job_in(connection: &Connection, job_id: &str) -> StoreResult<Option<JobRecord>> {
    let row = connection
        .query_row(
            "SELECT tool, request_hash, response FROM jobs WHERE job_id = ?1",
            params![job_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(tool, request_hash, response)| {
        Ok(JobRecord {
            tool,
            request_hash,
            response: serde_json::from_str(&response)?,
        })
    })
    .transpose()
}

/// Read access, available inside and outside write transactions.
pub struct Reader<'a> {
    connection: &'a Connection,
}

pub struct Writer<'a> {
    transaction: &'a Transaction<'a>,
}

impl std::ops::Deref for Writer<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.transaction
    }
}

macro_rules! reads {
    ($($owner:ty),*) => {$(
impl $owner {
    fn connection(&self) -> &Connection {
        self.connection_ref()
    }

    pub fn job(&self, job_id: &str) -> StoreResult<Option<JobRecord>> {
        job_in(self.connection(), job_id)
    }

    pub fn project_ids(&self) -> StoreResult<Vec<String>> {
        let mut statement = self.connection().prepare("SELECT id FROM projects ORDER BY id")?;
        let ids = statement.query_map([], |row| row.get(0))?.collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    pub fn project(&self, id: &str) -> StoreResult<Option<(ProjectInfo, Value)>> {
        let row = self
            .connection()
            .query_row("SELECT info, settings FROM projects WHERE id = ?1", params![id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .optional()?;
        row.map(|(info, settings)| Ok((serde_json::from_str(&info)?, serde_json::from_str(&settings)?))).transpose()
    }

    pub fn revision(&self, project: &str, id: &str) -> StoreResult<Option<Revision>> {
        let body = self
            .connection()
            .query_row("SELECT body FROM revisions WHERE project = ?1 AND id = ?2", params![project, id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        body.map(|body| Ok(serde_json::from_str(&body)?)).transpose()
    }

    pub fn revisions(&self, project: &str) -> StoreResult<Vec<Revision>> {
        let mut statement = self.connection().prepare("SELECT body FROM revisions WHERE project = ?1 ORDER BY seq")?;
        let bodies = statement.query_map(params![project], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        bodies.iter().map(|b| Ok(serde_json::from_str(b)?)).collect()
    }

    pub fn revision_count(&self, project: &str) -> StoreResult<u64> {
        let count: i64 =
            self.connection().query_row("SELECT COUNT(*) FROM revisions WHERE project = ?1", params![project], |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn document<T: DeserializeOwned>(&self, project: &str, collection: &str, id: &str) -> StoreResult<Option<T>> {
        let body = self
            .connection()
            .query_row(
                "SELECT body FROM documents WHERE project = ?1 AND collection = ?2 AND id = ?3",
                params![project, collection, id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        body.map(|body| Ok(serde_json::from_str(&body)?)).transpose()
    }

    pub fn documents<T: DeserializeOwned>(&self, project: &str, collection: &str) -> StoreResult<Vec<T>> {
        let mut statement = self
            .connection()
            .prepare("SELECT body FROM documents WHERE project = ?1 AND collection = ?2 ORDER BY seq")?;
        let bodies = statement
            .query_map(params![project, collection], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        bodies.iter().map(|b| Ok(serde_json::from_str(b)?)).collect()
    }

    pub fn document_count(&self, project: &str, collection: &str) -> StoreResult<u64> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM documents WHERE project = ?1 AND collection = ?2",
            params![project, collection],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }
}
    )*};
}

impl Reader<'_> {
    fn connection_ref(&self) -> &Connection {
        self.connection
    }
}

impl Writer<'_> {
    fn connection_ref(&self) -> &Connection {
        self.transaction
    }
}

reads!(Reader<'_>, Writer<'_>);

impl Writer<'_> {
    pub fn record_job(
        &self,
        job_id: &str,
        tool: &str,
        request_hash: &str,
        response: &Value,
    ) -> StoreResult<()> {
        self.connection().execute(
            "INSERT INTO jobs (job_id, tool, request_hash, response) VALUES (?1, ?2, ?3, ?4)",
            params![job_id, tool, request_hash, response.to_string()],
        )?;
        Ok(())
    }

    pub fn insert_project(&self, info: &ProjectInfo, settings: &Value) -> StoreResult<()> {
        let id = &info.name;
        self.connection().execute(
            "INSERT INTO projects (id, info, settings) VALUES (?1, ?2, ?3)",
            params![id, serde_json::to_string(info)?, settings.to_string()],
        )?;
        Ok(())
    }

    pub fn update_project(&self, info: &ProjectInfo, settings: &Value) -> StoreResult<()> {
        self.connection().execute(
            "UPDATE projects SET info = ?2, settings = ?3 WHERE id = ?1",
            params![
                info.name,
                serde_json::to_string(info)?,
                settings.to_string()
            ],
        )?;
        Ok(())
    }

    pub fn insert_revision(&self, project: &str, revision: &Revision) -> StoreResult<()> {
        let seq = self.revision_count(project)? as i64 + 1;
        self.connection().execute(
            "INSERT INTO revisions (project, id, seq, body) VALUES (?1, ?2, ?3, ?4)",
            params![project, revision.id, seq, serde_json::to_string(revision)?],
        )?;
        Ok(())
    }

    pub fn put_document<T: Serialize>(
        &self,
        project: &str,
        collection: &str,
        id: &str,
        body: &T,
    ) -> StoreResult<()> {
        let seq: i64 = self.connection().query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM documents WHERE project = ?1 AND collection = ?2",
            params![project, collection],
            |row| row.get(0),
        )?;
        self.connection().execute(
            "INSERT INTO documents (project, collection, id, seq, body) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (project, collection, id) DO UPDATE SET body = excluded.body",
            params![project, collection, id, seq, serde_json::to_string(body)?],
        )?;
        Ok(())
    }

    pub fn delete_document(&self, project: &str, collection: &str, id: &str) -> StoreResult<()> {
        self.connection().execute(
            "DELETE FROM documents WHERE project = ?1 AND collection = ?2 AND id = ?3",
            params![project, collection, id],
        )?;
        Ok(())
    }
}
