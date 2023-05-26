//
// Unused import checking
//
// Although this is mostly a lint pass, it lives in here because it depends on
// resolve data structures and because it finalises the privacy information for
// `use` items.
//
// Unused trait imports can't be checked until the method resolution. We save
// candidates here, and do the actual check in rustc_hir_analysis/check_unused.rs.
//
// Checking for unused imports is split into three steps:
//
//  - `UnusedImportCheckVisitor` walks the AST to find all the unused imports
//    inside of `UseTree`s, recording their `NodeId`s and grouping them by
//    the parent `use` item
//
//  - `calc_unused_spans` then walks over all the `use` items marked in the
//    previous step to collect the spans associated with the `NodeId`s and to
//    calculate the spans that can be removed by rustfix; This is done in a
//    separate step to be able to collapse the adjacent spans that rustfix
//    will remove
//
//  - `check_crate` finally emits the diagnostics based on the data generated
//    in the last step

use crate::imports::ImportKind;
use crate::module_to_string;
use crate::Resolver;

use rustc_ast as ast;
use rustc_ast::visit::{self, Visitor};
use rustc_data_structures::fx::{FxHashMap, FxIndexMap};
use rustc_data_structures::unord::UnordSet;
use rustc_errors::{pluralize, MultiSpan};
use rustc_hir::def::{DefKind, Res};
use rustc_session::lint::builtin::{MACRO_USE_EXTERN_CRATE, UNUSED_EXTERN_CRATES, UNUSED_IMPORTS};
use rustc_session::lint::BuiltinLintDiagnostics;
use rustc_span::symbol::{kw, Ident};
use rustc_span::{Span, DUMMY_SP};

struct UnusedImport<'a> {
    use_tree: &'a ast::UseTree,
    use_tree_id: ast::NodeId,
    item_span: Span,
    unused: UnordSet<ast::NodeId>,
}

impl<'a> UnusedImport<'a> {
    fn add(&mut self, id: ast::NodeId) {
        self.unused.insert(id);
    }
}

struct UnusedImportCheckVisitor<'a, 'b, 'tcx> {
    r: &'a mut Resolver<'b, 'tcx>,
    /// All the (so far) unused imports, grouped path list
    unused_imports: FxIndexMap<ast::NodeId, UnusedImport<'a>>,
    extern_crate_items: Vec<ExternCrateToLint>,
    base_use_tree: Option<&'a ast::UseTree>,
    base_id: ast::NodeId,
    item_span: Span,
    base_use_is_pub: bool,
}

struct ExternCrateToLint {
    id: ast::NodeId,
    /// Span from the item
    span: Span,
    /// Span to use to suggest complete removal.
    span_with_attributes: Span,
    /// Span of the visibility, if any.
    vis_span: Span,
    /// Whether the item has attrs.
    has_attrs: bool,
    /// Name used to refer to the crate.
    ident: Ident,
    /// Whether the statement renames the crate `extern crate orig_name as new_name;`.
    renames: bool,
}

impl<'a, 'b, 'tcx> UnusedImportCheckVisitor<'a, 'b, 'tcx> {
    // We have information about whether `use` (import) items are actually
    // used now. If an import is not used at all, we signal a lint error.
    fn check_import(&mut self, id: ast::NodeId) {
        let used = self.r.used_imports.contains(&id);
        let def_id = self.r.local_def_id(id);
        if !used {
            if self.r.maybe_unused_trait_imports.contains(&def_id) {
                // Check later.
                return;
            }
            self.unused_import(self.base_id).add(id);
        } else {
            // This trait import is definitely used, in a way other than
            // method resolution.
            self.r.maybe_unused_trait_imports.remove(&def_id);
            if let Some(i) = self.unused_imports.get_mut(&self.base_id) {
                i.unused.remove(&id);
            }
        }
    }

    fn unused_import(&mut self, id: ast::NodeId) -> &mut UnusedImport<'a> {
        let use_tree_id = self.base_id;
        let use_tree = self.base_use_tree.unwrap();
        let item_span = self.item_span;

