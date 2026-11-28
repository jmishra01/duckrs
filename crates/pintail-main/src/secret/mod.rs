// Ported from duckdb/src/main/secret/secret_manager.cpp, duckdb/src/main/secret/secret_storage.cpp
// and duckdb/src/include/duckdb/main/secret/{secret_manager,secret_storage}.hpp @ rust-port-baseline
//
//! The secret manager of a database (`SecretManager`): the secret types and their providers, and
//! the storages that hold the secrets. `memory` holds temporary secrets, `local_file` persistent
//! ones (a `.duckdb_secret` file each under `secret_directory`) and `connection` the secrets only
//! the connection that made them sees. A lookup takes the secret whose scope is the longest prefix
//! of the path, the storages and the names breaking ties.
//!
//! Differences from C++: the `memory` and `local_file` storages are not catalog sets, so a secret
//! another connection creates inside a transaction is visible to the others before it commits; a
//! rollback still undoes what a transaction did to `memory` and `connection`. Extensions cannot be
//! loaded, so a type or provider of one is the error with the install hint.

pub mod default_secrets;
pub mod key_value;
mod statements;

pub use default_secrets::{CreateSecretFunction, CreateSecretInput, SecretType};
pub use key_value::{KeyValueSecret, SecretDisplayType, SecretPersistType};
pub(crate) use statements::BoundCreateSecret;

use parking_lot::Mutex;
use pintail_common::serializer::{BinaryDeserializer, BinarySerializer, SerializationOptions};
use pintail_common::{Error, ErrorKind, Result};
use pintail_core::execution::file_system::FileAccess;
use pintail_parser::ast::sql_quoted_identifier as quoted;
use std::collections::BTreeMap;
use std::sync::Arc;

/// `SecretManager::TEMPORARY_STORAGE_NAME`.
pub const TEMPORARY_STORAGE_NAME: &str = "memory";
/// `SecretManager::LOCAL_FILE_STORAGE_NAME`.
pub const LOCAL_FILE_STORAGE_NAME: &str = "local_file";
/// `SecretManager::TRANSACTION_STORAGE_NAME`.
pub const TRANSACTION_STORAGE_NAME: &str = "transaction";
/// `SecretManager::CONNECTION_STORAGE_NAME`.
pub const CONNECTION_STORAGE_NAME: &str = "connection";

/// `SecretStorage::*_STORAGE_OFFSET`: lower offsets win ties between storages.
const CONNECTION_STORAGE_OFFSET: i64 = 5;
const TEMPORARY_STORAGE_OFFSET: i64 = 10;
const LOCAL_FILE_STORAGE_OFFSET: i64 = 20;

/// `EXTENSION_SECRET_TYPES`: the secret types extensions register.
const EXTENSION_SECRET_TYPES: [(&str, &str); 14] = [
    ("aws", "httpfs"),
    ("azure", "azure"),
    ("ducklake", "ducklake"),
    ("gcs", "httpfs"),
    ("huggingface", "httpfs"),
    ("iceberg", "iceberg"),
    ("mysql", "mysql_scanner"),
    ("postgres", "postgres_scanner"),
    ("quack", "quack"),
    ("r2", "httpfs"),
    ("rds", "postgres_scanner"),
    ("s3", "httpfs"),
    ("uc", "unity_catalog"),
    ("unity_catalog", "unity_catalog"),
];

/// `EXTENSION_SECRET_PROVIDERS`, by `type/provider` (the `httfps` is C++'s).
const EXTENSION_SECRET_PROVIDERS: [(&str, &str); 19] = [
    ("s3/config", "httpfs"),
    ("gcs/config", "httpfs"),
    ("r2/config", "httpfs"),
    ("s3/credential_chain", "aws"),
    ("gcs/credential_chain", "aws"),
    ("r2/credential_chain", "aws"),
    ("aws/credential_chain", "aws"),
    ("rds/credential_chain", "aws"),
    ("s3/none", "aws"),
    ("aws/none", "aws"),
    ("azure/access_token", "azure"),
    ("azure/config", "azure"),
    ("azure/credential_chain", "azure"),
    ("azure/service_principal", "azure"),
    ("huggingface/config", "httfps"),
    ("huggingface/credential_chain", "httpfs"),
    ("bearer/config", "httpfs"),
    ("mysql/config", "mysql_scanner"),
    ("postgres/config", "postgres_scanner"),
];

