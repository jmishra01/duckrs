// Ported from the SECRET_ENTRY cases of duckdb/src/planner/binder/statement/bind_create.cpp and
// bind_drop.cpp, duckdb/src/execution/operator/helper/physical_create_secret.cpp, the SECRET_ENTRY
// case of duckdb/src/execution/operator/schema/physical_drop.cpp and the scans of
// duckdb/src/function/table/system/duckdb_{secrets,secret_types,secret_type_parameters,
// which_secret}.cpp @ rust-port-baseline
//
//! `CREATE SECRET`, `DROP SECRET`, and what the secret table functions read.

use super::{ConnectionSecrets, OnConflict, SecretDisplayType, SecretPersistType, SecretSettings, SecretUndo};
use crate::{ClientContext, DatabaseInstance, MaterializedQueryResult, settings};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::{LogicalType, LogicalTypeId, PhysicalType};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_core::execution::expression_executor::evaluate_scalar;
use duckrs_core::planner::binder::{Binder, SecretTables};
use duckrs_parser::ast::{CreateInfo, CreateKind, DropInfo, Expr, OnCreateConflict};
use std::sync::Arc;

/// `CreateSecretInput` as `Binder::BindCreateSecret` makes it: every expression evaluated.
#[derive(Debug, Clone)]
pub struct BoundCreateSecret {
    pub secret_type: String,
    pub provider: String,
    /// `storage_type`: the storage `IN` names, lower case
    pub storage: String,
    pub name: String,
    pub scope: Vec<String>,
    pub options: Vec<(String, Value)>,
    pub on_conflict: OnConflict,
    pub persist_type: SecretPersistType,
}

/// The secret manager as a statement of one connection sees it: what the secret table functions
/// of the binder read.
pub struct SecretView {
    pub db: Arc<DatabaseInstance>,
    pub settings: SecretSettings,
    pub connection: ConnectionSecrets,
}

impl SecretTables for SecretView {
    fn secrets(&self, redact: bool) -> Result<Vec<Vec<Value>>> {
        if !redact && !self.settings.allow_unredacted {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Displaying unredacted secrets is disabled",
            ));
        }
        let mode = if redact {
            SecretDisplayType::Redacted
        } else {
            SecretDisplayType::Unredacted
        };
        let entries = self.db.secrets.all_secrets(&self.settings, &self.connection)?;
        entries
            .iter()
            .map(|entry| {
                let secret = &entry.secret;
                let scope = secret.scope.iter().map(Value::varchar).collect();
                Ok(vec![
                    Value::varchar(&secret.name),
                    Value::varchar(&secret.secret_type),
                    Value::varchar(&secret.provider),
                    Value::boolean(entry.persist_type == SecretPersistType::Persistent),
                    Value::varchar(&entry.storage_mode),
                    Value::list(LogicalType::VARCHAR, scope)?,
                    Value::varchar(secret.to_display_string(mode)),
                ])
            })
            .collect()
    }

    fn secret_types(&self) -> Result<Vec<Vec<Value>>> {
        Ok(self
            .db
            .secrets
            .all_secret_types()
            .iter()
            .map(|secret_type| {
                vec![
                    Value::varchar(secret_type.name),
                    Value::varchar(secret_type.default_provider),
                    Value::varchar(secret_type.extension),
                ]
            })
            .collect())
    }

    fn secret_type_parameters(&self) -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        for function in self.db.secrets.all_secret_functions() {
            for (name, ty) in &function.named_parameters {
                rows.push(vec![
                    Value::varchar(function.secret_type),
                    Value::varchar(function.provider),
                    Value::varchar(*name),
                    Value::varchar(ty.to_string()),
                    Value::null(LogicalType::VARCHAR),
                ]);
            }
        }
        Ok(rows)
    }

    fn which_secret(&self, path: &str, secret_type: &str) -> Result<Option<Vec<Value>>> {
        let found =
            self.db
                .secrets
                .lookup_secret(path, &secret_type.to_lowercase(), &self.settings, &self.connection)?;
        Ok(found.map(|entry| {
            vec![
                Value::varchar(&entry.secret.name),
                Value::varchar(entry.persist_type.name()),
                Value::varchar(&entry.storage_mode),
            ]
        }))
    }
}

