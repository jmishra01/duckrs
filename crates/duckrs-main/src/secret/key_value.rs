// Ported from duckdb/src/main/secret/secret.cpp and duckdb/src/include/duckdb/main/secret/secret.hpp
// @ rust-port-baseline
//
//! A secret: its scope, type, provider and name, and the key-value map of a `KeyValueSecret`, with
//! the keys whose values are redacted. Every secret type C++ core registers is a `KeyValueSecret`,
//! so that is the only kind duckrs has.

use duckrs_common::serializer::{BinaryDeserializer, BinarySerializer, Deserialize, Serialize};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use std::collections::{BTreeMap, BTreeSet};

/// `SecretPersistType`: the lifetime of a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretPersistType {
    Default,
    Temporary,
    Persistent,
    Transaction,
}

impl SecretPersistType {
    /// `EnumUtil::ToString`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::Temporary => "TEMPORARY",
            Self::Persistent => "PERSISTENT",
            Self::Transaction => "TRANSACTION",
        }
    }
}

/// `SecretDisplayType`: whether the values of redacted keys are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretDisplayType {
    Redacted,
    Unredacted,
}

/// `KeyValueSecret` (with the members of `BaseSecret`).
#[derive(Debug, Clone)]
pub struct KeyValueSecret {
    /// `prefix_paths`: the prefixes of the paths the secret applies to
    pub scope: Vec<String>,
    pub secret_type: String,
    pub provider: String,
    pub name: String,
    /// `secret_map`, by the lower-case key (`identifier_tree_t`): the key as given and its value
    pub map: BTreeMap<String, (String, Value)>,
    /// `redact_keys`, lower case
    pub redact_keys: BTreeSet<String>,
}

impl KeyValueSecret {
    pub fn new(scope: Vec<String>, secret_type: &str, provider: &str, name: &str) -> Self {
        Self {
            scope,
            secret_type: secret_type.to_string(),
            provider: provider.to_string(),
            name: name.to_string(),
            map: BTreeMap::new(),
            redact_keys: BTreeSet::new(),
        }
    }

    /// `secret_map[key] = value`.
    pub fn set(&mut self, key: &str, value: Value) {
        self.map.insert(key.to_lowercase(), (key.to_string(), value));
    }

    /// `BaseSecret::MatchScore`: the length of the longest prefix of `path` in the scope; 0 for a
    /// secret without a scope or with an empty prefix, which match every path; `None` for no match.
    pub fn match_score(&self, path: &str) -> Option<i64> {
        let mut longest = if self.scope.is_empty() { Some(0) } else { None };
        for prefix in &self.scope {
            if prefix.is_empty() {
                longest = Some(0);
                continue;
            }
            if path.starts_with(prefix.as_str()) {
                longest = Some(longest.map_or(prefix.len() as i64, |l: i64| l.max(prefix.len() as i64)));
            }
        }
        longest
    }

    /// `KeyValueSecret::ToString`: `name=..;type=..;provider=..;serializable=true;scope=a,b;key=value;..`.
    pub fn to_display_string(&self, mode: SecretDisplayType) -> String {
        let mut result = String::new();
        result += &format!("name={};", self.name);
        result += &format!("type={};", self.secret_type);
        result += &format!("provider={};", self.provider);
        result += "serializable=true;";
        result += "scope=";
        for scope in &self.scope {
            result += scope;
            result.push(',');
        }
        // the last character goes: the comma, or the `=` of an empty scope
        result.pop();
        result.push(';');
        let entries: Vec<String> = self
            .map
            .iter()
            .map(|(key, (given, value))| {
                let shown = if mode == SecretDisplayType::Redacted && self.redact_keys.contains(key) {
                    "redacted".to_string()
                } else if value.is_null() {
                    "NULL".to_string()
                } else {
                    value.try_to_string().unwrap_or_default()
                };
                format!("{given}={shown}")
            })
            .collect();
        result += &entries.join(";");
        result
    }

