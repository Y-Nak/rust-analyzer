//! Type inference for patterns.

use std::iter::repeat;
use std::sync::Arc;

use hir_def::{
    expr::{BindingAnnotation, Expr, Literal, Pat, PatId, RecordFieldPat},
    path::Path,
    type_ref::Mutability,
    FieldId,
};
use hir_expand::name::Name;
use test_utils::mark;

use super::{BindingMode, Expectation, InferenceContext};
use crate::{utils::variant_data, Substs, Ty};

impl<'a> InferenceContext<'a> {
    fn infer_tuple_struct_pat(
        &mut self,
        path: Option<&Path>,
        subpats: &[PatId],
        expected: &Ty,
        default_bm: BindingMode,
        id: PatId,
        ellipsis: Option<usize>,
    ) -> Ty {
        let (ty, def) = self.resolve_variant(path);
        let var_data = def.map(|it| variant_data(self.db.upcast(), it));
        if let Some(variant) = def {
            self.write_variant_resolution(id.into(), variant);
        }
        self.unify(&ty, expected);

        let substs = ty.substs().cloned().unwrap_or_else(Substs::empty);

        let field_tys = def.map(|it| self.db.field_types(it)).unwrap_or_default();
        let (pre, post) = match ellipsis {
            Some(idx) => subpats.split_at(idx),
            None => (&subpats[..], &[][..]),
        };
        let post_idx_offset = field_tys.iter().count() - post.len();

        let pre_iter = pre.iter().enumerate();
        let post_iter = (post_idx_offset..).zip(post.iter());
        for (i, &subpat) in pre_iter.chain(post_iter) {
            let expected_ty = var_data
                .as_ref()
                .and_then(|d| d.field(&Name::new_tuple_field(i)))
                .map_or(Ty::Unknown, |field| field_tys[field].clone().subst(&substs));
            let expected_ty = self.normalize_associated_types_in(expected_ty);
            self.infer_pat(subpat, &expected_ty, default_bm);
        }

        ty
    }

    fn infer_record_pat(
        &mut self,
        path: Option<&Path>,
        subpats: &[RecordFieldPat],
        expected: &Ty,
        default_bm: BindingMode,
        id: PatId,
    ) -> Ty {
        let (ty, def) = self.resolve_variant(path);
        let var_data = def.map(|it| variant_data(self.db.upcast(), it));
        if let Some(variant) = def {
            self.write_variant_resolution(id.into(), variant);
        }

        self.unify(&ty, expected);

        let substs = ty.substs().cloned().unwrap_or_else(Substs::empty);

        let field_tys = def.map(|it| self.db.field_types(it)).unwrap_or_default();
        for subpat in subpats {
            let matching_field = var_data.as_ref().and_then(|it| it.field(&subpat.name));
            if let Some(local_id) = matching_field {
                let field_def = FieldId { parent: def.unwrap(), local_id };
                self.result.record_pat_field_resolutions.insert(subpat.pat, field_def);
            }

            let expected_ty =
                matching_field.map_or(Ty::Unknown, |field| field_tys[field].clone().subst(&substs));
            let expected_ty = self.normalize_associated_types_in(expected_ty);
            self.infer_pat(subpat.pat, &expected_ty, default_bm);
        }

        ty
    }

