use hir::ModuleDef;
use ide_db::{
    assists::{AssistId, AssistKind},
    defs::Definition,
    helpers::mod_path_to_ast,
    imports::insert_use::{insert_use, ImportScope},
    search::{FileReference, UsageSearchResult},
    source_change::SourceChangeBuilder,
    FxHashSet,
};
use itertools::Itertools;
use syntax::{
    ast::{
        self,
        edit::IndentLevel,
        edit_in_place::{AttrsOwnerEdit, Indent},
        make, HasName,
    },
    ted, AstNode, NodeOrToken, SyntaxNode, T,
};
use text_edit::TextRange;

use crate::assist_context::{AssistContext, Assists};

// Assist: bool_to_enum
//
// This converts boolean local variables, fields, constants, and statics into a new
// enum with two variants `Bool::True` and `Bool::False`, as well as replacing
// all assignments with the variants and replacing all usages with `== Bool::True` or
// `== Bool::False`.
//
// ```
// fn main() {
//     let $0bool = true;
//
//     if bool {
//         println!("foo");
//     }
// }
// ```
// ->
// ```
// fn main() {
//     #[derive(PartialEq, Eq)]
//     enum Bool { True, False }
//
//     let bool = Bool::True;
//
//     if bool == Bool::True {
//         println!("foo");
//     }
// }
// ```
pub(crate) fn bool_to_enum(acc: &mut Assists, ctx: &AssistContext<'_>) -> Option<()> {
    let BoolNodeData { target_node, name, ty_annotation, initializer, definition } =
        find_bool_node(ctx)?;
    let target_module = ctx.sema.scope(&target_node)?.module().nearest_non_block_module(ctx.db());

    let target = name.syntax().text_range();
    acc.add(
        AssistId("bool_to_enum", AssistKind::RefactorRewrite),
        "Convert boolean to enum",
        target,
        |edit| {
            if let Some(ty) = &ty_annotation {
                cov_mark::hit!(replaces_ty_annotation);
                edit.replace(ty.syntax().text_range(), "Bool");
            }

            if let Some(initializer) = initializer {
                replace_bool_expr(edit, initializer);
            }

            let usages = definition.usages(&ctx.sema).all();
            add_enum_def(edit, ctx, &usages, target_node, &target_module);
            replace_usages(edit, ctx, &usages, definition, &target_module);
        },
    )
}

struct BoolNodeData {
    target_node: SyntaxNode,
    name: ast::Name,
    ty_annotation: Option<ast::Type>,
    initializer: Option<ast::Expr>,
    definition: Definition,
}

/// Attempts to find an appropriate node to apply the action to.
fn find_bool_node(ctx: &AssistContext<'_>) -> Option<BoolNodeData> {
    let name: ast::Name = ctx.find_node_at_offset()?;

    if let Some(let_stmt) = name.syntax().ancestors().find_map(ast::LetStmt::cast) {
        let bind_pat = match let_stmt.pat()? {
            ast::Pat::IdentPat(pat) => pat,
            _ => {
                cov_mark::hit!(not_applicable_in_non_ident_pat);
                return None;
            }
        };
        let def = ctx.sema.to_def(&bind_pat)?;
        if !def.ty(ctx.db()).is_bool() {
            cov_mark::hit!(not_applicable_non_bool_local);
            return None;
        }

        Some(BoolNodeData {
            target_node: let_stmt.syntax().clone(),
            name,
            ty_annotation: let_stmt.ty(),
            initializer: let_stmt.initializer(),
            definition: Definition::Local(def),
        })
    } else if let Some(const_) = name.syntax().parent().and_then(ast::Const::cast) {
        let def = ctx.sema.to_def(&const_)?;
        if !def.ty(ctx.db()).is_bool() {
            cov_mark::hit!(not_applicable_non_bool_const);
            return None;
        }

        Some(BoolNodeData {
            target_node: const_.syntax().clone(),
            name,
            ty_annotation: const_.ty(),
            initializer: const_.body(),
            definition: Definition::Const(def),
        })
    } else if let Some(static_) = name.syntax().parent().and_then(ast::Static::cast) {
        let def = ctx.sema.to_def(&static_)?;
        if !def.ty(ctx.db()).is_bool() {
            cov_mark::hit!(not_applicable_non_bool_static);
            return None;
        }

        Some(BoolNodeData {
            target_node: static_.syntax().clone(),
            name,
            ty_annotation: static_.ty(),
            initializer: static_.body(),
            definition: Definition::Static(def),
        })
    } else {
        let field = name.syntax().parent().and_then(ast::RecordField::cast)?;
        if field.name()? != name {
            return None;
        }

        let adt = field.syntax().ancestors().find_map(ast::Adt::cast)?;
        let def = ctx.sema.to_def(&field)?;
        if !def.ty(ctx.db()).is_bool() {
            cov_mark::hit!(not_applicable_non_bool_field);
            return None;
        }
        Some(BoolNodeData {
            target_node: adt.syntax().clone(),
            name,
            ty_annotation: field.ty(),
            initializer: None,
            definition: Definition::Field(def),
        })
    }
}