/// `ExtensionHelper::FindExtensionInEntries`.
fn find_extension(entries: &[(&str, &'static str)], name: &str) -> Option<&'static str> {
    entries
        .iter()
        .find(|(entry, _)| entry.eq_ignore_ascii_case(name))
        .map(|(_, extension)| *extension)
}

/// `ExtensionHelper::AddExtensionInstallHintToErrorMsg`: pintail loads no extensions, so the hint is
/// the one for an extension that cannot be autoloaded.
fn add_install_hint(message: String, extension: &str) -> String {
    format!(
        "{message}\n\nPlease try installing and loading the {extension} extension:\nINSTALL {extension};\nLOAD \
         {extension};\n\n"
    )
}

fn invalid_input(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn io_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Io, message)
}

/// `SecretEntry`: a secret, where it is stored and for how long.
#[derive(Debug, Clone)]
pub struct SecretEntry {
    pub persist_type: SecretPersistType,
    /// `storage_mode`: the name of the storage
    pub storage_mode: String,
    pub secret: Arc<KeyValueSecret>,
}

/// What the secret manager reads of the configuration when it is used, as
/// `SecretManagerConfig` holds it, and the file access the statement has.
#[derive(Debug, Clone)]
pub struct SecretSettings {
    /// `allow_persistent_secrets`
    pub allow_persistent_secrets: bool,
    /// `secret_directory`, or the default `~/.duckdb/stored_secrets`
    pub secret_path: String,
    /// `default_secret_storage`
    pub default_storage: String,
    /// `allow_unredacted_secrets`
    pub allow_unredacted: bool,
    /// what `enable_external_access` off still allows
    pub file_access: Option<FileAccess>,
    /// `disabled_filesystems` has `LocalFileSystem`
    pub local_file_system_disabled: bool,
}

/// The secrets of the `connection` storage of one connection (`ConnectionSecretState`), by the
/// lower-case name.
pub type ConnectionSecrets = Arc<Mutex<BTreeMap<String, SecretEntry>>>;

/// A storage whose changes a rollback undoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoStorage {
    Memory,
    Connection,
}

/// The pre-image of a secret a transaction changed (`ConnectionSecretState::StageChange`): `None`
/// if there was no secret of the name before.
#[derive(Debug, Clone)]
pub struct SecretUndo {
    pub storage: UndoStorage,
    pub key: String,
    pub before: Option<SecretEntry>,
}

/// `OnCreateConflict`, as far as secrets have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    Error,
    Ignore,
    Replace,
}

/// A secret storage of the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    LocalFile,
    Memory,
    Connection,
}

impl Storage {
    fn name(self) -> &'static str {
        match self {
            Self::LocalFile => LOCAL_FILE_STORAGE_NAME,
            Self::Memory => TEMPORARY_STORAGE_NAME,
            Self::Connection => CONNECTION_STORAGE_NAME,
        }
    }

    fn persistent(self) -> bool {
        self == Self::LocalFile
    }

    fn offset(self) -> i64 {
        match self {
            Self::LocalFile => LOCAL_FILE_STORAGE_OFFSET,
            Self::Memory => TEMPORARY_STORAGE_OFFSET,
            Self::Connection => CONNECTION_STORAGE_OFFSET,
        }
    }
}

/// `SecretManager`.
#[derive(Debug, Default)]
pub struct SecretManager {
    state: Mutex<ManagerState>,
}

#[derive(Debug, Default)]
struct ManagerState {
    initialized: bool,
    /// `TemporarySecretStorage`
    memory: BTreeMap<String, SecretEntry>,
    /// `LocalFileSecretStorage`, when persistent secrets are allowed
    local_file: Option<LocalFileStorage>,
}

/// `SecretStorage::SelectBestMatch`: the better of `entry` and `best` for `path`, by the length of
/// the matching prefix, the storage's offset and then the name.
fn select_best_match(entry: &SecretEntry, path: &str, offset: i64, best: &mut Option<(SecretEntry, i64)>) {
    let Some(score) = entry.secret.match_score(path) else {
        return;
    };
    let score = 100 * score - offset;
    let better = match best {
        None => true,
        Some((current, current_score)) => {
            score > *current_score
                || (score == *current_score && entry.secret.name.to_lowercase() < current.secret.name.to_lowercase())
        }
    };
    if better {
        *best = Some((entry.clone(), score));
    }
}

impl SecretManager {
    /// `SecretManager::ThrowOnSettingChangeIfInitialized`: for `allow_persistent_secrets` and
    /// `secret_directory`.
    pub fn check_setting_change(&self) -> Result<()> {
        if self.state.lock().initialized {
            return Err(invalid_input(
                "Changing Secret Manager settings after the secret manager is used is not allowed!",
            ));
        }
        Ok(())
    }

    /// `SecretManager::AllSecretTypes`.
    pub fn all_secret_types(&self) -> Vec<SecretType> {
        default_secrets::default_secret_types()
    }

    /// `SecretManager::AllSecretFunctions`.
    pub fn all_secret_functions(&self) -> Vec<CreateSecretFunction> {
        default_secrets::default_secret_functions()
    }

    /// `SecretManager::LookupTypeInternal`.
    fn lookup_type(secret_type: &str) -> Result<SecretType> {
        default_secrets::default_secret_types()
            .into_iter()
            .find(|t| t.name.eq_ignore_ascii_case(secret_type))
            .ok_or_else(|| Self::type_not_found_error(secret_type, ""))
    }

    /// `SecretManager::ThrowTypeNotFoundError`.
    fn type_not_found_error(secret_type: &str, secret_path: &str) -> Error {
        let mut message = match find_extension(&EXTENSION_SECRET_TYPES, secret_type) {
            Some(extension) => {
                let mut message = add_install_hint(
                    format!("Secret type '{secret_type}' does not exist, but it exists in the {extension} extension."),
                    extension,
                );
                if !secret_path.is_empty() {
                    message += "\n\nAlternatively, ";
                }
                message
            }
            None => {
                let mut message = format!("Secret type {} not found", quoted(secret_type));
                if !secret_path.is_empty() {
                    message += ", ";
                }
                message
            }
        };
        if !secret_path.is_empty() {
            message += &format!("try removing the secret at path '{secret_path}'.");
        }
        invalid_input(message)
    }