    pub(super) fn infer_pat(
        &mut self,
        pat: PatId,
        mut expected: &Ty,
        mut default_bm: BindingMode,
    ) -> Ty {
        let body = Arc::clone(&self.body); // avoid borrow checker problem

        if is_non_ref_pat(&body, pat) {
            while let Some((inner, mutability)) = expected.as_reference() {
                expected = inner;
                default_bm = match default_bm {
                    BindingMode::Move => BindingMode::Ref(mutability),
                    BindingMode::Ref(Mutability::Shared) => BindingMode::Ref(Mutability::Shared),
                    BindingMode::Ref(Mutability::Mut) => BindingMode::Ref(mutability),
                }
            }
        } else if let Pat::Ref { .. } = &body[pat] {
            mark::hit!(match_ergonomics_ref);
            // When you encounter a `&pat` pattern, reset to Move.
            // This is so that `w` is by value: `let (_, &w) = &(1, &2);`
            default_bm = BindingMode::Move;
        }

        // Lose mutability.
        let default_bm = default_bm;
        let expected = expected;

        let ty = match &body[pat] {
            &Pat::Tuple { ref args, ellipsis } => {
                let expectations = match expected.as_tuple() {
                    Some(parameters) => &*parameters.0,
                    _ => &[],
                };

                let (pre, post) = match ellipsis {
                    Some(idx) => args.split_at(idx),
                    None => (&args[..], &[][..]),
                };
                let n_uncovered_patterns = expectations.len().saturating_sub(args.len());
                let mut expectations_iter = expectations.iter().chain(repeat(&Ty::Unknown));
                let mut infer_pat = |(&pat, ty)| self.infer_pat(pat, ty, default_bm);

                let mut inner_tys = Vec::with_capacity(n_uncovered_patterns + args.len());
                inner_tys.extend(pre.iter().zip(expectations_iter.by_ref()).map(&mut infer_pat));
                inner_tys.extend(expectations_iter.by_ref().take(n_uncovered_patterns).cloned());
                inner_tys.extend(post.iter().zip(expectations_iter).map(infer_pat));

                Ty::Tuple(inner_tys.len(), Substs(inner_tys.into()))
            }
            Pat::Or(ref pats) => {
                if let Some((first_pat, rest)) = pats.split_first() {
                    let ty = self.infer_pat(*first_pat, expected, default_bm);
                    for pat in rest {
                        self.infer_pat(*pat, expected, default_bm);
                    }
                    ty
                } else {
                    Ty::Unknown
                }
            }
            Pat::Ref { pat, mutability } => {
                let expectation = match expected.as_reference() {
                    Some((inner_ty, exp_mut)) => {
                        if *mutability != exp_mut {
                            // FIXME: emit type error?
                        }
                        inner_ty
                    }
                    _ => &Ty::Unknown,
                };
                let subty = self.infer_pat(*pat, expectation, default_bm);
                Ty::Ref(*mutability, Substs::single(subty))
            }
            Pat::TupleStruct { path: p, args: subpats, ellipsis } => self.infer_tuple_struct_pat(
                p.as_ref(),
                subpats,
                expected,
                default_bm,
                pat,
                *ellipsis,
            ),
            Pat::Record { path: p, args: fields, ellipsis: _ } => {
                self.infer_record_pat(p.as_ref(), fields, expected, default_bm, pat)
            }
            Pat::Path(path) => {
                // FIXME use correct resolver for the surrounding expression
                let resolver = self.resolver.clone();
                self.infer_path(&resolver, &path, pat.into()).unwrap_or(Ty::Unknown)
            }
            Pat::Bind { mode, name: _, subpat } => {
                let mode = if mode == &BindingAnnotation::Unannotated {
                    default_bm
                } else {
                    BindingMode::convert(*mode)
                };
                let inner_ty = if let Some(subpat) = subpat {
                    self.infer_pat(*subpat, expected, default_bm)
                } else {
                    expected.clone()
                };
                let inner_ty = self.insert_type_vars_shallow(inner_ty);

                let bound_ty = match mode {
                    BindingMode::Ref(mutability) => {
                        Ty::Ref(mutability, Substs::single(inner_ty.clone()))
                    }
                    BindingMode::Move => inner_ty.clone(),
                };
                let bound_ty = self.resolve_ty_as_possible(bound_ty);
                self.write_pat_ty(pat, bound_ty);
                return inner_ty;
            }
            Pat::Slice { prefix, slice, suffix } => {
                let (container_ty, elem_ty): (fn(_) -> _, _) = match &expected {
                    Ty::Array(st) => (Ty::Array, st.as_single().clone()),
                    Ty::Slice(st) => (Ty::Slice, st.as_single().clone()),
                    _ => (Ty::Slice, Ty::Unknown),
                };

                for pat_id in prefix.iter().chain(suffix) {
                    self.infer_pat(*pat_id, &elem_ty, default_bm);
                }

                let pat_ty = container_ty(Substs::single(elem_ty));
                if let Some(slice_pat_id) = slice {
                    self.infer_pat(*slice_pat_id, &pat_ty, default_bm);
                }

                pat_ty
            }
            Pat::Wild => expected.clone(),
            Pat::Range { start, end } => {
                let start_ty = self.infer_expr(*start, &Expectation::has_type(expected.clone()));
                let end_ty = self.infer_expr(*end, &Expectation::has_type(start_ty));
                end_ty
            }
            Pat::Lit(expr) => self.infer_expr(*expr, &Expectation::has_type(expected.clone())),
            Pat::Box { inner } => match self.resolve_boxed_box() {
                Some(box_adt) => {
                    let inner_expected = match expected.as_adt() {
                        Some((adt, substs)) if adt == box_adt => substs.as_single(),
                        _ => &Ty::Unknown,
                    };

                    let inner_ty = self.infer_pat(*inner, inner_expected, default_bm);
                    Ty::Adt(box_adt, Substs::single(inner_ty))
                }
                None => Ty::Unknown,
            },
            Pat::ConstBlock(expr) => {
                self.infer_expr(*expr, &Expectation::has_type(expected.clone()))
            }
            Pat::Missing => Ty::Unknown,
        };
        // use a new type variable if we got Ty::Unknown here
        let ty = self.insert_type_vars_shallow(ty);
        if !self.unify(&ty, expected) {
            // FIXME record mismatch, we need to change the type of self.type_mismatches for that
        }
        let ty = self.resolve_ty_as_possible(ty);
        self.write_pat_ty(pat, ty.clone());
        ty
    }
}

fn is_non_ref_pat(body: &hir_def::body::Body, pat: PatId) -> bool {
    match &body[pat] {
        Pat::Tuple { .. }
        | Pat::TupleStruct { .. }
        | Pat::Record { .. }
        | Pat::Range { .. }
        | Pat::Slice { .. } => true,
        Pat::Or(pats) => pats.iter().all(|p| is_non_ref_pat(body, *p)),
        // FIXME: ConstBlock/Path/Lit might actually evaluate to ref, but inference is unimplemented.
        Pat::Path(..) => true,
        Pat::ConstBlock(..) => true,
        Pat::Lit(expr) => match body[*expr] {
            Expr::Literal(Literal::String(..)) => false,
            _ => true,
        },
        Pat::Wild | Pat::Bind { .. } | Pat::Ref { .. } | Pat::Box { .. } | Pat::Missing => false,
    }
}