fn replace_bool_expr(edit: &mut SourceChangeBuilder, expr: ast::Expr) {
    let expr_range = expr.syntax().text_range();
    let enum_expr = bool_expr_to_enum_expr(expr);
    edit.replace(expr_range, enum_expr.syntax().text())
}

/// Converts an expression of type `bool` to one of the new enum type.
fn bool_expr_to_enum_expr(expr: ast::Expr) -> ast::Expr {
    let true_expr = make::expr_path(make::path_from_text("Bool::True")).clone_for_update();
    let false_expr = make::expr_path(make::path_from_text("Bool::False")).clone_for_update();

    if let ast::Expr::Literal(literal) = &expr {
        match literal.kind() {
            ast::LiteralKind::Bool(true) => true_expr,
            ast::LiteralKind::Bool(false) => false_expr,
            _ => expr,
        }
    } else {
        make::expr_if(
            expr,
            make::tail_only_block_expr(true_expr),
            Some(ast::ElseBranch::Block(make::tail_only_block_expr(false_expr))),
        )
        .clone_for_update()
    }
}

/// Replaces all usages of the target identifier, both when read and written to.
fn replace_usages(
    edit: &mut SourceChangeBuilder,
    ctx: &AssistContext<'_>,
    usages: &UsageSearchResult,
    target_definition: Definition,
    target_module: &hir::Module,
) {
    for (file_id, references) in usages.iter() {
        edit.edit_file(*file_id);

        let refs_with_imports =
            augment_references_with_imports(edit, ctx, references, target_module);

        refs_with_imports.into_iter().rev().for_each(
            |FileReferenceWithImport { range, old_name, new_name, import_data }| {
                // replace the usages in patterns and expressions
                if let Some(ident_pat) = old_name.syntax().ancestors().find_map(ast::IdentPat::cast)
                {
                    cov_mark::hit!(replaces_record_pat_shorthand);

                    let definition = ctx.sema.to_def(&ident_pat).map(Definition::Local);
                    if let Some(def) = definition {
                        replace_usages(
                            edit,
                            ctx,
                            &def.usages(&ctx.sema).all(),
                            target_definition,
                            target_module,
                        )
                    }
                } else if let Some(initializer) = find_assignment_usage(&new_name) {
                    cov_mark::hit!(replaces_assignment);

                    replace_bool_expr(edit, initializer);
                } else if let Some((prefix_expr, inner_expr)) = find_negated_usage(&new_name) {
                    cov_mark::hit!(replaces_negation);

                    edit.replace(
                        prefix_expr.syntax().text_range(),
                        format!("{} == Bool::False", inner_expr),
                    );
                } else if let Some((record_field, initializer)) = old_name
                    .as_name_ref()
                    .and_then(ast::RecordExprField::for_field_name)
                    .and_then(|record_field| ctx.sema.resolve_record_field(&record_field))
                    .and_then(|(got_field, _, _)| {
                        find_record_expr_usage(&new_name, got_field, target_definition)
                    })
                {
                    cov_mark::hit!(replaces_record_expr);

                    let record_field = edit.make_mut(record_field);
                    let enum_expr = bool_expr_to_enum_expr(initializer);
                    record_field.replace_expr(enum_expr);
                } else if let Some(pat) = find_record_pat_field_usage(&old_name) {
                    match pat {
                        ast::Pat::IdentPat(ident_pat) => {
                            cov_mark::hit!(replaces_record_pat);

                            let definition = ctx.sema.to_def(&ident_pat).map(Definition::Local);
                            if let Some(def) = definition {
                                replace_usages(
                                    edit,
                                    ctx,
                                    &def.usages(&ctx.sema).all(),
                                    target_definition,
                                    target_module,
                                )
                            }
                        }
                        ast::Pat::LiteralPat(literal_pat) => {
                            cov_mark::hit!(replaces_literal_pat);

                            if let Some(expr) = literal_pat.literal().and_then(|literal| {
                                literal.syntax().ancestors().find_map(ast::Expr::cast)
                            }) {
                                replace_bool_expr(edit, expr);
                            }
                        }
                        _ => (),
                    }
                } else if new_name.syntax().ancestors().find_map(ast::UseTree::cast).is_none() {
                    // for any other usage in an expression, replace it with a check that it is the true variant
                    if let Some((record_field, expr)) = new_name
                        .as_name_ref()
                        .and_then(ast::RecordExprField::for_field_name)
                        .and_then(|record_field| {
                            record_field.expr().map(|expr| (record_field, expr))
                        })
                    {
                        record_field.replace_expr(
                            make::expr_bin_op(
                                expr,
                                ast::BinaryOp::CmpOp(ast::CmpOp::Eq { negated: false }),
                                make::expr_path(make::path_from_text("Bool::True")),
                            )
                            .clone_for_update(),
                        );
                    } else {
                        edit.replace(range, format!("{} == Bool::True", new_name.text()));
                    }
                }

                // add imports across modules where needed
                if let Some((import_scope, path)) = import_data {
                    insert_use(&import_scope, path, &ctx.config.insert_use);
                }
            },
        )
    }
}

