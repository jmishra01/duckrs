// Ported from duckdb/src/planner/collation_binding.cpp and the collations of
// duckdb/src/function/function.cpp (`AddCollation`) @ rust-port-baseline
//
//! Collations: a VARCHAR compares, groups and sorts by what its collation functions make of it.
//! `nocase` lowers it and `noaccent` strips its accents (both combinable), `nfc` normalizes it.
//! The ICU collations are not here: naming one fails as an unknown collation.

use crate::planner::expression::BoundExpr;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::{Error, ErrorKind, Result};

/// `CollateCatalogEntry`: a collation's name, its function and whether it combines with others.
const COLLATIONS: [(&str, &str, bool); 3] = [
    ("nocase", "lower", true),
    ("noaccent", "strip_accents", true),
    ("nfc", "nfc_normalize", false),
];

/// Which collations to apply (`CollationType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollationType {
    AllCollations,
    /// only the combinable ones (for functions such as LIKE)
    CombinableCollations,
}

/// `GetVarcharCollationFunctions`: the functions the collation of `ty` (or the default
/// collation) applies, in order. Binary collations apply none.
pub fn collation_functions(ty: &LogicalType, default: &str, kind: CollationType) -> Result<Vec<&'static str>> {
    if ty.id() != LogicalTypeId::Varchar || ty.alias().is_some() {
        return Ok(Vec::new());
    }
    let collation = ty.collation().unwrap_or(default).to_lowercase();
    if collation.is_empty() || matches!(collation.as_str(), "binary" | "c" | "posix") {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(&str, &str, bool)> = Vec::new();
    let mut seen = Vec::new();
    for name in collation.split('.') {
        if seen.contains(&name) {
            continue;
        }
        let Some(&entry) = COLLATIONS.iter().find(|(n, ..)| *n == name) else {
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Collation with name {name} does not exist!"),
            ));
        };
        if entry.2 {
            entries.insert(0, entry);
        } else {
            if let Some(last) = entries.last()
                && !last.2
            {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!("Cannot combine collation types {} and {}", last.0, entry.0),
                ));
            }
            entries.push(entry);
        }
        seen.push(name);
    }
    let mut functions = Vec::new();
    for (_, function, combinable) in entries {
        if !combinable && kind == CollationType::CombinableCollations {
            break;
        }
        functions.push(function);
    }
    Ok(functions)
}

/// `ApplyCollationFunction`: `function(source)`, keeping its alias.
fn apply(function: &str, source: BoundExpr) -> Result<BoundExpr> {
    let alias = source.alias.clone();
    let bound = match crate::function::BuiltinFunctions::get().scalar(function) {
        Some(set) => crate::function::binder::bind_scalar_function(
            set,
            vec![source],
            false,
            &crate::execution::expression_executor::try_evaluate_scalar,
        )?,
        None => crate::function::extras::bind_function(function, &[String::new()], vec![source])?,
    };
    Ok(bound.with_alias(alias))
}

/// `CollationBinding::PushCollation`: `source` with the collation of `ty` applied, and whether
/// there was one to apply.
pub fn push_collation(
    source: BoundExpr,
    ty: &LogicalType,
    default: &str,
    kind: CollationType,
) -> Result<(BoundExpr, bool)> {
    let functions = collation_functions(ty, default, kind)?;
    if functions.is_empty() {
        return Ok((source, false));
    }
    let mut result = source;
    for function in functions {
        result = apply(function, result)?;
    }
    Ok((result, true))
}

/// `ExpressionBinder::TestCollation`: fails if `collation` is not one there is.
pub fn test_collation(collation: &str) -> Result<()> {
    collation_functions(
        &LogicalType::varchar_collation(collation),
        "",
        CollationType::AllCollations,
    )
    .map(|_| ())
}

/// The collation two VARCHARs compare with (`TryBindComparison`): the one either side has, and
/// an error if both have one and they differ.
pub fn combined_collation(left: &LogicalType, right: &LogicalType) -> Result<Option<String>> {
    match (left.collation(), right.collation()) {
        (Some(l), Some(r)) if l != r => Err(Error::new(
            ErrorKind::Binder,
            "Cannot combine types with different collation!",
        )),
        (Some(c), _) | (None, Some(c)) => Ok(Some(c.to_string())),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collations_resolve_to_functions() {
        let all = CollationType::AllCollations;
        let ty = |c: &str| LogicalType::varchar_collation(c);
        assert_eq!(collation_functions(&ty("NOCASE"), "", all).unwrap(), ["lower"]);
        assert_eq!(
            collation_functions(&ty("nfc.noaccent.nocase"), "", all).unwrap(),
            ["lower", "strip_accents", "nfc_normalize"]
        );
        assert_eq!(
            collation_functions(&ty("nfc.nocase"), "", CollationType::CombinableCollations).unwrap(),
            ["lower"]
        );
        assert!(collation_functions(&ty("binary"), "nocase", all).unwrap().is_empty());
        assert_eq!(
            collation_functions(&LogicalType::VARCHAR, "nocase", all).unwrap(),
            ["lower"]
        );
        assert_eq!(
            collation_functions(&ty("de"), "", all).unwrap_err().to_string(),
            "Catalog Error: Collation with name de does not exist!"
        );
        assert!(combined_collation(&ty("nocase"), &ty("noaccent")).is_err());
        assert_eq!(LogicalType::VARCHAR, ty("nocase"));
        assert_eq!(ty("nocase").to_string(), "VARCHAR('nocase')");
    }
}
