use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

pub const WORKSPACE_METADATA_SCHEMA_VERSION: u32 = 1;

const STORE_DIRECTORY: &str = "jj-waltz";
const MANIFEST_FILE: &str = "manifest.json";
const WORKSPACES_DIRECTORY: &str = "workspaces";
const TEMP_ATTEMPTS: usize = 100;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorkspaceMetadata {
    pub workspace_name: String,
    pub created_at_unix_ms: u64,
    pub creation_operation_id: String,
    pub creation_base_commit_id: String,
    pub associated_bookmark: Option<String>,
    pub intended_remote: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceMetadataStore {
    root: PathBuf,
    repository_config_path: PathBuf,
    repository_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryManifest {
    schema_version: u32,
    repository_id: String,
    repository_config_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRecord {
    schema_version: u32,
    metadata: ManagedWorkspaceMetadata,
}

impl WorkspaceMetadataStore {
    /// Locates repository-scoped metadata without creating anything on disk.
    pub fn from_repo_config_path(path: impl AsRef<Path>) -> Result<Self> {
        let repository_config_path = normalize_repo_config_path(path.as_ref())?;
        let parent = repository_config_path.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "repository config path has no parent: {}",
                repository_config_path.display()
            )
        })?;
        let repository_id = repository_id(&repository_config_path);

        Ok(Self {
            root: parent.join(STORE_DIRECTORY),
            repository_config_path,
            repository_id,
        })
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list(&self) -> Result<Vec<ManagedWorkspaceMetadata>> {
        if !self.validate_existing_store()? {
            return Ok(Vec::new());
        }

        let Some(entries) = read_directory_if_present(&self.workspaces_directory())? else {
            return Ok(Vec::new());
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| {
                format!(
                    "failed to read workspace metadata directory {}",
                    self.workspaces_directory().display()
                )
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if !entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", path.display()))?
                .is_file()
            {
                bail!(
                    "workspace metadata record is not a regular file: {}",
                    path.display()
                );
            }
            records.push(self.read_record(&path)?);
        }
        records.sort_by(|left, right| left.workspace_name.cmp(&right.workspace_name));
        Ok(records)
    }

    pub fn get(&self, workspace_name: &str) -> Result<Option<ManagedWorkspaceMetadata>> {
        validate_workspace_name(workspace_name)?;
        if !self.validate_existing_store()? {
            return Ok(None);
        }

        let path = self.workspace_path(workspace_name);
        match fs::read(&path) {
            Ok(contents) => self.parse_record(&path, &contents).map(Some),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Creates a record without replacing an existing managed workspace.
    pub fn insert(&self, metadata: &ManagedWorkspaceMetadata) -> Result<()> {
        validate_metadata(metadata)?;
        if self.get(&metadata.workspace_name)?.is_some() {
            bail!("workspace is already managed: {}", metadata.workspace_name);
        }

        self.ensure_initialized()?;
        let path = self.workspace_path(&metadata.workspace_name);
        let record = WorkspaceRecord {
            schema_version: WORKSPACE_METADATA_SCHEMA_VERSION,
            metadata: metadata.clone(),
        };
        write_json_new(&path, &record).with_context(|| {
            format!(
                "failed to insert metadata for workspace {}",
                metadata.workspace_name
            )
        })
    }

    /// Atomically replaces one workspace record and returns its prior value.
    pub fn upsert(
        &self,
        metadata: &ManagedWorkspaceMetadata,
    ) -> Result<Option<ManagedWorkspaceMetadata>> {
        validate_metadata(metadata)?;
        let previous = self.get(&metadata.workspace_name)?;
        self.ensure_initialized()?;

        let record = WorkspaceRecord {
            schema_version: WORKSPACE_METADATA_SCHEMA_VERSION,
            metadata: metadata.clone(),
        };
        let path = self.workspace_path(&metadata.workspace_name);
        write_json_atomic(&path, &record).with_context(|| {
            format!(
                "failed to write metadata for workspace {}",
                metadata.workspace_name
            )
        })?;
        Ok(previous)
    }

    pub fn remove(&self, workspace_name: &str) -> Result<Option<ManagedWorkspaceMetadata>> {
        let previous = self.get(workspace_name)?;
        if previous.is_none() {
            return Ok(None);
        }

        match fs::remove_file(self.workspace_path(workspace_name)) {
            Ok(()) => Ok(previous),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("failed to remove metadata for {workspace_name}")),
        }
    }

    /// Removes a record only when its current contents match `expected`.
    pub fn remove_if_matches(&self, expected: &ManagedWorkspaceMetadata) -> Result<bool> {
        validate_metadata(expected)?;
        if self.get(&expected.workspace_name)?.as_ref() != Some(expected) {
            return Ok(false);
        }

        match fs::remove_file(self.workspace_path(&expected.workspace_name)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| {
                format!("failed to remove metadata for {}", expected.workspace_name)
            }),
        }
    }

    fn ensure_initialized(&self) -> Result<()> {
        if !self.validate_existing_store()? {
            fs::create_dir_all(&self.root).with_context(|| {
                format!("failed to create metadata store {}", self.root.display())
            })?;

            // Recheck after creating the directory so concurrent initialization
            // either validates an identical manifest or observes an empty store.
            if !self.validate_existing_store()? {
                write_json_atomic(&self.manifest_path(), &self.expected_manifest())
                    .context("failed to initialize workspace metadata manifest")?;
            }
        }
        self.validate_manifest()?;

        let directory = self.workspaces_directory();
        match fs::create_dir(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&directory)
                    .with_context(|| format!("failed to inspect {}", directory.display()))?;
                if metadata.file_type().is_dir() {
                    Ok(())
                } else {
                    bail!(
                        "workspace metadata path is not a directory: {}",
                        directory.display()
                    )
                }
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to create {}", directory.display()))
            }
        }
    }

    /// Returns false only for a genuinely absent or empty store.
    fn validate_existing_store(&self) -> Result<bool> {
        let root_metadata = match fs::symlink_metadata(&self.root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", self.root.display()));
            }
        };
        if !root_metadata.file_type().is_dir() {
            bail!(
                "workspace metadata store is not a directory: {}",
                self.root.display()
            );
        }

        match fs::metadata(self.manifest_path()) {
            Ok(metadata) if metadata.is_file() => {
                self.validate_manifest()?;
                Ok(true)
            }
            Ok(_) => bail!(
                "workspace metadata manifest is not a regular file: {}",
                self.manifest_path().display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let entries = fs::read_dir(&self.root)
                    .with_context(|| {
                        format!("failed to read metadata store {}", self.root.display())
                    })?
                    .collect::<io::Result<Vec<_>>>()
                    .with_context(|| {
                        format!("failed to read metadata store {}", self.root.display())
                    })?;
                if entries.is_empty()
                    || entries
                        .iter()
                        .all(|entry| is_manifest_temporary_file(&entry.file_name()))
                {
                    Ok(false)
                } else if entries
                    .iter()
                    .any(|entry| entry.file_name() == MANIFEST_FILE)
                {
                    // A concurrent initializer may have installed the manifest
                    // after the metadata lookup above.
                    self.validate_manifest()?;
                    Ok(true)
                } else {
                    bail!(
                        "workspace metadata store is missing manifest: {}",
                        self.manifest_path().display()
                    )
                }
            }
            Err(error) => Err(error)
                .with_context(|| format!("failed to inspect {}", self.manifest_path().display())),
        }
    }

    fn validate_manifest(&self) -> Result<()> {
        let path = self.manifest_path();
        let contents = fs::read(&path)
            .with_context(|| format!("failed to read metadata manifest {}", path.display()))?;
        let manifest: RepositoryManifest = serde_json::from_slice(&contents)
            .with_context(|| format!("metadata manifest is corrupt: {}", path.display()))?;
        if manifest.schema_version != WORKSPACE_METADATA_SCHEMA_VERSION {
            bail!(
                "unsupported workspace metadata schema {} in {} (expected {})",
                manifest.schema_version,
                path.display(),
                WORKSPACE_METADATA_SCHEMA_VERSION
            );
        }

        let expected = self.expected_manifest();
        if manifest.repository_id != expected.repository_id
            || manifest.repository_config_path != expected.repository_config_path
        {
            bail!(
                "workspace metadata repository identity mismatch in {}: expected {}, found {}",
                path.display(),
                expected.repository_id,
                manifest.repository_id
            );
        }
        Ok(())
    }

    fn expected_manifest(&self) -> RepositoryManifest {
        RepositoryManifest {
            schema_version: WORKSPACE_METADATA_SCHEMA_VERSION,
            repository_id: self.repository_id.clone(),
            repository_config_path: self.repository_config_path.to_string_lossy().into_owned(),
        }
    }

    fn read_record(&self, path: &Path) -> Result<ManagedWorkspaceMetadata> {
        let contents = fs::read(path)
            .with_context(|| format!("failed to read workspace metadata {}", path.display()))?;
        self.parse_record(path, &contents)
    }

    fn parse_record(&self, path: &Path, contents: &[u8]) -> Result<ManagedWorkspaceMetadata> {
        let record: WorkspaceRecord = serde_json::from_slice(contents)
            .with_context(|| format!("workspace metadata is corrupt: {}", path.display()))?;
        if record.schema_version != WORKSPACE_METADATA_SCHEMA_VERSION {
            bail!(
                "unsupported workspace metadata schema {} in {} (expected {})",
                record.schema_version,
                path.display(),
                WORKSPACE_METADATA_SCHEMA_VERSION
            );
        }
        validate_metadata(&record.metadata)
            .with_context(|| format!("invalid workspace metadata in {}", path.display()))?;
        let expected_path = self.workspace_path(&record.metadata.workspace_name);
        if path.file_name() != expected_path.file_name() {
            bail!(
                "workspace metadata filename does not match embedded name in {}",
                path.display()
            );
        }
        Ok(record.metadata)
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_FILE)
    }

    fn workspaces_directory(&self) -> PathBuf {
        self.root.join(WORKSPACES_DIRECTORY)
    }

    fn workspace_path(&self, workspace_name: &str) -> PathBuf {
        self.workspaces_directory()
            .join(workspace_file_name(workspace_name))
    }
}

