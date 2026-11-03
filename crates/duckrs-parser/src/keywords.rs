// Ported from duckdb/src/parser/peg/keyword_helper/default_keyword_maps.cpp and the keyword maps
// that scripts/parser/inline_grammar.py generates @ rust-port-baseline

use std::collections::HashMap;
use std::sync::OnceLock;

/// Keyword category bits (`KEYWORD_*` in default_keyword_maps.cpp).
pub mod category {
    pub const UNRESERVED: u8 = 1 << 0;
    pub const RESERVED: u8 = 1 << 1;
    /// Function-name or type-name keywords (the union of both lists).
    pub const TYPE_FUNC: u8 = 1 << 2;
    pub const COL_NAME: u8 = 1 << 3;
    pub const TYPE_NAME: u8 = 1 << 4;
    pub const BARE_LABEL: u8 = 1 << 5;
}

/// Which keywords an identifier position accepts (`GetIdentifierMask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierKind {
    /// Most names: unreserved and column-name keywords are allowed.
    General,
    TypeName,
    FunctionName,
    /// A column alias without `AS`.
    BareLabel,
}

impl IdentifierKind {
    pub fn mask(self) -> u8 {
        use category::*;
        match self {
            Self::BareLabel => BARE_LABEL,
            Self::TypeName => UNRESERVED | TYPE_NAME,
            Self::FunctionName => UNRESERVED | TYPE_FUNC,
            Self::General => UNRESERVED | COL_NAME,
        }
    }
}

const LISTS: [(&str, u8); 6] = [
    (
        include_str!("../grammar/keywords/reserved_keyword.list"),
        category::RESERVED,
    ),
    (
        include_str!("../grammar/keywords/unreserved_keyword.list"),
        category::UNRESERVED,
    ),
    (
        include_str!("../grammar/keywords/column_name_keyword.list"),
        category::COL_NAME,
    ),
    (
        include_str!("../grammar/keywords/func_name_keyword.list"),
        category::TYPE_FUNC,
    ),
    (
        include_str!("../grammar/keywords/type_name_keyword.list"),
        category::TYPE_FUNC | category::TYPE_NAME,
    ),
    (
        include_str!("../grammar/keywords/bare_label_keyword.list"),
        category::BARE_LABEL,
    ),
];

fn table() -> &'static HashMap<String, u8> {
    static TABLE: OnceLock<HashMap<String, u8>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map: HashMap<String, u8> = HashMap::new();
        for (list, bits) in LISTS {
            for word in list.lines().map(str::trim).filter(|w| !w.is_empty()) {
                *map.entry(word.to_ascii_lowercase()).or_default() |= bits;
            }
        }
        map
    })
}

/// The category bits of `word` (0 if it is not a keyword).
pub fn categories(word: &str) -> u8 {
    if word.len() > 32 {
        return 0;
    }
    table().get(&word.to_ascii_lowercase()).copied().unwrap_or(0)
}

pub fn is_keyword(word: &str) -> bool {
    categories(word) != 0
}

pub fn is_reserved(word: &str) -> bool {
    categories(word) & category::RESERVED != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_from_lists() {
        assert!(is_reserved("SELECT") && is_reserved("select"));
        assert_eq!(categories("abort") & category::UNRESERVED, category::UNRESERVED);
        assert_ne!(categories("asof") & category::TYPE_NAME, 0);
        assert!(!is_keyword("my_table"));
        assert!(IdentifierKind::General.mask() & categories("abort") != 0);
        assert!(IdentifierKind::General.mask() & categories("select") == 0);
    }
}
