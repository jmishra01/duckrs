// Ported from duckdb/src/main/client_context.cpp (statement routing) and
// duckdb/src/catalog/catalog_entry/trigger_catalog_entry.cpp @ rust-port-baseline
//
//! The names of the tables a parsed statement refers to: for routing a statement to the database
//! it reads, and for the dependencies of a trigger body. They live here rather than in `client`
//! so that storage (checkpoints write trigger dependencies) doesn't depend on the client.

use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{CreateKind, Expr, ExprKind, QualifiedName, QueryNode, QueryNodeKind, Statement, TableRef};

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

/// The names of the tables and other entries that a statement refers to directly, for routing it
/// to the database they are in. Subqueries in expressions aren't looked at.
pub fn for_each_table_name(statement: &mut Statement, f: &mut impl FnMut(&mut QualifiedName)) {
    fn in_table(table: &mut duckrs_parser::ast::TableRef, f: &mut impl FnMut(&mut QualifiedName)) {
        use duckrs_parser::ast::TableRefKind as K;
        match &mut table.kind {
            K::BaseTable { name, .. } => f(name),
            K::Join(join) => {
                in_table(&mut join.left, f);
                in_table(&mut join.right, f);
            }
            K::Subquery(query) => in_query(&mut query.node, f),
            // `pragma_table_info('db.t')`: the table is named by the text
            K::TableFunction { function, .. } => {
                if let ExprKind::Function(call) = &mut function.kind
                    && call.name.qualification.is_empty()
                    && matches!(
                        call.name.name.to_lowercase().as_str(),
                        "pragma_table_info" | "pragma_show"
                    )
                    && let [argument] = call.args.as_mut_slice()
                    && let ExprKind::Constant(literal) = &mut argument.expr.kind
                    && literal.kind == duckrs_parser::ast::LiteralKind::String
                {
                    let mut parts: Vec<String> = literal.text.split('.').map(str::to_string).collect();
                    let mut name = QualifiedName {
                        name: parts.pop().unwrap_or_default(),
                        qualification: parts,
                    };
                    f(&mut name);
                    let mut parts = name.qualification;
                    parts.push(name.name);
                    literal.text = parts.join(".");
                }
            }
            _ => {}
        }
    }
    fn in_query(node: &mut duckrs_parser::ast::QueryNode, f: &mut impl FnMut(&mut QualifiedName)) {
        use duckrs_parser::ast::QueryNodeKind as K;
        for (_, cte) in &mut node.cte_map.0 {
            in_query(&mut cte.query, f);
        }
        match &mut node.kind {
            K::Select(select) => in_table(&mut select.from_table, f),
            K::SetOperation(set) => set.children.iter_mut().for_each(|child| in_query(child, f)),
            _ => {}
        }
        duckrs_parser::transformer::visit_query_node_exprs_mut(node, &mut |e: &mut Expr| in_expression(e, f));
    }
    /// The subquery of `e`, whose parts go by anyway.
    fn in_expression(e: &mut Expr, f: &mut impl FnMut(&mut QualifiedName)) {
        if let ExprKind::Subquery { subquery, .. } = &mut e.kind {
            in_query(&mut subquery.node, f);
        }
    }
    use duckrs_parser::ast::InsertValues;
    match statement {
        Statement::Select(select) => in_query(&mut select.node, f),
        Statement::Insert(insert) => {
            f(&mut insert.table);
            if let InsertValues::Select(query) = &mut insert.values {
                in_query(&mut query.node, f);
            }
        }
        Statement::Update(update) => in_table(&mut update.table, f),
        Statement::Delete(delete) => in_table(&mut delete.table, f),
        Statement::CreateTable(info) => {
            f(&mut info.name);
            if let Some(query) = &mut info.query {
                in_query(&mut query.node, f);
            }
        }
        // a trigger finds the database of its table itself, as it keeps the name as written
        Statement::Create(info) if matches!(info.kind, CreateKind::Trigger { .. }) => {}
        Statement::Create(info) => f(&mut info.name),
        Statement::Drop(info) if info.trigger_table.is_some() => {}
        Statement::Drop(info) => f(&mut info.name),
        Statement::Alter(info) => f(&mut info.name),
        Statement::Explain { statement, .. } => for_each_table_name(statement, f),
        Statement::Vacuum { table: Some(table), .. } => in_table(table, f),
        Statement::Copy(info) => match &mut info.select {
            Some(query) => in_query(query, f),
            None => f(&mut info.table),
        },
        _ => {}
    }
}