struct FileReferenceWithImport {
    range: TextRange,
    old_name: ast::NameLike,
    new_name: ast::NameLike,
    import_data: Option<(ImportScope, ast::Path)>,
}

fn augment_references_with_imports(
    edit: &mut SourceChangeBuilder,
    ctx: &AssistContext<'_>,
    references: &[FileReference],
    target_module: &hir::Module,
) -> Vec<FileReferenceWithImport> {
    let mut visited_modules = FxHashSet::default();

    references
        .iter()
        .filter_map(|FileReference { range, name, .. }| {
            ctx.sema.scope(name.syntax()).map(|scope| (*range, name, scope.module()))
        })
        .map(|(range, name, ref_module)| {
            let old_name = name.clone();
            let new_name = edit.make_mut(name.clone());

            // if the referenced module is not the same as the target one and has not been seen before, add an import
            let import_data = if ref_module.nearest_non_block_module(ctx.db()) != *target_module
                && !visited_modules.contains(&ref_module)
            {
                visited_modules.insert(ref_module);

                let import_scope =
                    ImportScope::find_insert_use_container(new_name.syntax(), &ctx.sema);
                let path = ref_module
                    .find_use_path_prefixed(
                        ctx.sema.db,
                        ModuleDef::Module(*target_module),
                        ctx.config.insert_use.prefix_kind,
                        ctx.config.prefer_no_std,
                    )
                    .map(|mod_path| {
                        make::path_concat(mod_path_to_ast(&mod_path), make::path_from_text("Bool"))
                    });

                import_scope.zip(path)
            } else {
                None
            };

            FileReferenceWithImport { range, old_name, new_name, import_data }
        })
        .collect()
}

fn find_assignment_usage(name: &ast::NameLike) -> Option<ast::Expr> {
    let bin_expr = name.syntax().ancestors().find_map(ast::BinExpr::cast)?;

    if !bin_expr.lhs()?.syntax().descendants().contains(name.syntax()) {
        cov_mark::hit!(dont_assign_incorrect_ref);
        return None;
    }

    if let Some(ast::BinaryOp::Assignment { op: None }) = bin_expr.op_kind() {
        bin_expr.rhs()
    } else {
        None
    }
}

fn find_negated_usage(name: &ast::NameLike) -> Option<(ast::PrefixExpr, ast::Expr)> {
    let prefix_expr = name.syntax().ancestors().find_map(ast::PrefixExpr::cast)?;

    if !matches!(prefix_expr.expr()?, ast::Expr::PathExpr(_) | ast::Expr::FieldExpr(_)) {
        cov_mark::hit!(dont_overwrite_expression_inside_negation);
        return None;
    }

    if let Some(ast::UnaryOp::Not) = prefix_expr.op_kind() {
        let inner_expr = prefix_expr.expr()?;
        Some((prefix_expr, inner_expr))
    } else {
        None
    }
}