    /// `SecretManager::LookupFunctionInternal`.
    fn lookup_function(secret_type: &str, provider: &str) -> Option<CreateSecretFunction> {
        default_secrets::default_secret_functions()
            .into_iter()
            .find(|function| {
                function.secret_type.eq_ignore_ascii_case(secret_type)
                    && function.provider.eq_ignore_ascii_case(provider)
            })
    }

    /// `SecretManager::ThrowProviderNotFoundError`.
    fn provider_not_found_error(secret_type: &str, provider: &str, was_default: bool) -> Error {
        let key = format!("{secret_type}/{provider}");
        if let Some(extension) = find_extension(&EXTENSION_SECRET_PROVIDERS, &key) {
            let message = format!(
                "{} {} for type {} does not exist, but it exists in the {extension} extension.",
                if was_default {
                    "Default secret provider"
                } else {
                    "Secret provider"
                },
                quoted(provider),
                quoted(secret_type)
            );
            return invalid_input(add_install_hint(message, extension));
        }
        invalid_input(format!(
            "Secret provider {} not found for type {}",
            quoted(provider),
            quoted(secret_type)
        ))
    }

    /// `SecretManager::InitializeSecrets`: the storages, the first time the manager is used. When the
    /// persistent storage cannot be loaded nothing is, and the next use tries again.
    fn initialize(&self, state: &mut ManagerState, settings: &SecretSettings) -> Result<()> {
        if state.initialized {
            return Ok(());
        }
        if settings.allow_persistent_secrets {
            state.local_file = Some(LocalFileStorage::new(&settings.secret_path, settings)?);
        }
        state.initialized = true;
        Ok(())
    }

    /// `SecretManager::GetSecretStorages`, in the order they are listed.
    fn storages(state: &ManagerState) -> Vec<Storage> {
        let mut storages = Vec::new();
        if state.local_file.is_some() {
            storages.push(Storage::LocalFile);
        }
        storages.push(Storage::Memory);
        storages.push(Storage::Connection);
        storages
    }

    /// `SecretManager::GetSecretStorage`.
    fn storage(state: &ManagerState, name: &str) -> Option<Storage> {
        Self::storages(state)
            .into_iter()
            .find(|storage| storage.name().eq_ignore_ascii_case(name))
    }