/// The statement a trigger body is.
pub fn body_statement(action: &QueryNode) -> Result<Statement> {
    Ok(match &action.kind {
        QueryNodeKind::Insert(insert) => {
            let mut insert = insert.clone();
            if !action.cte_map.0.is_empty() {
                insert.cte_map = action.cte_map.clone();
            }
            Statement::Insert(insert)
        }
        QueryNodeKind::Update(update) => {
            let mut update = update.clone();
            if !action.cte_map.0.is_empty() {
                update.cte_map = action.cte_map.clone();
            }
            Statement::Update(update)
        }
        QueryNodeKind::Delete(delete) => {
            let mut delete = delete.clone();
            if !action.cte_map.0.is_empty() {
                delete.cte_map = action.cte_map.clone();
            }
            Statement::Delete(delete)
        }
        QueryNodeKind::Merge(merge) => Statement::Merge(merge.clone()),
        _ => {
            return Err(binder_error(
                "Trigger body must be an INSERT, UPDATE, or DELETE statement",
            ));
        }
    })
}

/// The names of the tables and views a statement refers to: its target, its FROM and USING
/// clauses, and those of its subqueries.
pub fn referenced_names(statement: &mut Statement) -> Vec<duckrs_parser::ast::QualifiedName> {
    use duckrs_parser::ast::{SelectNode, SelectStatement};
    let mut names = Vec::new();
    let mut add = |statement: &mut Statement| {
        for_each_table_name(statement, &mut |name| names.push(name.clone()));
    };
    // a SELECT of a FROM clause and of expressions, for their names
    let select_of = |from: TableRef, exprs: Vec<Expr>| {
        let node = SelectNode {
            select_list: if exprs.is_empty() { vec![Expr::star()] } else { exprs },
            from_table: from,
            ..Default::default()
        };
        Statement::Select(SelectStatement { node: node.into() })
    };
    add(statement);
    match statement {
        Statement::Update(update) => {
            let mut exprs = update.set_info.expressions.clone();
            exprs.extend(update.set_info.condition.iter().map(|c| (**c).clone()));
            let from = update.from_table.clone().unwrap_or_else(TableRef::empty);
            add(&mut select_of(from, exprs));
        }
        Statement::Delete(delete) => {
            for using in delete.using.clone() {
                add(&mut select_of(using, Vec::new()));
            }
            let exprs: Vec<Expr> = delete.condition.iter().map(|c| (**c).clone()).collect();
            add(&mut select_of(TableRef::empty(), exprs));
        }
        Statement::Merge(merge) => {
            add(&mut select_of((*merge.target).clone(), Vec::new()));
            add(&mut select_of((*merge.source).clone(), Vec::new()));
        }
        _ => {}
    }
    names
}

/// The tables and views a trigger body names, other than its transition tables and its own CTEs:
/// what it depends on.
pub fn body_dependencies(action: &QueryNode, referencing: &[&str]) -> Vec<String> {
    let Ok(mut statement) = body_statement(action) else {
        return Vec::new();
    };
    let cte_names: Vec<String> = action.cte_map.0.iter().map(|(n, _)| n.to_lowercase()).collect();
    let mut dependencies: Vec<String> = Vec::new();
    for name in referenced_names(&mut statement) {
        let shadowed = name.qualification.is_empty()
            && (referencing
                .iter()
                .any(|t| !t.is_empty() && t.eq_ignore_ascii_case(&name.name))
                || cte_names.contains(&name.name.to_lowercase()));
        if !shadowed && !dependencies.iter().any(|d| d.eq_ignore_ascii_case(&name.name)) {
            dependencies.push(name.name.clone());
        }
    }
    dependencies
}
