// Ported from duckdb/src/main/secret/default_secrets.cpp and
// duckdb/src/include/duckdb/main/secret/default_secrets.hpp @ rust-port-baseline
//
//! The secret types and providers C++ core registers itself: the `http` type, with the `config`
//! and `env` providers. Every other type (S3, Azure, Hugging Face, ...) lives in an extension.

use super::key_value::KeyValueSecret;
use pintail_common::types::LogicalType;
use pintail_common::value::Value;

/// `SecretType`: a type of secret, and the provider used when none is given.
#[derive(Debug, Clone)]
pub struct SecretType {
    pub name: &'static str,
    pub default_provider: &'static str,
    /// the extension that registered it; empty for core
    pub extension: &'static str,
}

/// `CreateSecretInput`: what a provider makes a secret from.
#[derive(Debug, Clone)]
pub struct CreateSecretInput {
    pub secret_type: String,
    pub provider: String,
    pub name: String,
    pub scope: Vec<String>,
    /// by the lower-case option name
    pub options: Vec<(String, Value)>,
}

impl CreateSecretInput {
    fn option(&self, key: &str) -> Option<&Value> {
        self.options
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }
}

/// `CreateSecretFunction`: a provider of a secret type, with the parameters it takes.
#[derive(Debug, Clone)]
pub struct CreateSecretFunction {
    pub secret_type: &'static str,
    pub provider: &'static str,
    pub function: fn(&CreateSecretInput) -> KeyValueSecret,
    /// `named_parameters`, in the order they are registered
    pub named_parameters: Vec<(&'static str, LogicalType)>,
}

/// `CreateHTTPSecretFunctions::GetRedactedKeys`.
pub const HTTP_REDACTED_KEYS: [&str; 3] = ["http_proxy_password", "bearer_token", "extra_http_headers"];

/// The options both HTTP providers take, in the order the secret is filled from them.
const HTTP_OPTIONS: [&str; 6] = [
    "verify_ssl",
    "http_proxy",
    "http_proxy_password",
    "http_proxy_username",
    "extra_http_headers",
    "bearer_token",
];

/// `CreateHTTPSecretFunctions::GetDefaultSecretTypes`.
pub fn default_secret_types() -> Vec<SecretType> {
    vec![SecretType {
        name: "http",
        default_provider: "config",
        extension: "",
    }]
}

/// `CreateHTTPSecretFunctions::GetDefaultSecretFunctions`.
pub fn default_secret_functions() -> Vec<CreateSecretFunction> {
    let parameters = || {
        vec![
            ("verify_ssl", LogicalType::BOOLEAN),
            ("http_proxy", LogicalType::VARCHAR),
            ("http_proxy_password", LogicalType::VARCHAR),
            ("http_proxy_username", LogicalType::VARCHAR),
            (
                "extra_http_headers",
                LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR),
            ),
            ("bearer_token", LogicalType::VARCHAR),
        ]
    };
    vec![
        CreateSecretFunction {
            secret_type: "http",
            provider: "config",
            function: create_http_secret_from_config,
            named_parameters: parameters(),
        },
        CreateSecretFunction {
            secret_type: "http",
            provider: "env",
            function: create_http_secret_from_env,
            named_parameters: parameters(),
        },
    ]
}

/// `TryGetEnv`: the variable, or else its upper-case name.
fn try_get_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .or_else(|| std::env::var(name.to_uppercase()).ok())
}

/// `KeyValueSecret::TrySetValue` of every HTTP option given, then the redacted keys.
fn fill_http_secret(secret: &mut KeyValueSecret, input: &CreateSecretInput) {
    for key in HTTP_OPTIONS {
        if let Some(value) = input.option(key) {
            secret.set(key, value.clone());
        }
    }
    secret.redact_keys = HTTP_REDACTED_KEYS.iter().map(|key| key.to_string()).collect();
}

/// `CreateHTTPSecretFunctions::CreateHTTPSecretFromEnv`: the proxy settings of the environment,
/// which the options given override.
fn create_http_secret_from_env(input: &CreateSecretInput) -> KeyValueSecret {
    let mut secret = KeyValueSecret::new(input.scope.clone(), &input.secret_type, &input.provider, &input.name);
    for key in ["http_proxy", "http_proxy_password", "http_proxy_username"] {
        if let Some(value) = try_get_env(key) {
            secret.set(key, Value::varchar(value));
        }
    }
    fill_http_secret(&mut secret, input);
    secret
}

/// `CreateHTTPSecretFunctions::CreateHTTPSecretFromConfig`.
fn create_http_secret_from_config(input: &CreateSecretInput) -> KeyValueSecret {
    let mut secret = KeyValueSecret::new(input.scope.clone(), &input.secret_type, &input.provider, &input.name);
    fill_http_secret(&mut secret, input);
    secret
}