        self.unused_imports.entry(id).or_insert_with(|| UnusedImport {
            use_tree,
            use_tree_id,
            item_span,
            unused: Default::default(),
        })
    }

    fn check_import_as_underscore(&mut self, item: &ast::UseTree, id: ast::NodeId) {
        match item.kind {
            ast::UseTreeKind::Simple(Some(ident)) => {
                if ident.name == kw::Underscore
                    && !self.r.import_res_map.get(&id).is_some_and(|per_ns| {
                        per_ns.iter().filter_map(|res| res.as_ref()).any(|res| {
                            matches!(res, Res::Def(DefKind::Trait | DefKind::TraitAlias, _))
                        })
                    })
                {
                    self.unused_import(self.base_id).add(id);
                }
            }
            ast::UseTreeKind::Nested(ref items) => self.check_imports_as_underscore(items),
            _ => {}
        }
    }

    fn check_imports_as_underscore(&mut self, items: &[(ast::UseTree, ast::NodeId)]) {
        for (item, id) in items {
            self.check_import_as_underscore(item, *id);
        }
    }
}

impl<'a, 'b, 'tcx> Visitor<'a> for UnusedImportCheckVisitor<'a, 'b, 'tcx> {
    fn visit_item(&mut self, item: &'a ast::Item) {
        match item.kind {
            // Ignore is_public import statements because there's no way to be sure
            // whether they're used or not. Also ignore imports with a dummy span
            // because this means that they were generated in some fashion by the
            // compiler and we don't need to consider them.
            ast::ItemKind::Use(..) if item.span.is_dummy() => return,
            ast::ItemKind::Use(..) => self.base_use_is_pub = item.vis.kind.is_pub(),
            ast::ItemKind::ExternCrate(orig_name) => {
                self.extern_crate_items.push(ExternCrateToLint {
                    id: item.id,
                    span: item.span,
                    vis_span: item.vis.span,
                    span_with_attributes: item.span_with_attributes(),
                    has_attrs: !item.attrs.is_empty(),
                    ident: item.ident,
                    renames: orig_name.is_some(),
                });
            }
            _ => {}
        }

        self.item_span = item.span_with_attributes();
        visit::walk_item(self, item);
    }

    fn visit_use_tree(&mut self, use_tree: &'a ast::UseTree, id: ast::NodeId, nested: bool) {
        // Use the base UseTree's NodeId as the item id
        // This allows the grouping of all the lints in the same item
        if !nested {
            self.base_id = id;
            self.base_use_tree = Some(use_tree);
        }

        if self.base_use_is_pub {
            self.check_import_as_underscore(use_tree, id);
            return;
        }

        if let ast::UseTreeKind::Nested(ref items) = use_tree.kind {
            if items.is_empty() {
                self.unused_import(self.base_id).add(id);
            }
        } else {
            self.check_import(id);
        }

        visit::walk_use_tree(self, use_tree, id);
    }
}

enum UnusedSpanResult {
    Used,
    FlatUnused(Span, Span),
    NestedFullUnused(Vec<Span>, Span),
    NestedPartialUnused(Vec<Span>, Vec<Span>),
}

