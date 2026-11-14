// Ported from duckdb/src/planner/collation_binding.cpp and the collations of
// duckdb/src/function/function.cpp (`AddCollation`) @ rust-port-baseline
//
//! Collations: a VARCHAR compares, groups and sorts by what its collation functions make of it.
//! `nocase` lowers it and `noaccent` strips its accents (both combinable), `nfc` normalizes it.
//! The ICU collations are not here: naming one fails as an unknown collation.

use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, LambdaKind};
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
    // `GetTimeTZCollationFunctions`, `GetBitStringCollationFunctions` and
    // `GetIntervalCollationFunctions`: these compare by what they stand for
    match ty.id() {
        LogicalTypeId::TimeTz => return Ok(vec!["timetz_byte_comparable"]),
        LogicalTypeId::Bit => return Ok(vec!["bitstring_byte_comparable"]),
        LogicalTypeId::Interval => return Ok(vec!["normalized_interval"]),
        // `GetVariantCollationFunctions`
        LogicalTypeId::Variant => return Ok(vec!["variant_comparator"]),
        _ => {}
    }
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
    if function == "variant_comparator" {
        return Ok(crate::function::variant::bind_variant_function(function, vec![source])?.with_alias(alias));
    }
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
/// there was one to apply. The elements of a LIST or ARRAY and the fields of a STRUCT get theirs.
pub fn push_collation(
    source: BoundExpr,
    ty: &LogicalType,
    default: &str,
    kind: CollationType,
) -> Result<(BoundExpr, bool)> {
    if ty.alias().is_none() {
        match ty.id() {
            LogicalTypeId::Struct | LogicalTypeId::Tuple => return push_struct_collation(source, ty, default, kind),
            LogicalTypeId::List | LogicalTypeId::Array => {
                let child = ty.child_type().expect("a child").clone();
                return push_nested_collation(source, &child, default, kind);
            }
            _ => {}
        }
    }
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

/// The element of the list a lambda runs over.
fn lambda_element(ty: &LogicalType) -> BoundExpr {
    BoundExpr::new(BoundExprKind::Reference(0), ty.clone())
}

/// `list_transform(source, x -> body)`.
fn list_transform(source: BoundExpr, body: BoundExpr) -> BoundExpr {
    let ty = LogicalType::list(body.return_type.clone());
    BoundExpr::new(
        BoundExprKind::ListLambda {
            kind: LambdaKind::Transform,
            list: Box::new(source),
            initial: None,
            captures: Vec::new(),
            body: Box::new(body),
        },
        ty,
    )
}

/// `PushNestedCollation`: each element of a LIST or ARRAY with its collation, by a lambda.
fn push_nested_collation(
    source: BoundExpr,
    child: &LogicalType,
    default: &str,
    kind: CollationType,
) -> Result<(BoundExpr, bool)> {
    let (body, collated) = push_collation(lambda_element(child), child, default, kind)?;
    if !collated {
        return Ok((source, false));
    }
    let alias = source.alias.clone();
    Ok((list_transform(source, body).with_alias(alias), true))
}

/// `PushStructCollation`: the struct rebuilt from its fields with their collations, NULL staying
/// NULL. The source is evaluated once, as C++'s `invoke` does, by a lambda over a list of it.
fn push_struct_collation(
    source: BoundExpr,
    ty: &LogicalType,
    default: &str,
    kind: CollationType,
) -> Result<(BoundExpr, bool)> {
    use crate::function::nested::bind_nested_function;
    let element = lambda_element(ty);
    let tuple = ty.id() == LogicalTypeId::Tuple;
    let mut fields = Vec::new();
    let mut names = Vec::new();
    let mut collated = false;
    for (index, (name, field_type)) in ty.struct_children().iter().enumerate() {
        // a field of a named struct by its name, of a tuple by its position
        let key = if tuple {
            duckrs_common::value::Value::bigint(index as i64 + 1)
        } else {
            duckrs_common::value::Value::varchar(name.clone())
        };
        let field = bind_nested_function(
            "struct_extract",
            vec![String::new(); 2],
            vec![element.clone(), BoundExpr::constant(key)],
        )?;
        let (field, changed) = push_collation(field, field_type, default, kind)?;
        collated |= changed;
        fields.push(field.with_alias(name.clone()));
        names.push(if tuple { String::new() } else { name.clone() });
    }
    if !collated {
        return Ok((source, false));
    }
    let rebuilt = bind_nested_function(if tuple { "row" } else { "struct_pack" }, names, fields)?;
    let null = BoundExpr::constant(duckrs_common::value::Value::null(rebuilt.return_type.clone()));
    let is_null = BoundExpr::new(
        BoundExprKind::Operator {
            op: BoundOperator::IsNull,
            children: vec![element],
        },
        LogicalType::BOOLEAN,
    );
    let return_type = rebuilt.return_type.clone();
    let body = BoundExpr::new(
        BoundExprKind::Case {
            checks: vec![(is_null, null)],
            else_expr: Box::new(rebuilt),
        },
        return_type,
    );
    let alias = source.alias.clone();
    let list = bind_nested_function("list_value", vec![String::new()], vec![source])?;
    let one = BoundExpr::constant(duckrs_common::value::Value::bigint(1));
    let result = bind_nested_function(
        "list_extract",
        vec![String::new(); 2],
        vec![list_transform(list, body), one],
    )?;
    Ok((result.with_alias(alias), true))
}

/// `FunctionCollations`: the collation of the VARCHARs of a (nested) type, by field.
#[derive(Debug, Default)]
pub struct FunctionCollations {
    collation: Option<String>,
    fields: Vec<(String, FunctionCollations)>,
}

impl FunctionCollations {
    fn field(&mut self, name: String) -> &mut FunctionCollations {
        let at = match self.fields.iter().position(|(n, _)| n.eq_ignore_ascii_case(&name)) {
            Some(at) => at,
            None => {
                self.fields.push((name, FunctionCollations::default()));
                self.fields.len() - 1
            }
        };
        &mut self.fields[at].1
    }

    fn get(&self, name: &str) -> Option<&FunctionCollations> {
        self.fields
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, f)| f)
    }

    pub fn is_empty(&self) -> bool {
        self.collation.is_none() && self.fields.is_empty()
    }

    /// The collation of the VARCHARs at the top.
    pub fn collation(&self) -> Option<&str> {
        self.collation.as_deref()
    }
}