    /// `SecretManager::BindCreateSecret` and `PhysicalCreateSecret` (`SecretManager::CreateSecret`):
    /// checks the input, makes the secret with its provider and stores it.
    pub fn create_secret(
        &self,
        info: &BoundCreateSecret,
        settings: &SecretSettings,
        connection: &ConnectionSecrets,
    ) -> Result<Option<SecretUndo>> {
        if info.persist_type == SecretPersistType::Transaction
            || info.storage.eq_ignore_ascii_case(TRANSACTION_STORAGE_NAME)
        {
            return Err(Error::new(
                ErrorKind::Binder,
                "Transaction-scoped secrets cannot be created through SQL",
            ));
        }
        let mut state = self.state.lock();
        self.initialize(&mut state, settings)?;
        let (provider, default_provider) = if info.provider.is_empty() {
            (Self::lookup_type(&info.secret_type)?.default_provider.to_string(), true)
        } else {
            (info.provider.clone(), false)
        };
        let Some(function) = Self::lookup_function(&info.secret_type, &provider) else {
            return Err(Self::provider_not_found_error(
                &info.secret_type,
                &info.provider,
                default_provider,
            ));
        };
        // the parameters are cast to the types the provider takes
        let mut options = Vec::new();
        for (name, value) in &info.options {
            let Some((parameter, ty)) = function
                .named_parameters
                .iter()
                .find(|(parameter, _)| parameter.eq_ignore_ascii_case(name))
            else {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!(
                        "Unknown parameter {} for secret type {} with {}provider {}",
                        quoted(name),
                        quoted(&info.secret_type),
                        if default_provider { "default " } else { "" },
                        quoted(&provider)
                    ),
                ));
            };
            let value = pintail_core::function::cast::cast_value(value, ty).map_err(|error| {
                Error::new(
                    ErrorKind::Binder,
                    format!(
                        "Failed to cast option {} to type '{ty}': '{}'",
                        quoted(parameter),
                        error.message
                    ),
                )
            })?;
            options.push((parameter.to_string(), value));
        }
        let input = CreateSecretInput {
            secret_type: info.secret_type.clone(),
            provider,
            name: info.name.clone(),
            scope: info.scope.clone(),
            options,
        };
        let secret = (function.function)(&input);
        Self::register_secret(
            &mut state,
            secret,
            info.on_conflict,
            info.persist_type,
            &info.storage,
            settings,
            connection,
        )
    }

    /// `SecretManager::RegisterSecretInternal`.
    fn register_secret(
        state: &mut ManagerState,
        secret: KeyValueSecret,
        on_conflict: OnConflict,
        mut persist_type: SecretPersistType,
        storage: &str,
        settings: &SecretSettings,
        connection: &ConnectionSecrets,
    ) -> Result<Option<SecretUndo>> {
        Self::lookup_type(&secret.secret_type)?;
        if persist_type == SecretPersistType::Default {
            persist_type = if storage.is_empty() {
                SecretPersistType::Temporary
            } else {
                match Self::storage(state, storage) {
                    Some(backend) if !backend.persistent() => SecretPersistType::Temporary,
                    _ => SecretPersistType::Persistent,
                }
            };
        }
        let resolved = if !storage.is_empty() {
            storage.to_string()
        } else if persist_type == SecretPersistType::Persistent {
            settings.default_storage.clone()
        } else {
            TEMPORARY_STORAGE_NAME.to_string()
        };
        let Some(backend) = Self::storage(state, &resolved) else {
            if !settings.allow_persistent_secrets
                && (persist_type == SecretPersistType::Persistent
                    || storage.eq_ignore_ascii_case(LOCAL_FILE_STORAGE_NAME))
            {
                return Err(invalid_input(
                    "Persistent secrets are disabled. Restart DuckDB and enable persistent secrets through 'SET \
                     allow_persistent_secrets=true'",
                ));
            }
            return Err(invalid_input(format!("Secret storage '{resolved}' not found!")));
        };
        if persist_type == SecretPersistType::Persistent {
            if !backend.persistent() {
                return Err(invalid_input(
                    "Cannot create persistent secrets in a temporary secret storage!",
                ));
            }
            if !settings.allow_persistent_secrets {
                return Err(invalid_input(
                    "Persistent secrets are currently disabled. To enable them, restart duckdb and run 'SET \
                     allow_persistent_secrets=true'",
                ));
            }
        } else if backend.persistent() {
            return Err(invalid_input(
                "Cannot create temporary secrets in a persistent secret storage!",
            ));
        }
        Self::store_secret(state, backend, secret, on_conflict, settings, connection)
    }

    /// `SecretStorage::StoreSecret` of each storage.
    fn store_secret(
        state: &mut ManagerState,
        backend: Storage,
        secret: KeyValueSecret,
        on_conflict: OnConflict,
        settings: &SecretSettings,
        connection: &ConnectionSecrets,
    ) -> Result<Option<SecretUndo>> {
        let key = secret.name.to_lowercase();
        match backend {
            Storage::Connection => {
                let mut secrets = connection.lock();
                let before = secrets.get(&key).cloned();
                if before.is_some() {
                    match on_conflict {
                        OnConflict::Error => {
                            return Err(invalid_input(format!(
                                "Connection secret with name '{}' already exists",
                                secret.name
                            )));
                        }
                        OnConflict::Ignore => return Ok(None),
                        OnConflict::Replace => {}
                    }
                }
                let entry = SecretEntry {
                    persist_type: SecretPersistType::Temporary,
                    storage_mode: CONNECTION_STORAGE_NAME.to_string(),
                    secret: Arc::new(secret),
                };
                secrets.insert(key.clone(), entry);
                Ok(Some(SecretUndo {
                    storage: UndoStorage::Connection,
                    key,
                    before,
                }))
            }
            Storage::Memory | Storage::LocalFile => {
                let persistent = backend.persistent();
                let existing = match backend {
                    Storage::Memory => state.memory.get(&key).cloned(),
                    _ => state.local_file.as_mut().expect("registered").get(&key)?,
                };
                if existing.is_some() {
                    match on_conflict {
                        OnConflict::Error => {
                            return Err(invalid_input(format!(
                                "{} secret with name {} already exists{}!",
                                if persistent { "Persistent" } else { "Temporary" },
                                quoted(&secret.name),
                                if persistent {
                                    format!(" in secret storage '{}'", backend.name())
                                } else {
                                    String::new()
                                }
                            )));
                        }
                        OnConflict::Ignore => return Ok(None),
                        OnConflict::Replace => {}
                    }
                }
                let entry = SecretEntry {
                    persist_type: if persistent {
                        SecretPersistType::Persistent
                    } else {
                        SecretPersistType::Temporary
                    },
                    storage_mode: backend.name().to_string(),
                    secret: Arc::new(secret),
                };
                if backend == Storage::Memory {
                    state.memory.insert(key.clone(), entry);
                    return Ok(Some(SecretUndo {
                        storage: UndoStorage::Memory,
                        key,
                        before: existing,
                    }));
                }
                // the secret is written completely before the catalog changes
                let storage = state.local_file.as_mut().expect("registered");
                storage.write_secret(&entry.secret, settings)?;
                storage.entries.insert(key, entry);
                Ok(None)
            }
        }
    }

    /// `SecretStorage::GetSecretByName`.
    fn get_by_name(
        state: &mut ManagerState,
        storage: Storage,
        key: &str,
        connection: &ConnectionSecrets,
    ) -> Result<Option<SecretEntry>> {
        Ok(match storage {
            Storage::Memory => state.memory.get(key).cloned(),
            Storage::Connection => connection.lock().get(key).cloned(),
            Storage::LocalFile => state.local_file.as_mut().expect("registered").get(key)?,
        })
    }

    /// `SecretManager::DropSecretByName`.
    pub fn drop_secret(
        &self,
        name: &str,
        if_exists: bool,
        persist_type: SecretPersistType,
        storage: &str,
        settings: &SecretSettings,
        connection: &ConnectionSecrets,
    ) -> Result<Option<SecretUndo>> {
        let mut state = self.state.lock();
        self.initialize(&mut state, settings)?;
        // SQL creates no transaction secrets, so there is no transaction storage to drop from
        if storage.eq_ignore_ascii_case(TRANSACTION_STORAGE_NAME) || persist_type == SecretPersistType::Transaction {
            if !if_exists {
                return Err(invalid_input(format!(
                    "Failed to remove non-existent transaction secret with name {}",
                    quoted(name)
                )));
            }
            return Ok(None);
        }
        let key = name.to_lowercase();
        let mut matches = Vec::new();
        if !storage.is_empty() {
            let Some(backend) = Self::storage(&state, storage) else {
                return Err(invalid_input(format!(
                    "Unknown storage type found for drop secret: {}",
                    quoted(storage)
                )));
            };
            matches.push(backend);
        } else {
            for backend in Self::storages(&state) {
                if persist_type == SecretPersistType::Persistent && !backend.persistent() {
                    continue;
                }
                if persist_type == SecretPersistType::Temporary && backend.persistent() {
                    continue;
                }
                if Self::get_by_name(&mut state, backend, &key, connection)?.is_some() {
                    matches.push(backend);
                }
            }
        }
        if matches.len() > 1 {
            let names: Vec<&str> = matches.iter().map(|backend| backend.name()).collect();
            return Err(invalid_input(format!(
                "Ambiguity found for secret name {}, secret occurs in multiple storages: [{}] Please specify which \
                 secret to drop using: 'DROP <PERSISTENT|TEMPORARY> SECRET [FROM <storage>]'.",
                quoted(name),
                names.join(",")
            )));
        }
        let Some(backend) = matches.first().copied() else {
            if !if_exists {
                let storage = if storage.is_empty() {
                    String::new()
                } else {
                    format!(" for storage '{storage}'")
                };
                return Err(invalid_input(format!(
                    "Failed to remove non-existent secret with name {}{storage}",
                    quoted(name)
                )));
            }
            return Ok(None);
        };
        match backend {
            Storage::Connection => {
                let mut secrets = connection.lock();
                match secrets.remove(&key) {
                    Some(before) => Ok(Some(SecretUndo {
                        storage: UndoStorage::Connection,
                        key,
                        before: Some(before),
                    })),
                    None if if_exists => Ok(None),
                    None => Err(invalid_input(format!("Connection secret with name '{name}' not found"))),
                }
            }
            Storage::Memory | Storage::LocalFile => {
                if Self::get_by_name(&mut state, backend, &key, connection)?.is_none() {
                    if if_exists {
                        return Ok(None);
                    }
                    let persistent = backend.persistent();
                    return Err(invalid_input(format!(
                        "Failed to remove non-existent {} secret {}{}",
                        if persistent { "persistent" } else { "temporary" },
                        quoted(name),
                        if persistent {
                            format!(" in secret storage '{}'", backend.name())
                        } else {
                            String::new()
                        }
                    )));
                }
                if backend == Storage::Memory {
                    let before = state.memory.remove(&key);
                    return Ok(Some(SecretUndo {
                        storage: UndoStorage::Memory,
                        key,
                        before,
                    }));
                }
                let storage = state.local_file.as_mut().expect("registered");
                storage.entries.remove(&key);
                storage.remove_secret(name)?;
                Ok(None)
            }
        }
    }

    /// `SecretManager::AllSecrets`.
    pub fn all_secrets(&self, settings: &SecretSettings, connection: &ConnectionSecrets) -> Result<Vec<SecretEntry>> {
        let mut state = self.state.lock();
        self.initialize(&mut state, settings)?;
        let mut result = Vec::new();
        for backend in Self::storages(&state) {
            match backend {
                Storage::LocalFile => {
                    let storage = state.local_file.as_mut().expect("registered");
                    storage.load_all()?;
                    result.extend(storage.entries.values().cloned());
                }
                Storage::Memory => result.extend(state.memory.values().cloned()),
                Storage::Connection => result.extend(connection.lock().values().cloned()),
            }
        }
        Ok(result)
    }

    /// `SecretManager::LookupSecret`: the secret of `secret_type` whose scope matches `path` best.
    pub fn lookup_secret(
        &self,
        path: &str,
        secret_type: &str,
        settings: &SecretSettings,
        connection: &ConnectionSecrets,
    ) -> Result<Option<SecretEntry>> {
        let mut state = self.state.lock();
        self.initialize(&mut state, settings)?;
        let mut best: Option<(SecretEntry, i64)> = None;
        for backend in Self::storages(&state) {
            let entries: Vec<SecretEntry> = match backend {
                Storage::LocalFile => {
                    let storage = state.local_file.as_mut().expect("registered");
                    storage.load_all()?;
                    storage.entries.values().cloned().collect()
                }
                Storage::Memory => state.memory.values().cloned().collect(),
                Storage::Connection => connection.lock().values().cloned().collect(),
            };
            let mut storage_best = None;
            for entry in entries
                .iter()
                .filter(|e| e.secret.secret_type.eq_ignore_ascii_case(secret_type))
            {
                select_best_match(entry, path, backend.offset(), &mut storage_best);
            }
            if let Some((entry, score)) = storage_best
                && best.as_ref().is_none_or(|(_, best_score)| score > *best_score)
            {
                best = Some((entry, score));
            }
        }
        Ok(best.map(|(entry, _)| entry))
    }

    /// Undoes what a transaction that rolls back did to the secrets (`TransactionRollback`).
    pub fn undo(&self, changes: Vec<SecretUndo>, connection: &ConnectionSecrets) {
        let mut state = self.state.lock();
        for change in changes.into_iter().rev() {
            let target = match change.storage {
                UndoStorage::Memory => &mut state.memory,
                UndoStorage::Connection => {
                    let mut secrets = connection.lock();
                    match change.before {
                        Some(before) => secrets.insert(change.key, before),
                        None => secrets.remove(&change.key),
                    };
                    continue;
                }
            };
            match change.before {
                Some(before) => target.insert(change.key, before),
                None => target.remove(&change.key),
            };
        }
    }
}