fn calc_unused_spans(
    unused_import: &UnusedImport<'_>,
    use_tree: &ast::UseTree,
    use_tree_id: ast::NodeId,
) -> UnusedSpanResult {
    // The full span is the whole item's span if this current tree is not nested inside another
    // This tells rustfix to remove the whole item if all the imports are unused
    let full_span = if unused_import.use_tree.span == use_tree.span {
        unused_import.item_span
    } else {
        use_tree.span
    };
    match use_tree.kind {
        ast::UseTreeKind::Simple(..) | ast::UseTreeKind::Glob => {
            if unused_import.unused.contains(&use_tree_id) {
                UnusedSpanResult::FlatUnused(use_tree.span, full_span)
            } else {
                UnusedSpanResult::Used
            }
        }
        ast::UseTreeKind::Nested(ref nested) => {
            if nested.is_empty() {
                return UnusedSpanResult::FlatUnused(use_tree.span, full_span);
            }

            let mut unused_spans = Vec::new();
            let mut to_remove = Vec::new();
            let mut all_nested_unused = true;
            let mut previous_unused = false;
            for (pos, (use_tree, use_tree_id)) in nested.iter().enumerate() {
                let remove = match calc_unused_spans(unused_import, use_tree, *use_tree_id) {
                    UnusedSpanResult::Used => {
                        all_nested_unused = false;
                        None
                    }
                    UnusedSpanResult::FlatUnused(span, remove) => {
                        unused_spans.push(span);
                        Some(remove)
                    }
                    UnusedSpanResult::NestedFullUnused(mut spans, remove) => {
                        unused_spans.append(&mut spans);
                        Some(remove)
                    }
                    UnusedSpanResult::NestedPartialUnused(mut spans, mut to_remove_extra) => {
                        all_nested_unused = false;
                        unused_spans.append(&mut spans);
                        to_remove.append(&mut to_remove_extra);
                        None
                    }
                };
                if let Some(remove) = remove {
                    let remove_span = if nested.len() == 1 {
                        remove
                    } else if pos == nested.len() - 1 || !all_nested_unused {
                        // Delete everything from the end of the last import, to delete the
                        // previous comma
                        nested[pos - 1].0.span.shrink_to_hi().to(use_tree.span)
                    } else {
                        // Delete everything until the next import, to delete the trailing commas
                        use_tree.span.to(nested[pos + 1].0.span.shrink_to_lo())
                    };

                    // Try to collapse adjacent spans into a single one. This prevents all cases of
                    // overlapping removals, which are not supported by rustfix
                    if previous_unused && !to_remove.is_empty() {
                        let previous = to_remove.pop().unwrap();
                        to_remove.push(previous.to(remove_span));
                    } else {
                        to_remove.push(remove_span);
                    }
                }
                previous_unused = remove.is_some();
            }
            if unused_spans.is_empty() {
                UnusedSpanResult::Used
            } else if all_nested_unused {
                UnusedSpanResult::NestedFullUnused(unused_spans, full_span)
            } else {
                UnusedSpanResult::NestedPartialUnused(unused_spans, to_remove)
            }
        }
    }
}