fn find_record_expr_usage(
    name: &ast::NameLike,
    got_field: hir::Field,
    target_definition: Definition,
) -> Option<(ast::RecordExprField, ast::Expr)> {
    let name_ref = name.as_name_ref()?;
    let record_field = ast::RecordExprField::for_field_name(name_ref)?;
    let initializer = record_field.expr()?;

    if let Definition::Field(expected_field) = target_definition {
        if got_field != expected_field {
            return None;
        }
    }

    Some((record_field, initializer))
}

fn find_record_pat_field_usage(name: &ast::NameLike) -> Option<ast::Pat> {
    let record_pat_field = name.syntax().parent().and_then(ast::RecordPatField::cast)?;
    let pat = record_pat_field.pat()?;

    match pat {
        ast::Pat::IdentPat(_) | ast::Pat::LiteralPat(_) | ast::Pat::WildcardPat(_) => Some(pat),
        _ => None,
    }
}

/// Adds the definition of the new enum before the target node.
fn add_enum_def(
    edit: &mut SourceChangeBuilder,
    ctx: &AssistContext<'_>,
    usages: &UsageSearchResult,
    target_node: SyntaxNode,
    target_module: &hir::Module,
) {
    let make_enum_pub = usages
        .iter()
        .flat_map(|(_, refs)| refs)
        .filter_map(|FileReference { name, .. }| {
            ctx.sema.scope(name.syntax()).map(|scope| scope.module())
        })
        .any(|module| module.nearest_non_block_module(ctx.db()) != *target_module);
    let enum_def = make_bool_enum(make_enum_pub);

    let indent = IndentLevel::from_node(&target_node);
    enum_def.reindent_to(indent);

    ted::insert_all(
        ted::Position::before(&edit.make_syntax_mut(target_node)),
        vec![
            enum_def.syntax().clone().into(),
            make::tokens::whitespace(&format!("\n\n{indent}")).into(),
        ],
    );
}

fn make_bool_enum(make_pub: bool) -> ast::Enum {
    let enum_def = make::enum_(
        if make_pub { Some(make::visibility_pub()) } else { None },
        make::name("Bool"),
        make::variant_list(vec![
            make::variant(make::name("True"), None),
            make::variant(make::name("False"), None),
        ]),
    )
    .clone_for_update();

    let derive_eq = make::attr_outer(make::meta_token_tree(
        make::ext::ident_path("derive"),
        make::token_tree(
            T!['('],
            vec![
                NodeOrToken::Token(make::tokens::ident("PartialEq")),
                NodeOrToken::Token(make::token(T![,])),
                NodeOrToken::Token(make::tokens::single_space()),
                NodeOrToken::Token(make::tokens::ident("Eq")),
            ],
        ),
    ))
    .clone_for_update();
    enum_def.add_attr(derive_eq);

    enum_def
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tests::{check_assist, check_assist_not_applicable};

    #[test]
    fn local_variable_with_usage() {
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    let $0foo = true;

    if foo {
        println!("foo");
    }
}
"#,
            r#"
fn main() {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    let foo = Bool::True;

    if foo == Bool::True {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn local_variable_with_usage_negated() {
        cov_mark::check!(replaces_negation);
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    let $0foo = true;

    if !foo {
        println!("foo");
    }
}
"#,
            r#"
fn main() {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    let foo = Bool::True;

    if foo == Bool::False {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn local_variable_with_type_annotation() {
        cov_mark::check!(replaces_ty_annotation);
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    let $0foo: bool = false;
}
"#,
            r#"
fn main() {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    let foo: Bool = Bool::False;
}
"#,
        )
    }

    #[test]
    fn local_variable_with_non_literal_initializer() {
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    let $0foo = 1 == 2;
}
"#,
            r#"
fn main() {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    let foo = if 1 == 2 { Bool::True } else { Bool::False };
}
"#,
        )
    }

    #[test]
    fn local_variable_binexpr_usage() {
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    let $0foo = false;
    let bar = true;

    if !foo && bar {
        println!("foobar");
    }
}
"#,
            r#"
fn main() {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    let foo = Bool::False;
    let bar = true;

    if foo == Bool::False && bar {
        println!("foobar");
    }
}
"#,
        )
    }

    #[test]
    fn local_variable_unop_usage() {
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    let $0foo = true;

    if *&foo {
        println!("foobar");
    }
}
"#,
            r#"
