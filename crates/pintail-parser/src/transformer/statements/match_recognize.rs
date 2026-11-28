// Ported from duckdb/src/parser/peg/transformer/transform_match_recognize.cpp @ rust-port-baseline
//
//! MATCH_RECOGNIZE: row pattern recognition over a table.

use super::super::{Transformer, parser_error, visit_expr_mut};
use crate::ast::*;
use crate::matcher::ParseNode;
use pintail_common::Result;

/// PERMUTE spells out every order of its parts, so the pattern grows with the factorial of the list.
const MAX_PERMUTE_PARTS: usize = 6;

/// `std::next_permutation`: the next arrangement in lexicographic order, or `false` after the last.
fn next_permutation(order: &mut [usize]) -> bool {
    let Some(i) = (1..order.len()).rev().find(|&i| order[i - 1] < order[i]) else {
        return false;
    };
    let j = (i..order.len())
        .rev()
        .find(|&j| order[j] > order[i - 1])
        .expect("a larger element");
    order.swap(i - 1, j);
    order[i..].reverse();
    true
}

fn quantified(child: Expr, min: Option<u64>, max: Option<u64>, excluded: bool, reluctant: bool) -> Expr {
    ExprKind::Quantified {
        child: Box::new(child),
        min,
        max,
        excluded,
        reluctant,
    }
    .into()
}

fn concatenation(mut parts: Vec<Expr>) -> Expr {
    if parts.len() == 1 {
        return parts.pop().unwrap();
    }
    ExprKind::Concatenation(parts).into()
}