/// `CollationFieldName`.
fn field_name(ty: &LogicalType, index: usize) -> String {
    if ty.id() == LogicalTypeId::Tuple {
        index.to_string()
    } else {
        ty.struct_children()[index].0.clone()
    }
}

/// `ExtractCollationFromType`: adds the collations of `ty`'s VARCHARs, an error where two differ.
pub fn extract_collations(ty: &LogicalType, collations: &mut FunctionCollations) -> Result<()> {
    if ty.alias().is_some() {
        return Ok(());
    }
    match ty.id() {
        LogicalTypeId::Varchar => match (&collations.collation, ty.collation()) {
            (Some(existing), Some(other)) if existing != other => {
                return Err(Error::new(
                    ErrorKind::Binder,
                    "Cannot combine types with different collation",
                ));
            }
            (None, Some(other)) => collations.collation = Some(other.to_string()),
            _ => {}
        },
        LogicalTypeId::List | LogicalTypeId::Array => {
            extract_collations(ty.child_type().expect("a child"), collations)?;
        }
        LogicalTypeId::Struct | LogicalTypeId::Tuple => {
            for (index, (_, child)) in ty.struct_children().iter().enumerate() {
                extract_collations(child, collations.field(field_name(ty, index)))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `ApplyCollationToType`: `ty` with the collations on its VARCHARs.
pub fn apply_collations(ty: &LogicalType, collations: &FunctionCollations) -> LogicalType {
    if ty.alias().is_some() {
        return ty.clone();
    }
    match ty.id() {
        LogicalTypeId::Varchar => match &collations.collation {
            Some(collation) => LogicalType::varchar_collation(collation),
            None => ty.clone(),
        },
        LogicalTypeId::List => LogicalType::list(apply_collations(ty.child_type().expect("a child"), collations)),
        LogicalTypeId::Array => LogicalType::array(
            apply_collations(ty.child_type().expect("a child"), collations),
            ty.array_size().unwrap_or(0),
        ),
        LogicalTypeId::Struct | LogicalTypeId::Tuple => {
            let children: Vec<(String, LogicalType)> = ty
                .struct_children()
                .iter()
                .enumerate()
                .map(|(index, (name, child))| match collations.get(&field_name(ty, index)) {
                    Some(field) => (name.clone(), apply_collations(child, field)),
                    None => (name.clone(), child.clone()),
                })
                .collect();
            if ty.id() == LogicalTypeId::Tuple {
                LogicalType::tuple(children.into_iter().map(|(_, t)| t).collect())
            } else {
                LogicalType::struct_type(children)
            }
        }
        _ => ty.clone(),
    }
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