fn main() {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    let foo = Bool::True;

    if *&foo == Bool::True {
        println!("foobar");
    }
}
"#,
        )
    }

    #[test]
    fn local_variable_assigned_later() {
        cov_mark::check!(replaces_assignment);
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    let $0foo: bool;
    foo = true;
}
"#,
            r#"
fn main() {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    let foo: Bool;
    foo = Bool::True;
}
"#,
        )
    }

    #[test]
    fn local_variable_does_not_apply_recursively() {
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    let $0foo = true;
    let bar = !foo;

    if bar {
        println!("bar");
    }
}
"#,
            r#"
fn main() {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    let foo = Bool::True;
    let bar = foo == Bool::False;

    if bar {
        println!("bar");
    }
}
"#,
        )
    }

    #[test]
    fn local_variable_nested_in_negation() {
        cov_mark::check!(dont_overwrite_expression_inside_negation);
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    if !"foo".chars().any(|c| {
        let $0foo = true;
        foo
    }) {
        println!("foo");
    }
}
"#,
            r#"
fn main() {
    if !"foo".chars().any(|c| {
        #[derive(PartialEq, Eq)]
        enum Bool { True, False }

        let foo = Bool::True;
        foo == Bool::True
    }) {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn local_variable_non_bool() {
        cov_mark::check!(not_applicable_non_bool_local);
        check_assist_not_applicable(
            bool_to_enum,
            r#"
fn main() {
    let $0foo = 1;
}
"#,
        )
    }

    #[test]
    fn local_variable_cursor_not_on_ident() {
        check_assist_not_applicable(
            bool_to_enum,
            r#"
fn main() {
    let foo = $0true;
}
"#,
        )
    }

    #[test]
    fn local_variable_non_ident_pat() {
        cov_mark::check!(not_applicable_in_non_ident_pat);
        check_assist_not_applicable(
            bool_to_enum,
            r#"
fn main() {
    let ($0foo, bar) = (true, false);
}
"#,
        )
    }

    #[test]
    fn field_struct_basic() {
        cov_mark::check!(replaces_record_expr);
        check_assist(
            bool_to_enum,
            r#"
struct Foo {
    $0bar: bool,
    baz: bool,
}

fn main() {
    let foo = Foo { bar: true, baz: false };

    if foo.bar {
        println!("foo");
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

struct Foo {
    bar: Bool,
    baz: bool,
}

fn main() {
    let foo = Foo { bar: Bool::True, baz: false };

    if foo.bar == Bool::True {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn field_enum_basic() {
        cov_mark::check!(replaces_record_pat);
        check_assist(
            bool_to_enum,
            r#"
enum Foo {
    Foo,
    Bar { $0bar: bool },
}

fn main() {
    let foo = Foo::Bar { bar: true };

    if let Foo::Bar { bar: baz } = foo {
        if baz {
            println!("foo");
        }
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

enum Foo {
    Foo,
    Bar { bar: Bool },
}

fn main() {
    let foo = Foo::Bar { bar: Bool::True };

    if let Foo::Bar { bar: baz } = foo {
        if baz == Bool::True {
            println!("foo");
        }
    }
}
"#,
        )
    }

    #[test]
    fn field_enum_cross_file() {
        check_assist(
            bool_to_enum,
            r#"
//- /foo.rs
pub enum Foo {
    Foo,
    Bar { $0bar: bool },
}

fn foo() {
    let foo = Foo::Bar { bar: true };
}

//- /main.rs
use foo::Foo;

mod foo;

fn main() {
    let foo = Foo::Bar { bar: false };
}
"#,
            r#"
//- /foo.rs
#[derive(PartialEq, Eq)]
pub enum Bool { True, False }

pub enum Foo {
    Foo,
    Bar { bar: Bool },
}

fn foo() {
    let foo = Foo::Bar { bar: Bool::True };
}

//- /main.rs
use foo::{Foo, Bool};

mod foo;

fn main() {
    let foo = Foo::Bar { bar: Bool::False };
}
"#,
        )
    }

    #[test]
    fn field_enum_shorthand() {
        cov_mark::check!(replaces_record_pat_shorthand);
        check_assist(
            bool_to_enum,
            r#"
enum Foo {
    Foo,
    Bar { $0bar: bool },
}

fn main() {
    let foo = Foo::Bar { bar: true };

    match foo {
        Foo::Bar { bar } => {
            if bar {
                println!("foo");
            }
        }
        _ => (),
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

enum Foo {
    Foo,
    Bar { bar: Bool },
}

fn main() {
    let foo = Foo::Bar { bar: Bool::True };

    match foo {
        Foo::Bar { bar } => {
            if bar == Bool::True {
                println!("foo");
            }
        }
        _ => (),
    }
}
"#,
        )
    }

    #[test]
    fn field_enum_replaces_literal_patterns() {
        cov_mark::check!(replaces_literal_pat);
        check_assist(
            bool_to_enum,
            r#"
enum Foo {
    Foo,
    Bar { $0bar: bool },
}

fn main() {
    let foo = Foo::Bar { bar: true };

    if let Foo::Bar { bar: true } = foo {
        println!("foo");
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

enum Foo {
    Foo,
    Bar { bar: Bool },
}

fn main() {
    let foo = Foo::Bar { bar: Bool::True };

    if let Foo::Bar { bar: Bool::True } = foo {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn field_enum_keeps_wildcard_patterns() {
        check_assist(
            bool_to_enum,
            r#"
enum Foo {
    Foo,
    Bar { $0bar: bool },
}

fn main() {
    let foo = Foo::Bar { bar: true };

    if let Foo::Bar { bar: _ } = foo {
        println!("foo");
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

enum Foo {
    Foo,
    Bar { bar: Bool },
}

fn main() {
    let foo = Foo::Bar { bar: Bool::True };

    if let Foo::Bar { bar: _ } = foo {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn field_union_basic() {
        check_assist(
            bool_to_enum,
            r#"
union Foo {
    $0foo: bool,
    bar: usize,
}

fn main() {
    let foo = Foo { foo: true };

    if unsafe { foo.foo } {
        println!("foo");
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

union Foo {
    foo: Bool,
    bar: usize,
}

fn main() {
    let foo = Foo { foo: Bool::True };

    if unsafe { foo.foo == Bool::True } {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn field_negated() {
        check_assist(
            bool_to_enum,
            r#"
struct Foo {
    $0bar: bool,
}

fn main() {
    let foo = Foo { bar: false };

    if !foo.bar {
        println!("foo");
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

struct Foo {
    bar: Bool,
}

fn main() {
    let foo = Foo { bar: Bool::False };

    if foo.bar == Bool::False {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn field_in_mod_properly_indented() {
        check_assist(
            bool_to_enum,
            r#"
mod foo {
    struct Bar {
        $0baz: bool,
    }

    impl Bar {
        fn new(baz: bool) -> Self {
            Self { baz }
        }
    }
}
"#,
            r#"
mod foo {
    #[derive(PartialEq, Eq)]
    enum Bool { True, False }

    struct Bar {
        baz: Bool,
    }

    impl Bar {
        fn new(baz: bool) -> Self {
            Self { baz: if baz { Bool::True } else { Bool::False } }
        }
    }
}
"#,
        )
    }

    #[test]
    fn field_multiple_initializations() {
        check_assist(
            bool_to_enum,
            r#"
struct Foo {
    $0bar: bool,
    baz: bool,
}

fn main() {
    let foo1 = Foo { bar: true, baz: false };
    let foo2 = Foo { bar: false, baz: false };

    if foo1.bar && foo2.bar {
        println!("foo");
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

struct Foo {
    bar: Bool,
    baz: bool,
}

fn main() {
    let foo1 = Foo { bar: Bool::True, baz: false };
    let foo2 = Foo { bar: Bool::False, baz: false };

    if foo1.bar == Bool::True && foo2.bar == Bool::True {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn field_assigned_to_another() {
        cov_mark::check!(dont_assign_incorrect_ref);
        check_assist(
            bool_to_enum,
            r#"
struct Foo {
    $0foo: bool,
}

struct Bar {
    bar: bool,
}

fn main() {
    let foo = Foo { foo: true };
    let mut bar = Bar { bar: true };

    bar.bar = foo.foo;
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

struct Foo {
    foo: Bool,
}

struct Bar {
    bar: bool,
}

fn main() {
    let foo = Foo { foo: Bool::True };
    let mut bar = Bar { bar: true };

    bar.bar = foo.foo == Bool::True;
}
"#,
        )
    }

    #[test]
    fn field_initialized_with_other() {
        check_assist(
            bool_to_enum,
            r#"
struct Foo {
    $0foo: bool,
}

struct Bar {
    bar: bool,
}

fn main() {
    let foo = Foo { foo: true };
    let bar = Bar { bar: foo.foo };
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

struct Foo {
    foo: Bool,
}

struct Bar {
    bar: bool,
}

fn main() {
    let foo = Foo { foo: Bool::True };
    let bar = Bar { bar: foo.foo == Bool::True };
}
"#,
        )
    }

    #[test]
    fn field_non_bool() {
        cov_mark::check!(not_applicable_non_bool_field);
        check_assist_not_applicable(
            bool_to_enum,
            r#"
struct Foo {
    $0bar: usize,
}

fn main() {
    let foo = Foo { bar: 1 };
}
"#,
        )
    }

    #[test]
    fn const_basic() {
        check_assist(
            bool_to_enum,
            r#"
const $0FOO: bool = false;

fn main() {
    if FOO {
        println!("foo");
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

const FOO: Bool = Bool::False;

fn main() {
    if FOO == Bool::True {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn const_in_module() {
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    if foo::FOO {
        println!("foo");
    }
}

mod foo {
    pub const $0FOO: bool = true;
}
"#,
            r#"
use foo::Bool;

fn main() {
    if foo::FOO == Bool::True {
        println!("foo");
    }
}

mod foo {
    #[derive(PartialEq, Eq)]
    pub enum Bool { True, False }

    pub const FOO: Bool = Bool::True;
}
"#,
        )
    }

    #[test]
    fn const_in_module_with_import() {
        check_assist(
            bool_to_enum,
            r#"
fn main() {
    use foo::FOO;

    if FOO {
        println!("foo");
    }
}

mod foo {
    pub const $0FOO: bool = true;
}
"#,
            r#"
use crate::foo::Bool;

fn main() {
    use foo::FOO;

    if FOO == Bool::True {
        println!("foo");
    }
}

mod foo {
    #[derive(PartialEq, Eq)]
    pub enum Bool { True, False }

    pub const FOO: Bool = Bool::True;
}
"#,
        )
    }

    #[test]
    fn const_cross_file() {
        check_assist(
            bool_to_enum,
            r#"
//- /main.rs
mod foo;

fn main() {
    if foo::FOO {
        println!("foo");
    }
}

//- /foo.rs
pub const $0FOO: bool = true;
"#,
            r#"
//- /main.rs
use foo::Bool;

mod foo;

fn main() {
    if foo::FOO == Bool::True {
        println!("foo");
    }
}

//- /foo.rs
#[derive(PartialEq, Eq)]
pub enum Bool { True, False }

pub const FOO: Bool = Bool::True;
"#,
        )
    }

    #[test]
    fn const_cross_file_and_module() {
        check_assist(
            bool_to_enum,
            r#"
//- /main.rs
mod foo;

fn main() {
    use foo::bar;

    if bar::BAR {
        println!("foo");
    }
}

//- /foo.rs
pub mod bar {
    pub const $0BAR: bool = false;
}
"#,
            r#"
//- /main.rs
use crate::foo::bar::Bool;

mod foo;

fn main() {
    use foo::bar;

    if bar::BAR == Bool::True {
        println!("foo");
    }
}

//- /foo.rs
pub mod bar {
    #[derive(PartialEq, Eq)]
    pub enum Bool { True, False }

    pub const BAR: Bool = Bool::False;
}
"#,
        )
    }

    #[test]
    fn const_non_bool() {
        cov_mark::check!(not_applicable_non_bool_const);
        check_assist_not_applicable(
            bool_to_enum,
            r#"
const $0FOO: &str = "foo";

fn main() {
    println!("{FOO}");
}
"#,
        )
    }

    #[test]
    fn static_basic() {
        check_assist(
            bool_to_enum,
            r#"
static mut $0BOOL: bool = true;

fn main() {
    unsafe { BOOL = false };
    if unsafe { BOOL } {
        println!("foo");
    }
}
"#,
            r#"
#[derive(PartialEq, Eq)]
enum Bool { True, False }

static mut BOOL: Bool = Bool::True;

fn main() {
    unsafe { BOOL = Bool::False };
    if unsafe { BOOL == Bool::True } {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn static_non_bool() {
        cov_mark::check!(not_applicable_non_bool_static);
        check_assist_not_applicable(
            bool_to_enum,
            r#"
static mut $0FOO: usize = 0;

fn main() {
    if unsafe { FOO } == 0 {
        println!("foo");
    }
}
"#,
        )
    }

    #[test]
    fn not_applicable_to_other_names() {
        check_assist_not_applicable(bool_to_enum, "fn $0main() {}")
    }
}
