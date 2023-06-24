use clippy_utils::{
    diagnostics::span_lint_and_help,
    is_from_proc_macro,
    msrvs::{self, Msrv},
    path_to_local,
};
use itertools::Itertools;
use rustc_hir::{Expr, ExprKind, Node, Pat};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::{lint::in_external_macro, ty};
use rustc_session::{declare_tool_lint, impl_lint_pass};
use std::iter::once;

declare_clippy_lint! {
    /// ### What it does
    /// Checks for tuple<=>array conversions that are not done with `.into()`.
    ///
    /// ### Why is this bad?
    /// It's overly complex. `.into()` works for tuples<=>arrays with less than 13 elements and
    /// conveys the intent a lot better, while also leaving less room for bugs!
    ///
    /// ### Example
    /// ```rust,ignore
    /// let t1 = &[(1, 2), (3, 4)];
    /// let v1: Vec<[u32; 2]> = t1.iter().map(|&(a, b)| [a, b]).collect();
    /// ```
    /// Use instead:
    /// ```rust,ignore
    /// let t1 = &[(1, 2), (3, 4)];
    /// let v1: Vec<[u32; 2]> = t1.iter().map(|&t| t.into()).collect();
    /// ```
    #[clippy::version = "1.72.0"]
    pub TUPLE_ARRAY_CONVERSIONS,
    complexity,
    "checks for tuple<=>array conversions that are not done with `.into()`"
}
impl_lint_pass!(TupleArrayConversions => [TUPLE_ARRAY_CONVERSIONS]);

#[derive(Clone)]
pub struct TupleArrayConversions {
    pub msrv: Msrv,
}

impl LateLintPass<'_> for TupleArrayConversions {
    fn check_expr<'tcx>(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if !in_external_macro(cx.sess(), expr.span) && self.msrv.meets(msrvs::TUPLE_ARRAY_CONVERSIONS) {
            _ = check_array(cx, expr) || check_tuple(cx, expr);
        }
    }

    extract_msrv_attr!(LateContext);
}

fn check_array<'tcx>(cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) -> bool {
    let ExprKind::Array(elements) = expr.kind else {
        return false;
    };
    if !(1..=12).contains(&elements.len()) {
        return false;
    }

    if let Some(locals) = path_to_locals(cx, &elements.iter().collect_vec())
        && let [first, rest @ ..] = &*locals
        && let Node::Pat(first_pat) = first
        && let first_id = parent_pat(cx, first_pat).hir_id
        && rest.iter().chain(once(first)).all(|local| {
            if let Node::Pat(pat) = local
                && let parent = parent_pat(cx, pat)
                && parent.hir_id == first_id
            {
                return matches!(
                    cx.typeck_results().pat_ty(parent).peel_refs().kind(),
                    ty::Tuple(len) if len.len() == elements.len()
                );
            }

            false
        })
    {
        return emit_lint(cx, expr, ToType::Array);
    }

    if let Some(elements) = elements
            .iter()
            .map(|expr| {
                if let ExprKind::Field(path, _) = expr.kind {
                    return Some(path);
                };

                None
            })
            .collect::<Option<Vec<&Expr<'_>>>>()
        && let Some(locals) = path_to_locals(cx, &elements)
        && let [first, rest @ ..] = &*locals
        && let Node::Pat(first_pat) = first
        && let first_id = parent_pat(cx, first_pat).hir_id
        && rest.iter().chain(once(first)).all(|local| {
            if let Node::Pat(pat) = local
                && let parent = parent_pat(cx, pat)
                && parent.hir_id == first_id
            {
                return matches!(
                    cx.typeck_results().pat_ty(parent).peel_refs().kind(),
                    ty::Tuple(len) if len.len() == elements.len()
                );
            }

            false
        })
    {
        return emit_lint(cx, expr, ToType::Array);
    }

    false
}

#[expect(clippy::cast_possible_truncation)]
fn check_tuple<'tcx>(cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) -> bool {
    let ExprKind::Tup(elements) = expr.kind else {
        return false;
    };
    if !(1..=12).contains(&elements.len()) {
        return false;
    };

    if let Some(locals) = path_to_locals(cx, &elements.iter().collect_vec())
        && let [first, rest @ ..] = &*locals
        && let Node::Pat(first_pat) = first
        && let first_id = parent_pat(cx, first_pat).hir_id
        && rest.iter().chain(once(first)).all(|local| {
            if let Node::Pat(pat) = local
                && let parent = parent_pat(cx, pat)
                && parent.hir_id == first_id
            {
                return matches!(
                    cx.typeck_results().pat_ty(parent).peel_refs().kind(),
                    ty::Array(_, len) if len.eval_target_usize(cx.tcx, cx.param_env) as usize == elements.len()
                );
            }

            false
        })
    {
        return emit_lint(cx, expr, ToType::Tuple);
    }

    if let Some(elements) = elements
            .iter()
            .map(|expr| {
                if let ExprKind::Index(path, _) = expr.kind {
                    return Some(path);
                };

                None
            })
            .collect::<Option<Vec<&Expr<'_>>>>()
        && let Some(locals) = path_to_locals(cx, &elements)
        && let [first, rest @ ..] = &*locals
        && let Node::Pat(first_pat) = first
        && let first_id = parent_pat(cx, first_pat).hir_id
        && rest.iter().chain(once(first)).all(|local| {
            if let Node::Pat(pat) = local
                && let parent = parent_pat(cx, pat)
                && parent.hir_id == first_id
            {
                return matches!(
                    cx.typeck_results().pat_ty(parent).peel_refs().kind(),
                    ty::Array(_, len) if len.eval_target_usize(cx.tcx, cx.param_env) as usize == elements.len()
                );
            }

            false
        })
    {
        return emit_lint(cx, expr, ToType::Tuple);
    }

    false
}

/// Walks up the `Pat` until it's reached the final containing `Pat`.
fn parent_pat<'tcx>(cx: &LateContext<'tcx>, start: &'tcx Pat<'tcx>) -> &'tcx Pat<'tcx> {
    let mut end = start;
    for (_, node) in cx.tcx.hir().parent_iter(start.hir_id) {
        if let Node::Pat(pat) = node {
            end = pat;
        } else {
            break;
        }
    }
    end
}

fn path_to_locals<'tcx>(cx: &LateContext<'tcx>, exprs: &[&'tcx Expr<'tcx>]) -> Option<Vec<Node<'tcx>>> {
    exprs
        .iter()
        .map(|element| path_to_local(element).and_then(|local| cx.tcx.hir().find(local)))
        .collect()
}

#[derive(Clone, Copy)]
enum ToType {
    Array,
    Tuple,
}

impl ToType {
    fn msg(self) -> &'static str {
        match self {
            ToType::Array => "it looks like you're trying to convert a tuple to an array",
            ToType::Tuple => "it looks like you're trying to convert an array to a tuple",
        }
    }

    fn help(self) -> &'static str {
        match self {
            ToType::Array => "use `.into()` instead, or `<[T; N]>::from` if type annotations are needed",
            ToType::Tuple => "use `.into()` instead, or `<(T0, T1, ..., Tn)>::from` if type annotations are needed",
        }
    }
}

fn emit_lint<'tcx>(cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>, to_type: ToType) -> bool {
    if !is_from_proc_macro(cx, expr) {
        span_lint_and_help(
            cx,
            TUPLE_ARRAY_CONVERSIONS,
            expr.span,
            to_type.msg(),
            None,
            to_type.help(),
        );

        return true;
    }

    false
}