impl ClientContext {
    /// What the secret manager reads of the settings (`SecretManagerConfig`).
    pub(crate) fn secret_settings(&self) -> SecretSettings {
        let flag = |name: &str, default: bool| match self.setting(name).as_ref().map(Value::data) {
            Some(ValueData::Bool(value)) => *value,
            _ => default,
        };
        let mut secret_path = self.setting_text("secret_directory");
        if secret_path.is_empty() {
            // `SecretManager::Initialize`: the default path
            let home = duckrs_core::execution::file_system::home_directory();
            secret_path = format!("{}/.duckdb/stored_secrets", home.trim_end_matches('/'));
        }
        let mut default_storage = self.setting_text("default_secret_storage");
        if default_storage.is_empty() {
            default_storage = super::LOCAL_FILE_STORAGE_NAME.to_string();
        }
        SecretSettings {
            allow_persistent_secrets: flag("allow_persistent_secrets", true),
            secret_path,
            default_storage,
            allow_unredacted: flag("allow_unredacted_secrets", false),
            file_access: self.file_access(),
            local_file_system_disabled: self
                .setting_text("disabled_filesystems")
                .split(',')
                .any(|name| name.trim() == "LocalFileSystem"),
        }
    }

    /// What the secret table functions of a statement read.
    pub(crate) fn secret_view(&self) -> Arc<dyn SecretTables> {
        Arc::new(SecretView {
            db: self.home.clone(),
            settings: self.secret_settings(),
            connection: self.connection_secrets.clone(),
        })
    }

    /// `SET` or `RESET` of a setting the secret manager reads when it starts
    /// (`SetEnablePersistentSecrets`, `SetPersistentSecretPath`).
    pub(crate) fn check_secret_setting_change(&self, name: &str) -> Result<()> {
        let canonical = settings::find_setting(name).map(|(_, setting)| setting.name);
        if matches!(canonical, Some("allow_persistent_secrets" | "secret_directory")) {
            self.home.secrets.check_setting_change()?;
        }
        Ok(())
    }

    /// Evaluates an expression of `CREATE SECRET` (`ConstantBinder(.., "Secret Parameter")` +
    /// `ExpressionExecutor::EvaluateScalar`).
    fn secret_parameter(&self, expr: &Expr) -> Result<Value> {
        if duckrs_core::planner::binder::has_subquery(expr) {
            return Err(Error::new(
                ErrorKind::Binder,
                "Secret Parameter cannot contain subqueries",
            ));
        }
        let catalog = self.catalog();
        let mut binder = Binder::with_options(&catalog, self.bind_options());
        binder.session = Some(self.session_values());
        let bound = binder.bind_value_expression(expr).map_err(|error| {
            if error
                .message
                .contains("was not found because the FROM clause is missing")
            {
                Error::new(ErrorKind::Binder, "Secret Parameter cannot contain column names")
            } else {
                error
            }
        })?;
        evaluate_scalar(&bound)
    }