/// `SECRET_NAME_ESCAPES`: what is percent-encoded in the file name of a secret.
const SECRET_NAME_ESCAPES: [(u8, &str); 6] = [
    (b'%', "%25"),
    (b'/', "%2F"),
    (b'\\', "%5C"),
    (b':', "%3A"),
    (b'\0', "%00"),
    (b'.', "%2E"),
];

/// `LocalFileSecretStorage::SecretFileName`: a dot is only escaped at the start of the name, so the
/// file is never `.`, `..` or hidden.
pub fn secret_file_name(secret_name: &str) -> String {
    let mut result = String::new();
    for (i, c) in secret_name.char_indices() {
        let escape = SECRET_NAME_ESCAPES
            .iter()
            .find(|(byte, _)| c.is_ascii() && c as u8 == *byte && (c != '.' || i == 0));
        match escape {
            Some((_, escaped)) => result += escaped,
            None => result.push(c),
        }
    }
    result + ".duckdb_secret"
}

/// `LocalFileSecretStorage::SecretNameFromFileName`.
pub fn secret_name_from_file_name(file_name: &str) -> String {
    let bytes = file_name.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match SECRET_NAME_ESCAPES
            .iter()
            .find(|(_, escaped)| bytes[i..].starts_with(escaped.as_bytes()))
        {
            Some((byte, _)) => {
                result.push(*byte);
                i += 3;
            }
            None => {
                result.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// `FileSystem::JoinPath`.
fn join_path(directory: &str, name: &str) -> String {
    if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

/// Reads a whole file, as `BufferedFileReader` does, logging it as the file system does.
fn read_logged(path: &str) -> std::io::Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    pintail_core::logging::log_file_system(path, "OPEN", None);
    pintail_core::logging::log_file_system(path, "READ", Some((data.len() as i64, 0)));
    pintail_core::logging::log_file_system(path, "CLOSE", None);
    Ok(data)
}

/// `LocalFileSystem::IsPrivateFile`: no permission for the group or others.
fn is_private_file(path: &str) -> Result<bool> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        io_error(format!(
            "Failed to stat '{path}' when checking file permissions, file may be missing or have incorrect \
             permissions"
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(metadata.permissions().mode() & 0o077 == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(true)
    }
}

/// `UUID::ToString(UUID::GenerateRandomUUID())`, for the temporary file of a secret being written:
/// it only has to differ from the other ones.
fn temporary_suffix() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    hasher.write_u32(std::process::id());
    let high = hasher.finish();
    hasher.write_u64(high);
    let low = hasher.finish();
    let hex = format!("{high:016x}{low:016x}");
    format!(
        "{}-{}-4{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `SecretManager::DeserializeSecret` of a secret file.
fn deserialize_secret(data: &[u8], secret_path: &str) -> Result<KeyValueSecret> {
    let mut d = BinaryDeserializer::new(data);
    d.read_object_value(|d| {
        let secret_type: String = d.read_property(100, "type")?;
        let provider: String = d.read_property(101, "provider")?;
        let name: String = d.read_property(102, "name")?;
        let scope: Vec<String> = d.read_list(103, "scope", |d| d.read_string())?;
        // `SecretSerializationType`: CUSTOM = 0, KEY_VALUE_SECRET = 1
        let serialization_type: u8 = d.read_property_with_explicit_default(104, "serialization_type", 1)?;
        match serialization_type {
            1 => {}
            // a type that serializes itself: every type pintail has is a key-value secret
            0 => {
                SecretManager::lookup_type(&secret_type)
                    .map_err(|_| SecretManager::type_not_found_error(&secret_type, secret_path))?;
            }
            other => {
                return Err(io_error(format!(
                    "Unrecognized secret serialization type found in secret '{secret_path}': {other}"
                )));
            }
        }
        let mut secret = KeyValueSecret::new(scope, &secret_type, &provider, &name);
        secret.deserialize_rest(d)?;
        if secret_type.eq_ignore_ascii_case("http") {
            // older secrets get the current HTTP redaction rules
            secret
                .redact_keys
                .extend(default_secrets::HTTP_REDACTED_KEYS.iter().map(|key| key.to_string()));
        }
        Ok(secret)
    })
}

/// `TryReadSecretName`: the name stored in a secret file, without loading the secret.
fn try_read_secret_name(path: &str) -> Option<String> {
    let data = read_logged(path).ok()?;
    if data.is_empty() {
        return None;
    }
    let mut d = BinaryDeserializer::new(&data);
    let _: String = d.read_property(100, "type").ok()?;
    let _: String = d.read_property(101, "provider").ok()?;
    d.read_property(102, "name").ok()
}

/// `LocalFileSecretStorage`: a file per secret, read when the secret is first used.
#[derive(Debug)]
struct LocalFileStorage {
    /// `secret_path`, expanded
    directory: String,
    /// the secrets read so far, by lower-case name
    entries: BTreeMap<String, SecretEntry>,
    /// `persistent_secrets`: the secrets in files not read yet, by lower-case name
    unloaded: BTreeMap<String, String>,
    /// `PersistentSecretFiles`: the file of each secret, by lower-case name
    files: BTreeMap<String, String>,
}

impl LocalFileStorage {
    /// Lists the secret files of the directory.
    fn new(secret_path: &str, settings: &SecretSettings) -> Result<Self> {
        let directory = pintail_core::execution::file_system::expand_path(secret_path);
        let mut storage = Self {
            directory,
            entries: BTreeMap::new(),
            unloaded: BTreeMap::new(),
            files: BTreeMap::new(),
        };
        // a disabled LocalFileSystem (not all external access) leaves the persistent secrets out
        if settings.local_file_system_disabled {
            return Ok(storage);
        }
        if let Some(access) = &settings.file_access {
            access.check(&storage.directory, true)?;
        }
        let Ok(listing) = std::fs::read_dir(&storage.directory) else {
            return Ok(storage);
        };
        let mut names: Vec<String> = listing
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        for file_name in names {
            let Some(stem) = file_name.strip_suffix(".duckdb_secret") else {
                continue;
            };
            let full_path = join_path(&storage.directory, &file_name);
            let mut secret_name = secret_name_from_file_name(stem);
            if secret_name != stem {
                // the escaped file of a secret, or a file from before names were escaped: the file tells
                if let Some(name) = try_read_secret_name(&full_path) {
                    secret_name = name;
                }
            }
            storage.unloaded.insert(secret_name.to_lowercase(), secret_name.clone());
            storage.files.insert(secret_name.to_lowercase(), full_path);
        }
        Ok(storage)
    }

    /// The secret of the lower-case name, read from its file the first time
    /// (`DefaultSecretGenerator::CreateDefaultEntryInternal`).
    fn get(&mut self, key: &str) -> Result<Option<SecretEntry>> {
        self.load(key)?;
        Ok(self.entries.get(key).cloned())
    }

    fn load_all(&mut self) -> Result<()> {
        let keys: Vec<String> = self.unloaded.keys().cloned().collect();
        for key in keys {
            self.load(&key)?;
        }
        Ok(())
    }

    fn load(&mut self, key: &str) -> Result<()> {
        let Some(entry_name) = self.unloaded.get(key).cloned() else {
            return Ok(());
        };
        let Some(secret_path) = self.files.get(key).cloned() else {
            return Ok(());
        };
        let read = || -> Result<Option<KeyValueSecret>> {
            let data = read_logged(&secret_path)
                .map_err(|error| io_error(format!("Cannot open file \"{secret_path}\": {error}")))?;
            if !is_private_file(&secret_path)? {
                return Err(io_error(format!(
                    "The secret file '{secret_path}' has incorrect permissions! Please set correct permissions or \
                     remove file"
                )));
            }
            if data.is_empty() {
                return Ok(None);
            }
            deserialize_secret(&data, &secret_path).map(Some)
        };
        let secret = match read() {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                return Err(Error::new(
                    ErrorKind::Serialization,
                    format!(
                        "Failed to deserialize secret {} from '{secret_path}': file appears empty! Please remove the \
                         file, restart and try again",
                        quoted(&entry_name)
                    ),
                ));
            }
            Err(error) if error.kind == ErrorKind::Serialization => {
                return Err(Error::new(
                    ErrorKind::Serialization,
                    format!(
                        "Failed to deserialize the persistent secret file: '{secret_path}'. The file maybe be corrupt, \
                         please remove the file, restart and try again. (error message: '{}')",
                        error.message
                    ),
                ));
            }
            Err(error) if error.kind == ErrorKind::Io => {
                return Err(io_error(format!(
                    "Failed to open the persistent secret file: '{secret_path}'. Some other process may have removed \
                     it, please restart and try again. (error message: '{}')",
                    error.message
                )));
            }
            Err(error) => return Err(error),
        };
        // the secret is not a default entry any more, so it can be dropped
        self.unloaded.remove(key);
        let name_key = secret.name.to_lowercase();
        self.files.insert(name_key.clone(), secret_path);
        self.entries.insert(
            name_key,
            SecretEntry {
                persist_type: SecretPersistType::Persistent,
                storage_mode: LOCAL_FILE_STORAGE_NAME.to_string(),
                secret: Arc::new(secret),
            },
        );
        Ok(())
    }

    /// `LocalFileSecretStorage::WriteSecret`: the file is written under a temporary name and moved
    /// over the secret's file, so a crash leaves the old file or the new one.
    fn write_secret(&mut self, secret: &KeyValueSecret, settings: &SecretSettings) -> Result<()> {
        if settings.local_file_system_disabled {
            return Err(Error::new(
                ErrorKind::Permission,
                "File system LocalFileSystem has been disabled by configuration",
            ));
        }
        if let Some(access) = &settings.file_access {
            access.check(&self.directory, true)?;
        }
        if !std::path::Path::new(&self.directory).is_dir() {
            std::fs::create_dir_all(&self.directory).map_err(|error| {
                io_error(format!(
                    "Failed to initialize persistent storage directory. (original error: 'Failed to create directory \
                     \"{}\": {error}')",
                    self.directory
                ))
            })?;
        }
        let key = secret.name.to_lowercase();
        // a replaced secret keeps its file, which can predate escaped names
        let file_path = match self.files.get(&key) {
            Some(path) => path.clone(),
            None => {
                let path = join_path(&self.directory, &secret_file_name(&secret.name));
                if let Some((other, _)) = self.files.iter().find(|(name, other)| **other == path && **name != key) {
                    let other = self
                        .unloaded
                        .get(other)
                        .cloned()
                        .or_else(|| self.entries.get(other).map(|entry| entry.secret.name.clone()));
                    return Err(io_error(format!(
                        "Failed to write the file '{path}' of the secret \"{}\": the file belongs to the secret \"{}\"",
                        secret.name,
                        other.unwrap_or_default()
                    )));
                }
                path
            }
        };
        if let Some(access) = &settings.file_access {
            access.check(&file_path, false)?;
        }
        let mut serializer = BinarySerializer::new(SerializationOptions::default());
        let mut serialized = Ok(());
        serializer.write_object_value(|s| serialized = secret.serialize(s));
        serialized?;
        let bytes = serializer.into_bytes();
        let temp_path = format!("{file_path}.tmp-{}", temporary_suffix());
        let written = write_private_file(&temp_path, &bytes).and_then(|()| {
            std::fs::rename(&temp_path, &file_path).map_err(|error| {
                io_error(format!(
                    "Could not rename file \"{temp_path}\" to \"{file_path}\": {error}"
                ))
            })
        });
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
        self.files.insert(key, file_path);
        Ok(())
    }

    /// `LocalFileSecretStorage::RemoveSecret`.
    fn remove_secret(&mut self, name: &str) -> Result<()> {
        let key = name.to_lowercase();
        let file = self
            .files
            .remove(&key)
            .unwrap_or_else(|| join_path(&self.directory, &secret_file_name(name)));
        self.unloaded.remove(&key);
        std::fs::remove_file(&file).map_err(|error| {
            io_error(format!(
                "Failed to remove secret file '{file}', the file may have been removed by another duckdb instance. \
                 (original error: 'Could not remove file \"{file}\": {error}')"
            ))
        })
    }
}

/// `WriteSecretFileToDisk`: a new file that only its owner can read and write.
fn write_private_file(path: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| io_error(format!("Cannot open file \"{path}\": {error}")))?;
    pintail_core::logging::log_file_system(path, "OPEN", None);
    file.write_all(bytes)
        .map_err(|error| io_error(format!("Could not write file \"{path}\": {error}")))?;
    pintail_core::logging::log_file_system(path, "WRITE", Some((bytes.len() as i64, 0)));
    // a file system can report a failed write only when the file is synced or closed
    file.sync_all()
        .map_err(|error| io_error(format!("Could not fsync file \"{path}\": {error}")))?;
    drop(file);
    pintail_core::logging::log_file_system(path, "CLOSE", None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_round_trip() {
        for name in [
            "../escaped_secret",
            "a/b%c",
            "..\\x",
            ".",
            "..",
            ".hidden",
            "my.secret:1",
            "plain",
        ] {
            let file = secret_file_name(name);
            let stem = file.strip_suffix(".duckdb_secret").unwrap();
            assert!(!stem.contains('/') && !stem.starts_with('.'), "{stem}");
            assert_eq!(secret_name_from_file_name(stem), name);
        }
        assert_eq!(secret_file_name("lit/name"), "lit%2Fname.duckdb_secret");
    }

    #[test]
    fn secret_files_round_trip() {
        use pintail_common::types::LogicalType;
        use pintail_common::value::Value;
        let mut secret = KeyValueSecret::new(vec!["https://a/".into(), "b".into()], "http", "config", "Name");
        secret.set("verify_ssl", Value::boolean(false));
        secret.set("bearer_token", Value::varchar("token"));
        let headers = pintail_core::function::cast::cast_value(
            &Value::varchar("{x=y}"),
            &LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR),
        )
        .unwrap();
        secret.set("extra_http_headers", headers);
        secret.redact_keys.insert("bearer_token".into());
        let mut serializer = BinarySerializer::new(SerializationOptions::default());
        serializer.write_object_value(|s| secret.serialize(s).unwrap());
        let read = deserialize_secret(&serializer.into_bytes(), "path").unwrap();
        assert_eq!(
            read.to_display_string(SecretDisplayType::Unredacted),
            secret.to_display_string(SecretDisplayType::Unredacted)
        );
        // the HTTP keys are redacted when read back, whatever the file says
        assert_eq!(
            read.to_display_string(SecretDisplayType::Redacted),
            "name=Name;type=http;provider=config;serializable=true;scope=https://a/,b;bearer_token=redacted;\
             extra_http_headers=redacted;verify_ssl=false"
        );
    }
}