impl Transformer<'_> {
    /// `TableMatchRecognizeClause`, applied to `input`.
    pub(in crate::transformer) fn table_match_recognize(
        &mut self,
        node: &ParseNode,
        input: TableRef,
    ) -> Result<TableRef> {
        let mut mr = MatchRecognizeRef {
            input: Box::new(input),
            partitions: Vec::new(),
            orders: Vec::new(),
            measures: Vec::new(),
            rows_per_match: RowsPerMatch::Default,
            after_match: AfterMatchSkip::Default,
            after_match_variable: String::new(),
            pattern: Box::new(ExprKind::Concatenation(Vec::new()).into()),
            subsets: Vec::new(),
            define_auto: false,
            defines: Vec::new(),
        };
        let mut seen: Vec<&'static str> = Vec::new();
        let body = self.req(node, "MatchRecognizeBody")?;
        for clause in self.subs(body, "MatchRecognizeClause") {
            let clause = self.alt(clause)?;
            let kind = match self.name(clause) {
                "MRPartition" => "PARTITION BY",
                "MROrderBy" => "ORDER BY",
                "MRMeasures" => "MEASURES",
                "MRRows" => "ROWS PER MATCH",
                "MRSkip" => "AFTER MATCH SKIP",
                "MRPattern" => "PATTERN",
                "MRSubset" => "SUBSET",
                _ => "DEFINE",
            };
            if seen.contains(&kind) {
                return Err(parser_error(format!("MATCH_RECOGNIZE has more than one {kind} clause")));
            }
            seen.push(kind);
            match self.name(clause) {
                "MRPartition" => mr.partitions = self.exprs(self.req(clause, "WindowPartition")?, "Expression")?,
                "MROrderBy" => mr.orders = self.order_by_clause(self.req(clause, "OrderByClause")?)?,
                "MRMeasures" => {
                    for element in self.subs(self.req(clause, "MeasuresClause")?, "MeasuresElement") {
                        let alias = Self::text(self.req(element, "ColLabelOrString")?);
                        mr.measures
                            .push(self.expr(self.req(element, "Expression")?)?.with_alias(alias));
                    }
                }
                "MRRows" => {
                    let rows = self.alt(self.req(clause, "RowsPerMatch")?)?;
                    mr.rows_per_match = if self.name(rows) == "OneRowPerMatch" {
                        RowsPerMatch::One
                    } else {
                        match self.sub(rows, "AllRowsEmptyMatches") {
                            Some(e) => match self.name(self.alt(e)?) {
                                "OmitEmptyMatches" => RowsPerMatch::AllOmitEmpty,
                                "WithUnmatchedRows" => RowsPerMatch::AllUnmatched,
                                _ => RowsPerMatch::All,
                            },
                            None => RowsPerMatch::All,
                        }
                    };
                }
                "MRSkip" => {
                    let target = self.alt(self.req(self.req(clause, "AfterMatchSkip")?, "AfterMatchSkipTarget")?)?;
                    mr.after_match = match self.name(target) {
                        "SkipToNextRow" => AfterMatchSkip::NextRow,
                        "SkipPastLastRow" => AfterMatchSkip::PastLastRow,
                        "SkipToFirstVar" => AfterMatchSkip::ToFirst,
                        _ => AfterMatchSkip::ToLast,
                    };
                    if let Some(var) = self.sub(target, "ColLabelOrString") {
                        mr.after_match_variable = Self::text(var);
                    }
                }
                "MRPattern" => {
                    let pattern = self.req(self.req(clause, "PatternClause")?, "RowPattern")?;
                    mr.pattern = Box::new(self.row_pattern(pattern)?);
                }
                "MRSubset" => {
                    for element in self.subs(self.req(clause, "SubsetClause")?, "SubsetElement") {
                        let mut names = self.texts(element, "ColLabelOrString");
                        let name = names.remove(0);
                        mr.subsets.push((name, names));
                    }
                }
                "MRDefineAuto" => mr.define_auto = true,
                _ => {
                    for element in self.subs(self.req(clause, "DefineClause")?, "DefineElement") {
                        let alias = Self::text(self.req(element, "ColLabelOrString")?);
                        mr.defines
                            .push(self.expr(self.req(element, "Expression")?)?.with_alias(alias));
                    }
                }
            }
        }
        if !seen.contains(&"PATTERN") {
            return Err(parser_error("MATCH_RECOGNIZE requires a PATTERN clause"));
        }
        let mut first_symbol = None;
        visit_expr_mut(&mut (*mr.pattern).clone(), &mut |e| {
            if let ExprKind::ColumnRef(names) = &e.kind
                && first_symbol.is_none()
            {
                first_symbol = names.last().cloned();
            }
        });
        if let Some(symbol) = first_symbol
            && !seen.contains(&"DEFINE")
        {
            return Err(parser_error(format!(
                "MATCH_RECOGNIZE requires a DEFINE clause. A variable left out of it matches any row, but the \
                 clause itself has to be there: a pattern that constrains nothing writes DEFINE {symbol} AS true"
            )));
        }
        let mut table = TableRef::from(TableRefKind::MatchRecognize(mr));
        if let Some((alias, columns)) = self.table_alias(node)? {
            table.alias = alias;
            table.column_name_alias = columns;
        }
        Ok(table)
    }

    /// `RowPattern <- RowPatternTerm RowPatternAlternative*`: alternation, left-associative.
    fn row_pattern(&mut self, node: &ParseNode) -> Result<Expr> {
        let mut result = self.row_pattern_term(self.req(node, "RowPatternTerm")?)?;
        for alternative in self.subs(node, "RowPatternAlternative") {
            let right = self.row_pattern_term(self.req(alternative, "RowPatternTerm")?)?;
            result = ExprKind::Alternation(Box::new(result), Box::new(right)).into();
        }
        Ok(result)
    }

    fn row_pattern_term(&mut self, node: &ParseNode) -> Result<Expr> {
        let mut factors = Vec::new();
        for factor in self.subs(node, "RowPatternFactor") {
            let primary = self.row_pattern_primary(self.alt(self.req(factor, "RowPatternPrimary")?)?)?;
            let (min, max, reluctant) = match self.sub(factor, "RowPatternQuantifier") {
                Some(q) => self.quantifier(q)?,
                None => (Some(1), Some(1), false),
            };
            factors.push(quantified(primary, min, max, false, reluctant));
        }
        Ok(concatenation(factors))
    }

    fn row_pattern_primary(&mut self, node: &ParseNode) -> Result<Expr> {
        Ok(match self.name(node) {
            "RowPatternPermute" => {
                let mut parts = Vec::new();
                for pattern in self.subs(node, "RowPattern") {
                    parts.push(self.row_pattern(pattern)?);
                }
                if parts.len() > MAX_PERMUTE_PARTS {
                    return Err(parser_error(format!(
                        "PERMUTE takes at most {MAX_PERMUTE_PARTS} parts, because it stands for every order of them"
                    )));
                }
                let mut order: Vec<usize> = (0..parts.len()).collect();
                let mut result: Option<Expr> = None;
                loop {
                    let arrangement = concatenation(order.iter().map(|&i| parts[i].clone()).collect());
                    result = Some(match result {
                        Some(r) => ExprKind::Alternation(Box::new(r), Box::new(arrangement)).into(),
                        None => arrangement,
                    });
                    if !next_permutation(&mut order) {
                        break;
                    }
                }
                result.expect("PERMUTE has parts")
            }
            "RowPatternExclusion" => quantified(
                self.row_pattern(self.req(node, "RowPattern")?)?,
                Some(1),
                Some(1),
                true,
                false,
            ),
            "RowPatternEmpty" => ExprKind::Concatenation(Vec::new()).into(),
            "RowPatternGroup" => self.row_pattern(self.req(node, "RowPattern")?)?,
            "RowPatternAnchor" => ExprKind::Anchor(self.name(self.alt(node)?) == "PatternEnd").into(),
            _ => Expr::column(vec![Self::text(self.req(node, "ColLabelOrString")?)]),
        })
    }

    /// `RowPatternQuantifier`: (min, max, reluctant).
    fn quantifier(&self, node: &ParseNode) -> Result<(Option<u64>, Option<u64>, bool)> {
        let reluctant = self.sub(node, "QuantifierReluctant").is_some();
        let kind = self.alt(self.req(node, "RowPatternQuantifierKind")?)?;
        let count = |n: &ParseNode| -> Result<u64> {
            let literal = Literal::number(Self::text(n));
            match literal.try_get_i64() {
                Some(v) if v >= 0 => Ok(v as u64),
                Some(_) => Err(parser_error("Pattern quantifier bounds must be non-negative integers")),
                None => match literal.text.replace('_', "").parse::<f64>() {
                    Ok(v) if v >= 0.0 && v.round() <= u64::MAX as f64 => Ok(v.round() as u64),
                    _ => Err(parser_error("Pattern quantifier bounds must be non-negative integers")),
                },
            }
        };
        let check_upper = |max: Option<u64>| -> Result<()> {
            if max == Some(0) {
                return Err(parser_error("A pattern quantifier's upper bound must be at least 1"));
            }
            Ok(())
        };
        let (min, max) = match self.name(kind) {
            "QuantifierStar" => (Some(0), None),
            "QuantifierPlus" => (Some(1), None),
            "QuantifierOptional" => (Some(0), Some(1)),
            _ => {
                let bounds = self.alt(self.req(kind, "QuantifierBounds")?)?;
                let numbers = self.subs(bounds, "NumberLiteral");
                let (min, max) = match self.name(bounds) {
                    "QuantifierMinMax" => {
                        let (min, max) = (count(numbers[0])?, count(numbers[1])?);
                        if min > max {
                            return Err(parser_error("Min count cannot be larger than max count"));
                        }
                        (Some(min), Some(max))
                    }
                    "QuantifierMin" => (Some(count(numbers[0])?), None),
                    "QuantifierMax" => (None, Some(count(numbers[0])?)),
                    _ => {
                        let exact = count(numbers[0])?;
                        (Some(exact), Some(exact))
                    }
                };
                check_upper(max)?;
                (min, max)
            }
        };
        Ok((min, max, reluctant))
    }
}

#[cfg(test)]
mod tests {
    use super::next_permutation;

    #[test]
    fn permutations_in_lexicographic_order() {
        let mut order = vec![0, 1, 2];
        let mut all = vec![order.clone()];
        while next_permutation(&mut order) {
            all.push(order.clone());
        }
        assert_eq!(all.len(), 6);
        assert_eq!(all[1], vec![0, 2, 1]);
        assert_eq!(all[5], vec![2, 1, 0]);
    }
}