fn validate_metadata(metadata: &ManagedWorkspaceMetadata) -> Result<()> {
    validate_workspace_name(&metadata.workspace_name)
}

fn validate_workspace_name(workspace_name: &str) -> Result<()> {
    if workspace_name.is_empty() {
        bail!("workspace name cannot be empty");
    }
    Ok(())
}

fn normalize_repo_config_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()
            .context("failed to resolve current directory")?
            .join(path)
    };
    if absolute.file_name().is_none() {
        bail!(
            "repository config path is not a file path: {}",
            absolute.display()
        );
    }
    let file_name = absolute
        .file_name()
        .expect("file name checked above")
        .to_owned();
    let parent = absolute.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "repository config path has no parent: {}",
            absolute.display()
        )
    })?;
    let parent = parent.canonicalize().with_context(|| {
        format!(
            "failed to resolve repository config parent {}",
            parent.display()
        )
    })?;
    Ok(parent.join(file_name))
}

fn repository_id(path: &Path) -> String {
    format!(
        "repo-{:032x}",
        stable_hash(b"repository", &path_bytes(path))
    )
}

fn workspace_file_name(workspace_name: &str) -> String {
    format!(
        "workspace-{:032x}.json",
        stable_hash(b"workspace", workspace_name.as_bytes())
    )
}