    /// `KeyValueSecret::Serialize`: the base secret, then the map as one MAP(VARCHAR, ANY) value
    /// and the redacted keys as a LIST(VARCHAR).
    pub fn serialize(&self, s: &mut BinarySerializer) -> Result<()> {
        // `BaseSecret::SerializeBaseSecret`
        s.write_property(100, "type", self.secret_type.as_str());
        s.write_property(101, "provider", self.provider.as_str());
        s.write_property(102, "name", self.name.as_str());
        s.write_list(103, "scope", self.scope.len(), |s, i| s.write_str(&self.scope[i]));
        // `Value::Serialize` of the MAP: its entries are STRUCT(key VARCHAR, value ANY), so the value of
        // each entry carries its own type and the key does not
        let map_type = LogicalType::map(LogicalType::VARCHAR, LogicalType::new(LogicalTypeId::Any));
        let entries: Vec<&(String, Value)> = self.map.values().collect();
        s.write_object(201, "secret_map", |s| {
            s.write_property(100, "type", &map_type);
            s.write_property(101, "is_null", &false);
            s.write_object(102, "value", |s| {
                s.write_list(100, "children", entries.len(), |s, i| {
                    let (key, value) = entries[i];
                    s.write_object_value(|s| {
                        s.write_property(101, "is_null", &false);
                        s.write_object(102, "value", |s| {
                            s.write_list(100, "children", 2, |s, child| {
                                if child == 0 {
                                    s.write_object_value(|s| {
                                        s.write_property(101, "is_null", &false);
                                        s.write_property(102, "value", key.as_str());
                                    });
                                } else {
                                    value.write_value(s);
                                }
                            });
                        });
                    });
                });
            });
        });
        let keys = self.redact_keys.iter().map(Value::varchar).collect();
        s.write_property(202, "redact_keys", &Value::list(LogicalType::VARCHAR, keys)?);
        Ok(())
    }

    /// `KeyValueSecret::Deserialize` after the base secret was read: the map and the redacted keys.
    pub fn deserialize_rest(&mut self, d: &mut BinaryDeserializer<'_>) -> Result<()> {
        self.read_secret_map(d)?;
        let redact: Value = d.read_property(202, "redact_keys")?;
        let ValueData::List(keys) = redact.data() else {
            return Err(serialization("Secret redact_keys must be a non-NULL list"));
        };
        if redact.logical_type().id() != LogicalTypeId::List {
            return Err(serialization("Secret redact_keys must be a non-NULL list"));
        }
        for key in keys {
            let key = if key.is_null() {
                "NULL".to_string()
            } else {
                key.try_to_string()?
            };
            self.redact_keys.insert(key.to_lowercase());
        }
        Ok(())
    }

    /// `KeyValueSecret::ReadSecretMap`.
    fn read_secret_map(&mut self, d: &mut BinaryDeserializer<'_>) -> Result<()> {
        d.read_object(201, "secret_map", |d| {
            let ty: LogicalType = d.read_property(100, "type")?;
            // before DuckDB 1.1.3, secrets declared VARCHAR values even when storing other types
            let key_is_varchar = ty
                .child_type()
                .and_then(|entry| entry.struct_children().first())
                .is_some_and(|(_, key)| key.id() == LogicalTypeId::Varchar);
            if ty.id() != LogicalTypeId::Map || !key_is_varchar {
                return Err(serialization("Secret map must be a MAP with VARCHAR keys"));
            }
            let is_null: bool = d.read_property(101, "is_null")?;
            if is_null {
                return Err(serialization("Secret map cannot be NULL"));
            }
            let entries = d.read_object(102, "value", |d| {
                d.read_list(100, "children", |d| {
                    // before DuckDB 1.2.0, entries carried their own types
                    d.read_object_value(|d| {
                        skip_own_type(d)?;
                        let is_null: bool = d.read_property(101, "is_null")?;
                        if is_null {
                            return Err(serialization("Secret map entry must be a non-NULL struct"));
                        }
                        let children = d.read_object(102, "value", |d| {
                            d.read_list(100, "children", |d| {
                                if d.has_property(100)? {
                                    Value::read_value(d).map(Some)
                                } else {
                                    // the key, of the type the map declares
                                    d.read_object_value(|d| {
                                        let is_null: bool = d.read_property(101, "is_null")?;
                                        if is_null {
                                            return Ok(None);
                                        }
                                        let key: String = d.read_property(102, "value")?;
                                        Ok(Some(Value::varchar(key)))
                                    })
                                }
                            })
                        })?;
                        Ok(children)
                    })
                })
            })?;
            for children in entries {
                if children.len() != 2 {
                    return Err(serialization("Secret map entry must have two fields"));
                }
                let key = match &children[0] {
                    Some(key) if !key.is_null() && key.logical_type().id() == LogicalTypeId::Varchar => key,
                    _ => return Err(serialization("Secret map key must be a non-NULL VARCHAR")),
                };
                let Some(value) = &children[1] else {
                    return Err(serialization(
                        "Failed to deserialize value: type ANY is missing its type info",
                    ));
                };
                self.set(&key.try_to_string()?, value.clone());
            }
            Ok(())
        })
    }
}

/// Skips the type an entry of a nested value written before DuckDB 1.2.0 carries.
fn skip_own_type(d: &mut BinaryDeserializer<'_>) -> Result<()> {
    if d.has_property(100)? {
        let _: LogicalType = d.read_property(100, "type")?;
    }
    Ok(())
}

fn serialization(message: &str) -> Error {
    Error::new(ErrorKind::Serialization, message)
}