impl Resolver<'_, '_> {
    pub(crate) fn check_unused(&mut self, krate: &ast::Crate) {
        let tcx = self.tcx;
        let mut maybe_unused_extern_crates = FxHashMap::default();

        for import in self.potentially_unused_imports.iter() {
            match import.kind {
                _ if import.used.get()
                    || import.expect_vis().is_public()
                    || import.span.is_dummy() =>
                {
                    if let ImportKind::MacroUse = import.kind {
                        if !import.span.is_dummy() {
                            self.lint_buffer.buffer_lint(
                                MACRO_USE_EXTERN_CRATE,
                                import.root_id,
                                import.span,
                                "deprecated `#[macro_use]` attribute used to \
                                import macros should be replaced at use sites \
                                with a `use` item to import the macro \
                                instead",
                            );
                        }
                    }
                }
                ImportKind::ExternCrate { id, .. } => {
                    let def_id = self.local_def_id(id);
                    if self.extern_crate_map.get(&def_id).map_or(true, |&cnum| {
                        !tcx.is_compiler_builtins(cnum)
                            && !tcx.is_panic_runtime(cnum)
                            && !tcx.has_global_allocator(cnum)
                            && !tcx.has_panic_handler(cnum)
                    }) {
                        maybe_unused_extern_crates.insert(id, import.span);
                    }
                }
                ImportKind::MacroUse => {
                    let msg = "unused `#[macro_use]` import";
                    self.lint_buffer.buffer_lint(UNUSED_IMPORTS, import.root_id, import.span, msg);
                }
                _ => {}
            }
        }

        let mut visitor = UnusedImportCheckVisitor {
            r: self,
            unused_imports: Default::default(),
            extern_crate_items: Default::default(),
            base_use_tree: None,
            base_id: ast::DUMMY_NODE_ID,
            item_span: DUMMY_SP,
            base_use_is_pub: false,
        };
        visit::walk_crate(&mut visitor, krate);

        for unused in visitor.unused_imports.values() {
            let mut fixes = Vec::new();
            let mut spans = match calc_unused_spans(unused, unused.use_tree, unused.use_tree_id) {
                UnusedSpanResult::Used => continue,
                UnusedSpanResult::FlatUnused(span, remove) => {
                    fixes.push((remove, String::new()));
                    vec![span]
                }
                UnusedSpanResult::NestedFullUnused(spans, remove) => {
                    fixes.push((remove, String::new()));
                    spans
                }
                UnusedSpanResult::NestedPartialUnused(spans, remove) => {
                    for fix in &remove {
                        fixes.push((*fix, String::new()));
                    }
                    spans
                }
            };

            let len = spans.len();
            spans.sort();
            let ms = MultiSpan::from_spans(spans.clone());
            let mut span_snippets = spans
                .iter()
                .filter_map(|s| match tcx.sess.source_map().span_to_snippet(*s) {
                    Ok(s) => Some(format!("`{}`", s)),
                    _ => None,
                })
                .collect::<Vec<String>>();
            span_snippets.sort();
            let msg = format!(
                "unused import{}{}",
                pluralize!(len),
                if !span_snippets.is_empty() {
                    format!(": {}", span_snippets.join(", "))
                } else {
                    String::new()
                }
            );

            let fix_msg = if fixes.len() == 1 && fixes[0].0 == unused.item_span {
                "remove the whole `use` item"
            } else if spans.len() > 1 {
                "remove the unused imports"
            } else {
                "remove the unused import"
            };

            // If we are in the `--test` mode, suppress a help that adds the `#[cfg(test)]`
            // attribute; however, if not, suggest adding the attribute. There is no way to
            // retrieve attributes here because we do not have a `TyCtxt` yet.
            let test_module_span = if tcx.sess.is_test_crate() {
                None
            } else {
                let parent_module = visitor.r.get_nearest_non_block_module(
                    visitor.r.local_def_id(unused.use_tree_id).to_def_id(),
                );
                match module_to_string(parent_module) {
                    Some(module)
                        if module == "test"
                            || module == "tests"
                            || module.starts_with("test_")
                            || module.starts_with("tests_")
                            || module.ends_with("_test")
                            || module.ends_with("_tests") =>
                    {
                        Some(parent_module.span)
                    }
                    _ => None,
                }
            };

            visitor.r.lint_buffer.buffer_lint_with_diagnostic(
                UNUSED_IMPORTS,
                unused.use_tree_id,
                ms,
                msg,
                BuiltinLintDiagnostics::UnusedImports(fix_msg.into(), fixes, test_module_span),
            );
        }

        for extern_crate in visitor.extern_crate_items {
            let warn_if_unused = !extern_crate.ident.name.as_str().starts_with('_');

            // If the crate is fully unused, we suggest removing it altogether.
            // We do this in any edition.
            if warn_if_unused {
                if let Some(&span) = maybe_unused_extern_crates.get(&extern_crate.id) {
                    visitor.r.lint_buffer.buffer_lint_with_diagnostic(
                        UNUSED_EXTERN_CRATES,
                        extern_crate.id,
                        span,
                        "unused extern crate",
                        BuiltinLintDiagnostics::UnusedExternCrate {
                            removal_span: extern_crate.span_with_attributes,
                        },
                    );
                    continue;
                }
            }

            // If we are not in Rust 2018 edition, then we don't make any further
            // suggestions.
            if !tcx.sess.rust_2018() {
                continue;
            }

            // If the extern crate has any attributes, they may have funky
            // semantics we can't faithfully represent using `use` (most
            // notably `#[macro_use]`). Ignore it.
            if extern_crate.has_attrs {
                continue;
            }

            // If the extern crate is renamed, then we cannot suggest replacing it with a use as this
            // would not insert the new name into the prelude, where other imports in the crate may be
            // expecting it.
            if extern_crate.renames {
                continue;
            }

            // If the extern crate isn't in the extern prelude,
            // there is no way it can be written as a `use`.
            if !visitor
                .r
                .extern_prelude
                .get(&extern_crate.ident)
                .is_some_and(|entry| !entry.introduced_by_item)
            {
                continue;
            }

            let vis_span = extern_crate
                .vis_span
                .find_ancestor_inside(extern_crate.span)
                .unwrap_or(extern_crate.vis_span);
            let ident_span = extern_crate
                .ident
                .span
                .find_ancestor_inside(extern_crate.span)
                .unwrap_or(extern_crate.ident.span);
            visitor.r.lint_buffer.buffer_lint_with_diagnostic(
                UNUSED_EXTERN_CRATES,
                extern_crate.id,
                extern_crate.span,
                "`extern crate` is not idiomatic in the new edition",
                BuiltinLintDiagnostics::ExternCrateNotIdiomatic { vis_span, ident_span },
            );
        }
    }
}
