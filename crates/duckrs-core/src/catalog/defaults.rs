// Ported from duckdb/src/catalog/default/default_{views,functions,table_functions}.cpp
// @ rust-port-baseline
//
//! The entries of the system catalog that are SQL: the views of `information_schema`,
//! `pg_catalog` and `main` (`sqlite_master`, ...), and the macros of `pg_catalog` and `main`
//! (`pg_typeof`, `current_user`, ...). C++ makes them when they are first looked up; here they
//! are found when nothing else has the name.

use super::MacroCatalogEntry;
use super::defaults_table::{MACROS, TABLE_MACROS, VIEWS};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// The schemas a name without one is looked up in, in order (the default search path).
const SEARCH_PATH: [&str; 2] = ["main", "pg_catalog"];

/// The schemas `schema` stands for: itself, or the search path when there is none.
fn schemas(schema: Option<&str>) -> Vec<String> {
    match schema {
        Some(schema) => vec![schema.to_lowercase()],
        None => SEARCH_PATH.iter().map(|s| s.to_string()).collect(),
    }
}

/// `DefaultViewGenerator`: the query of the system view `schema.name`, if there is one.
pub fn default_view(schema: Option<&str>, name: &str) -> Option<&'static str> {
    let name = name.to_lowercase();
    schemas(schema)
        .iter()
        .find_map(|s| VIEWS.iter().find(|(vs, vn, _)| vs == s && *vn == name))
        .map(|(_, _, sql)| *sql)
}

/// Whether `schema` is a schema of the system catalog that holds default entries.
pub fn is_default_schema(schema: &str) -> bool {
    matches!(schema.to_lowercase().as_str(), "pg_catalog" | "information_schema")
}

/// The system macros made so far, by (schema, name, whether a table macro), with `None` for a
/// name that is not one.
type MacroCache = HashMap<(String, String, bool), Option<Arc<MacroCatalogEntry>>>;

/// `DefaultFunctionGenerator` and `DefaultTableFunctionGenerator`: the system macro
/// `schema.name`, made from its SQL the first time it is asked for.
pub fn default_macro(schema: Option<&str>, name: &str, table_macro: bool) -> Option<Arc<MacroCatalogEntry>> {
    static CACHE: OnceLock<Mutex<MacroCache>> = OnceLock::new();
    let name = name.to_lowercase();
    for schema in schemas(schema) {
        let key = (schema.clone(), name.clone(), table_macro);
        let mut cache = CACHE.get_or_init(Mutex::default).lock().expect("not poisoned");
        if let Some(entry) = cache.get(&key) {
            if entry.is_some() {
                return entry.clone();
            }
            continue;
        }
        let entry = build_macro(&schema, &name, table_macro);
        cache.insert(key, entry.clone());
        if entry.is_some() {
            return entry;
        }
    }
    None
}

/// The system macros, in order: (schema, name, whether a table macro).
pub fn default_macro_names() -> impl Iterator<Item = (&'static str, &'static str, bool)> {
    let scalar = MACROS.iter().map(|(schema, name, _)| (*schema, *name, false));
    scalar.chain(TABLE_MACROS.iter().map(|(schema, name, ..)| (*schema, *name, true)))
}

/// `CreateInternalMacroInfo`: the macro from its definition.
fn build_macro(schema: &str, name: &str, table_macro: bool) -> Option<Arc<MacroCatalogEntry>> {
    let sql = if table_macro {
        let (_, _, parameters, named, body) = TABLE_MACROS.iter().find(|(s, n, ..)| *s == schema && *n == name)?;
        let mut all: Vec<String> = parameters.iter().map(|p| p.to_string()).collect();
        all.extend(named.iter().map(|(p, default)| format!("{p} := {default}")));
        format!("CREATE MACRO \"{name}\"({}) AS TABLE {body}", all.join(", "))
    } else {
        let (_, _, body) = MACROS.iter().find(|(s, n, _)| *s == schema && *n == name)?;
        format!("CREATE MACRO \"{name}\"{body}")
    };
    let statements = duckrs_parser::parse_ast(&sql).ok()?;
    let [duckrs_parser::ast::Statement::Create(info)] = statements.as_slice() else {
        return None;
    };
    let duckrs_parser::ast::CreateKind::Macro { table_macro, macros } = &info.kind else {
        return None;
    };
    Some(Arc::new(MacroCatalogEntry {
        name: name.to_string(),
        table_macro: *table_macro,
        macros: macros.clone(),
        comment: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve() {
        assert!(default_view(Some("information_schema"), "COLUMNS").is_some());
        assert!(default_view(None, "pg_namespace").is_some());
        assert!(default_view(None, "columns").is_none());
        // every macro and table macro parses
        for (schema, name, _) in MACROS {
            assert!(default_macro(Some(schema), name, false).is_some(), "{schema}.{name}");
        }
        for (schema, name, ..) in TABLE_MACROS {
            assert!(default_macro(Some(schema), name, true).is_some(), "{schema}.{name}");
        }
        assert!(default_macro(None, "pg_typeof", false).is_some());
    }
}