    /// `Binder::Bind(CreateStatement)` of a secret: the type, provider, scope and options
    /// evaluated.
    fn bind_create_secret(&self, info: &CreateInfo) -> Result<BoundCreateSecret> {
        let CreateKind::Secret {
            persistent,
            storage,
            provider,
            secret_type,
            scope,
            options,
        } = &info.kind
        else {
            unreachable!("a CREATE SECRET");
        };
        let text = |value: &Value| -> Result<String> {
            if value.is_null() {
                Ok("NULL".to_string())
            } else {
                value.try_to_string()
            }
        };
        let provider = match provider {
            Some(provider) => text(&self.secret_parameter(provider)?)?.to_lowercase(),
            None => String::new(),
        };
        let secret_type = match secret_type {
            Some(secret_type) => text(&self.secret_parameter(secret_type)?)?.to_lowercase(),
            None => String::new(),
        };
        let mut scopes = Vec::new();
        if let Some(scope) = scope {
            let value = self.secret_parameter(scope)?;
            let ty = value.logical_type();
            match (ty.id(), value.data()) {
                (LogicalTypeId::Varchar, _) => scopes.push(text(&value)?),
                (LogicalTypeId::List, data)
                    if ty
                        .child_type()
                        .is_some_and(|child| child.id() == LogicalTypeId::Varchar) =>
                {
                    if let ValueData::List(items) = data {
                        for item in items {
                            scopes.push(text(item)?);
                        }
                    }
                }
                // a STRUCT of VARCHARs, as `SCOPE ('a', 'b')` was parsed before scopes were expressions
                (_, data) if ty.physical_type() == PhysicalType::Struct => {
                    if let ValueData::Struct(children) = data {
                        for child in children {
                            if child.logical_type().id() != LogicalTypeId::Varchar {
                                return Err(Error::new(
                                    ErrorKind::InvalidInput,
                                    "Invalid input to scope parameter of create secret: only struct of VARCHARs is \
                                     allowed",
                                ));
                            }
                            scopes.push(text(child)?);
                        }
                    }
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Create Secret scope must be of type VARCHAR or LIST(VARCHAR)",
                    ));
                }
            }
        }
        let mut bound_options = Vec::new();
        for (name, expr) in options {
            bound_options.push((name.clone(), self.secret_parameter(expr)?));
        }
        Ok(BoundCreateSecret {
            secret_type,
            provider,
            storage: storage.to_lowercase(),
            name: info.name.name.clone(),
            scope: scopes,
            options: bound_options,
            on_conflict: match info.on_conflict {
                OnCreateConflict::Error => OnConflict::Error,
                OnCreateConflict::Ignore => OnConflict::Ignore,
                OnCreateConflict::Replace => OnConflict::Replace,
            },
            persist_type: if *persistent {
                SecretPersistType::Persistent
            } else if info.temporary {
                SecretPersistType::Temporary
            } else {
                SecretPersistType::Default
            },
        })
    }

    /// Keeps what a statement inside a transaction changed of the secrets, for a rollback.
    fn note_secret_change(&mut self, change: Option<SecretUndo>) {
        let (Some(change), Some(transaction)) = (change, &mut self.transaction) else {
            return;
        };
        // only the first change of a secret matters: it has the state from before the transaction
        if !transaction
            .secret_undo
            .iter()
            .any(|undo| undo.storage == change.storage && undo.key == change.key)
        {
            transaction.secret_undo.push(change);
        }
    }

    /// `CREATE SECRET`: binds the statement and runs `PhysicalCreateSecret`; the result is a row
    /// with `true`.
    pub(crate) fn create_secret(&mut self, info: &CreateInfo) -> Result<MaterializedQueryResult> {
        let bound = self.bind_create_secret(info)?;
        let settings = self.secret_settings();
        let change = self
            .home
            .secrets
            .create_secret(&bound, &settings, &self.connection_secrets)?;
        self.note_secret_change(change);
        let types = vec![LogicalType::BOOLEAN];
        let mut collection = ColumnDataCollection::new(types.clone());
        let mut chunk = duckrs_common::vector::DataChunk::new(&types);
        chunk.append_row(&[Value::boolean(true)])?;
        collection.append(&chunk);
        Ok(MaterializedQueryResult {
            names: vec!["Success".to_string()],
            types,
            collection,
        })
    }

    /// `DROP SECRET` (`PhysicalDrop` of a `SECRET_ENTRY`).
    pub(crate) fn drop_secret(&mut self, info: &DropInfo) -> Result<()> {
        let persist_type = match info.secret_persist {
            Some("PERSISTENT") => SecretPersistType::Persistent,
            Some(_) => SecretPersistType::Temporary,
            None => SecretPersistType::Default,
        };
        let settings = self.secret_settings();
        let change = self.home.secrets.drop_secret(
            &info.name.name,
            info.if_exists,
            persist_type,
            &info.secret_storage,
            &settings,
            &self.connection_secrets,
        )?;
        self.note_secret_change(change);
        Ok(())
    }

    /// Undoes what the transaction did to the secrets.
    pub(crate) fn undo_secret_changes(&self, changes: Vec<SecretUndo>) {
        if !changes.is_empty() {
            self.home.secrets.undo(changes, &self.connection_secrets);
        }
    }
}