fn stable_hash(domain: &[u8], value: &[u8]) -> u128 {
    const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    let mut hash = OFFSET_BASIS;
    for byte in domain
        .iter()
        .copied()
        .chain([0])
        .chain(value.iter().copied())
    {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

fn read_directory_if_present(path: &Path) -> Result<Option<fs::ReadDir>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if !metadata.file_type().is_dir() {
        bail!("metadata path is not a directory: {}", path.display());
    }
    fs::read_dir(path)
        .map(Some)
        .with_context(|| format!("failed to read {}", path.display()))
}

fn is_manifest_temporary_file(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(&format!(".{MANIFEST_FILE}.tmp-")))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let contents = json_bytes(value)?;
    let (mut file, mut temporary) = create_temporary_file(path)?;
    file.write_all(&contents)
        .with_context(|| format!("failed to write {}", temporary.path().display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temporary.path().display()))?;
    drop(file);

    atomic_replace(temporary.path(), path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            temporary.path().display()
        )
    })?;
    temporary.disarm();
    Ok(())
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let contents = json_bytes(value)?;
    let (mut file, mut temporary) = create_temporary_file(path)?;
    file.write_all(&contents)
        .with_context(|| format!("failed to write {}", temporary.path().display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temporary.path().display()))?;
    drop(file);

    match fs::hard_link(temporary.path(), path) {
        Ok(()) => {
            if fs::remove_file(temporary.path()).is_ok() {
                temporary.disarm();
            }
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            bail!(
                "workspace metadata record already exists: {}",
                path.display()
            )
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to install new workspace metadata record {}",
                path.display()
            )
        }),
    }
}

fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut contents = serde_json::to_vec_pretty(value).context("failed to serialize metadata")?;
    contents.push(b'\n');
    Ok(contents)
}

fn create_temporary_file(path: &Path) -> Result<(File, TemporaryPath)> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("metadata path has no parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!("metadata path has no UTF-8 filename: {}", path.display())
        })?;

    for _ in 0..TEMP_ATTEMPTS {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(".{file_name}.tmp-{}-{id}", process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((file, TemporaryPath::new(temporary_path))),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create temporary metadata file {file_name}")
                });
            }
        }
    }

    bail!(
        "failed to create unique temporary metadata file beside {}",
        path.display()
    )
}

struct TemporaryPath {
    path: PathBuf,
    armed: bool,
}

impl TemporaryPath {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex_w(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: Both paths are NUL-terminated and remain alive for the call.
    let result = unsafe {
        move_file_ex_w(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    fn test_store(tempdir: &TempDir) -> WorkspaceMetadataStore {
        let config_directory = tempdir.path().join("repo");
        fs::create_dir_all(&config_directory).expect("create repository config directory");
        WorkspaceMetadataStore::from_repo_config_path(config_directory.join("config.toml"))
            .expect("create metadata store")
    }

    fn metadata(workspace_name: &str) -> ManagedWorkspaceMetadata {
        ManagedWorkspaceMetadata {
            workspace_name: workspace_name.to_owned(),
            created_at_unix_ms: 1_750_000_000_123,
            creation_operation_id: "operation-1".to_owned(),
            creation_base_commit_id: "commit-1".to_owned(),
            associated_bookmark: Some(format!("wip/{workspace_name}")),
            intended_remote: Some("origin".to_owned()),
        }
    }

    #[test]
    fn absent_reads_do_not_create_store_and_roundtrip_after_upsert() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let store = test_store(&tempdir);
        let expected = metadata("solver");

        assert_eq!(store.get("solver").expect("read absent record"), None);
        assert!(store.list().expect("list absent store").is_empty());
        assert!(!store.root().exists());

        assert_eq!(store.upsert(&expected).expect("insert record"), None);
        assert_eq!(
            store.get("solver").expect("read record"),
            Some(expected.clone())
        );
        assert_eq!(store.list().expect("list records"), vec![expected.clone()]);

        let reopened = test_store(&tempdir);
        assert_eq!(reopened.repository_id(), store.repository_id());
        assert_eq!(
            reopened.get("solver").expect("reopen record"),
            Some(expected)
        );
    }

    #[test]
    fn independent_records_do_not_overwrite_each_other() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let store = test_store(&tempdir);
        let first = metadata("alpha");
        let second = metadata("beta");
        store.upsert(&first).expect("insert first record");
        store.upsert(&second).expect("insert second record");

        let mut updated = first.clone();
        updated.associated_bookmark = Some("wip/updated-alpha".to_owned());
        assert_eq!(
            store.upsert(&updated).expect("update first record"),
            Some(first)
        );
        assert_eq!(
            store.get("beta").expect("read second record"),
            Some(second.clone())
        );
        assert_eq!(store.list().expect("list records"), vec![updated, second]);
    }

    #[test]
    fn concurrent_writes_to_different_records_are_independent() {
        const RECORD_COUNT: usize = 12;

        let tempdir = tempfile::tempdir().expect("create temp directory");
        let store = Arc::new(test_store(&tempdir));
        let barrier = Arc::new(Barrier::new(RECORD_COUNT));
        let writers = (0..RECORD_COUNT)
            .map(|index| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let record = metadata(&format!("workspace-{index:02}"));
                    barrier.wait();
                    store.upsert(&record).expect("write independent record");
                })
            })
            .collect::<Vec<_>>();

        for writer in writers {
            writer.join().expect("writer thread succeeds");
        }
        assert_eq!(
            store.list().expect("list concurrent records").len(),
            RECORD_COUNT
        );
    }

    #[test]
    fn corrupt_manifest_and_record_are_errors_not_resets() {
        let manifest_tempdir = tempfile::tempdir().expect("create temp directory");
        let manifest_store = test_store(&manifest_tempdir);
        manifest_store
            .upsert(&metadata("solver"))
            .expect("initialize store");
        fs::write(manifest_store.manifest_path(), b"{not-json").expect("corrupt metadata manifest");
        let manifest_error = manifest_store.list().expect_err("reject corrupt manifest");
        assert!(manifest_error.to_string().contains("corrupt"));

        let record_tempdir = tempfile::tempdir().expect("create temp directory");
        let record_store = test_store(&record_tempdir);
        let expected = metadata("solver");
        record_store.upsert(&expected).expect("insert record");
        let record_path = record_store.workspace_path("solver");
        fs::write(&record_path, b"{not-json").expect("corrupt metadata record");
        let record_error = record_store
            .get("solver")
            .expect_err("reject corrupt record");
        assert!(record_error.to_string().contains("corrupt"));
        assert!(record_store.upsert(&expected).is_err());
        assert_eq!(fs::read(record_path).expect("record remains"), b"{not-json");
    }

    #[test]
    fn arbitrary_names_map_to_safe_record_filenames() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let store = test_store(&tempdir);
        let names = [
            "../outside",
            "nested/workspace",
            r"windows\\separator",
            "résumé 版本",
            "spaces and punctuation !@#$%",
        ];

        for name in names {
            let expected = metadata(name);
            store.upsert(&expected).expect("write safely named record");
            assert_eq!(
                store.get(name).expect("read safely named record"),
                Some(expected)
            );
            let filename = store
                .workspace_path(name)
                .file_name()
                .expect("record filename")
                .to_string_lossy()
                .into_owned();
            assert!(filename.starts_with("workspace-"));
            assert!(filename.ends_with(".json"));
            assert!(filename.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || character == '-'
                    || character == '.'
            }));
        }
        assert_eq!(
            store.list().expect("list safely named records").len(),
            names.len()
        );
        assert!(store.get("").is_err());
    }

    #[test]
    fn remove_returns_prior_record_and_preserves_absent_store_behavior() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let store = test_store(&tempdir);
        let expected = metadata("solver");

        assert_eq!(store.remove("solver").expect("remove absent record"), None);
        assert!(!store.root().exists());
        store.upsert(&expected).expect("insert record");
        assert_eq!(
            store.remove("solver").expect("remove record"),
            Some(expected)
        );
        assert_eq!(store.get("solver").expect("read removed record"), None);
        assert_eq!(store.remove("solver").expect("remove twice"), None);
    }

    #[test]
    fn insert_and_conditional_remove_preserve_replacement() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let store = test_store(&tempdir);
        let original = metadata("solver");
        store.insert(&original).expect("insert new record");
        assert!(store.insert(&original).is_err());

        let mut replacement = original.clone();
        replacement.creation_operation_id = "operation-2".to_owned();
        store.upsert(&replacement).expect("replace record");
        assert!(
            !store
                .remove_if_matches(&original)
                .expect("retain nonmatching record")
        );
        assert_eq!(
            store.get("solver").expect("read replacement"),
            Some(replacement.clone())
        );
        assert!(
            store
                .remove_if_matches(&replacement)
                .expect("remove matching record")
        );
        assert_eq!(store.get("solver").expect("record removed"), None);
    }

    #[test]
    fn repository_identity_uses_normalized_config_path() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let config_directory = tempdir.path().join("repo");
        fs::create_dir_all(config_directory.join("child"))
            .expect("create config directory and normalization segment");
        let direct =
            WorkspaceMetadataStore::from_repo_config_path(config_directory.join("config.toml"))
                .expect("direct store");
        let equivalent = WorkspaceMetadataStore::from_repo_config_path(
            config_directory.join("child/../config.toml"),
        )
        .expect("normalized store");

        assert_eq!(direct.repository_id(), equivalent.repository_id());
        assert_eq!(direct.root(), equivalent.root());
    }
}
