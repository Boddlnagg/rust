// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/*!
 *
 * # Compilation of match statements
 *
 * I will endeavor to explain the code as best I can.  I have only a loose
 * understanding of some parts of it.
 *
 * ## Matching
 *
 * The basic state of the code is maintained in an array `m` of `Match`
 * objects.  Each `Match` describes some list of patterns, all of which must
 * match against the current list of values.  If those patterns match, then
 * the arm listed in the match is the correct arm.  A given arm may have
 * multiple corresponding match entries, one for each alternative that
 * remains.  As we proceed these sets of matches are adjusted by the various
 * `enter_XXX()` functions, each of which adjusts the set of options given
 * some information about the value which has been matched.
 *
 * So, initially, there is one value and N matches, each of which have one
 * constituent pattern.  N here is usually the number of arms but may be
 * greater, if some arms have multiple alternatives.  For example, here:
 *
 *     enum Foo { A, B(int), C(uint, uint) }
 *     match foo {
 *         A => ...,
 *         B(x) => ...,
 *         C(1u, 2) => ...,
 *         C(_) => ...
 *     }
 *
 * The value would be `foo`.  There would be four matches, each of which
 * contains one pattern (and, in one case, a guard).  We could collect the
 * various options and then compile the code for the case where `foo` is an
 * `A`, a `B`, and a `C`.  When we generate the code for `C`, we would (1)
 * drop the two matches that do not match a `C` and (2) expand the other two
 * into two patterns each.  In the first case, the two patterns would be `1u`
 * and `2`, and the in the second case the _ pattern would be expanded into
 * `_` and `_`.  The two values are of course the arguments to `C`.
 *
 * Here is a quick guide to the various functions:
 *
 * - `compile_submatch()`: The main workhouse.  It takes a list of values and
 *   a list of matches and finds the various possibilities that could occur.
 *
 * - `enter_XXX()`: modifies the list of matches based on some information
 *   about the value that has been matched.  For example,
 *   `enter_rec_or_struct()` adjusts the values given that a record or struct
 *   has been matched.  This is an infallible pattern, so *all* of the matches
 *   must be either wildcards or record/struct patterns.  `enter_opt()`
 *   handles the fallible cases, and it is correspondingly more complex.
 *
 * ## Bindings
 *
 * We store information about the bound variables for each arm as part of the
 * per-arm `ArmData` struct.  There is a mapping from identifiers to
 * `BindingInfo` structs.  These structs contain the mode/id/type of the
 * binding, but they also contain an LLVM value which points at an alloca
 * called `llmatch`. For by value bindings that are Copy, we also create
 * an extra alloca that we copy the matched value to so that any changes
 * we do to our copy is not reflected in the original and vice-versa.
 * We don't do this if it's a move since the original value can't be used
 * and thus allowing us to cheat in not creating an extra alloca.
 *
 * The `llmatch` binding always stores a pointer into the value being matched
 * which points at the data for the binding.  If the value being matched has
 * type `T`, then, `llmatch` will point at an alloca of type `T*` (and hence
 * `llmatch` has type `T**`).  So, if you have a pattern like:
 *
 *    let a: A = ...;
 *    let b: B = ...;
 *    match (a, b) { (ref c, d) => { ... } }
 *
 * For `c` and `d`, we would generate allocas of type `C*` and `D*`
 * respectively.  These are called the `llmatch`.  As we match, when we come
 * up against an identifier, we store the current pointer into the
 * corresponding alloca.
 *
 * Once a pattern is completely matched, and assuming that there is no guard
 * pattern, we will branch to a block that leads to the body itself.  For any
 * by-value bindings, this block will first load the ptr from `llmatch` (the
 * one of type `D*`) and then load a second time to get the actual value (the
 * one of type `D`). For by ref bindings, the value of the local variable is
 * simply the first alloca.
 *
 * So, for the example above, we would generate a setup kind of like this:
 *
 *        +-------+
 *        | Entry |
 *        +-------+
 *            |
 *        +--------------------------------------------+
 *        | llmatch_c = (addr of first half of tuple)  |
 *        | llmatch_d = (addr of second half of tuple) |
 *        +--------------------------------------------+
 *            |
 *        +--------------------------------------+
 *        | *llbinding_d = **llmatch_d           |
 *        +--------------------------------------+
 *
 * If there is a guard, the situation is slightly different, because we must
 * execute the guard code.  Moreover, we need to do so once for each of the
 * alternatives that lead to the arm, because if the guard fails, they may
 * have different points from which to continue the search. Therefore, in that
 * case, we generate code that looks more like:
 *
 *        +-------+
 *        | Entry |
 *        +-------+
 *            |
 *        +-------------------------------------------+
 *        | llmatch_c = (addr of first half of tuple) |
 *        | llmatch_d = (addr of first half of tuple) |
 *        +-------------------------------------------+
 *            |
 *        +-------------------------------------------------+
 *        | *llbinding_d = **llmatch_d                      |
 *        | check condition                                 |
 *        | if false { goto next case }                     |
 *        | if true { goto body }                           |
 *        +-------------------------------------------------+
 *
 * The handling for the cleanups is a bit... sensitive.  Basically, the body
 * is the one that invokes `add_clean()` for each binding.  During the guard
 * evaluation, we add temporary cleanups and revoke them after the guard is
 * evaluated (it could fail, after all). Note that guards and moves are
 * just plain incompatible.
 *
 * Some relevant helper functions that manage bindings:
 * - `create_bindings_map()`
 * - `insert_lllocals()`
 *
 *
 * ## Notes on vector pattern matching.
 *
 * Vector pattern matching is surprisingly tricky. The problem is that
 * the structure of the vector isn't fully known, and slice matches
 * can be done on subparts of it.
 *
 * The way that vector pattern matches are dealt with, then, is as
 * follows. First, we make the actual condition associated with a
 * vector pattern simply a vector length comparison. So the pattern
 * [1, .. x] gets the condition "vec len >= 1", and the pattern
 * [.. x] gets the condition "vec len >= 0". The problem here is that
 * having the condition "vec len >= 1" hold clearly does not mean that
 * only a pattern that has exactly that condition will match. This
 * means that it may well be the case that a condition holds, but none
 * of the patterns matching that condition match; to deal with this,
 * when doing vector length matches, we have match failures proceed to
 * the next condition to check.
 *
 * There are a couple more subtleties to deal with. While the "actual"
 * condition associated with vector length tests is simply a test on
 * the vector length, the actual vec_len Opt entry contains more
 * information used to restrict which matches are associated with it.
 * So that all matches in a submatch are matching against the same
 * values from inside the vector, they are split up by how many
 * elements they match at the front and at the back of the vector. In
 * order to make sure that arms are properly checked in order, even
 * with the overmatching conditions, each vec_len Opt entry is
 * associated with a range of matches.
 * Consider the following:
 *
 *   match &[1, 2, 3] {
 *       [1, 1, .. _] => 0,
 *       [1, 2, 2, .. _] => 1,
 *       [1, 2, 3, .. _] => 2,
 *       [1, 2, .. _] => 3,
 *       _ => 4
 *   }
 * The proper arm to match is arm 2, but arms 0 and 3 both have the
 * condition "len >= 2". If arm 3 was lumped in with arm 0, then the
 * wrong branch would be taken. Instead, vec_len Opts are associated
 * with a contiguous range of matches that have the same "shape".
 * This is sort of ugly and requires a bunch of special handling of
 * vec_len options.
 *
 */

#![allow(non_camel_case_types)]

use back::abi;
use driver::config::FullDebugInfo;
use lib::llvm::{llvm, ValueRef, BasicBlockRef};
use middle::const_eval;
use middle::def;
use middle::lang_items::{UniqStrEqFnLangItem, StrEqFnLangItem};
use middle::pat_util::*;
use middle::resolve::DefMap;
use middle::trans::adt;
use middle::trans::base::*;
use middle::trans::build::*;
use middle::trans::callee;
use middle::trans::cleanup;
use middle::trans::cleanup::CleanupMethods;
use middle::trans::common::*;
use middle::trans::consts;
use middle::trans::controlflow;
use middle::trans::datum;
use middle::trans::datum::*;
use middle::trans::expr::Dest;
use middle::trans::expr;
use middle::trans::tvec;
use middle::trans::type_of;
use middle::trans::debuginfo;
use middle::ty;
use util::common::indenter;
use util::ppaux::{Repr, vec_map_to_str};

use std::collections::HashMap;
use std::cell::Cell;
use std::rc::Rc;
use std::gc::{Gc, GC};
use syntax::ast;
use syntax::ast::Ident;
use syntax::ast_util::path_to_ident;
use syntax::ast_util;
use syntax::codemap::{Span, DUMMY_SP};
use syntax::parse::token::InternedString;

// An option identifying a literal: either a unit-like struct or an
// expression.
enum Lit {
    UnitLikeStructLit(ast::NodeId),    // the node ID of the pattern
    ExprLit(Gc<ast::Expr>),
    ConstLit(ast::DefId),              // the def ID of the constant
}

#[deriving(PartialEq)]
pub enum VecLenOpt {
    vec_len_eq,
    vec_len_ge(/* length of prefix */uint)
}

// An option identifying a branch (either a literal, an enum variant or a
// range)
enum Opt {
    lit(Lit),
    var(ty::Disr, Rc<adt::Repr>),
    range(Gc<ast::Expr>, Gc<ast::Expr>),
    vec_len(/* length */ uint, VecLenOpt, /*range of matches*/(uint, uint))
}

fn lit_to_expr(tcx: &ty::ctxt, a: &Lit) -> Gc<ast::Expr> {
    match *a {
        ExprLit(existing_a_expr) => existing_a_expr,
        ConstLit(a_const) => const_eval::lookup_const_by_id(tcx, a_const).unwrap(),
        UnitLikeStructLit(_) => fail!("lit_to_expr: unexpected struct lit"),
    }
}

fn opt_eq(tcx: &ty::ctxt, a: &Opt, b: &Opt) -> bool {
    match (a, b) {
        (&lit(UnitLikeStructLit(a)), &lit(UnitLikeStructLit(b))) => a == b,
        (&lit(a), &lit(b)) => {
            let a_expr = lit_to_expr(tcx, &a);
            let b_expr = lit_to_expr(tcx, &b);
            match const_eval::compare_lit_exprs(tcx, &*a_expr, &*b_expr) {
                Some(val1) => val1 == 0,
                None => fail!("compare_list_exprs: type mismatch"),
            }
        }
        (&range(ref a1, ref a2), &range(ref b1, ref b2)) => {
            let m1 = const_eval::compare_lit_exprs(tcx, &**a1, &**b1);
            let m2 = const_eval::compare_lit_exprs(tcx, &**a2, &**b2);
            match (m1, m2) {
                (Some(val1), Some(val2)) => (val1 == 0 && val2 == 0),
                _ => fail!("compare_list_exprs: type mismatch"),
            }
        }
        (&var(a, _), &var(b, _)) => a == b,
        (&vec_len(a1, a2, _), &vec_len(b1, b2, _)) =>
            a1 == b1 && a2 == b2,
        _ => false
    }
}

pub enum opt_result<'a> {
    single_result(Result<'a>),
    lower_bound(Result<'a>),
    range_result(Result<'a>, Result<'a>),
}

fn trans_opt<'a>(bcx: &'a Block<'a>, o: &Opt) -> opt_result<'a> {
    let _icx = push_ctxt("match::trans_opt");
    let ccx = bcx.ccx();
    let mut bcx = bcx;
    match *o {
        lit(ExprLit(ref lit_expr)) => {
            let lit_datum = unpack_datum!(bcx, expr::trans(bcx, &**lit_expr));
            let lit_datum = lit_datum.assert_rvalue(bcx); // literals are rvalues
            let lit_datum = unpack_datum!(bcx, lit_datum.to_appropriate_datum(bcx));
            return single_result(Result::new(bcx, lit_datum.val));
        }
        lit(UnitLikeStructLit(pat_id)) => {
            let struct_ty = ty::node_id_to_type(bcx.tcx(), pat_id);
            let datum = datum::rvalue_scratch_datum(bcx, struct_ty, "");
            return single_result(Result::new(bcx, datum.val));
        }
        lit(l @ ConstLit(ref def_id)) => {
            let lit_ty = ty::node_id_to_type(bcx.tcx(), lit_to_expr(bcx.tcx(), &l).id);
            let (llval, _) = consts::get_const_val(bcx.ccx(), *def_id);
            let lit_datum = immediate_rvalue(llval, lit_ty);
            let lit_datum = unpack_datum!(bcx, lit_datum.to_appropriate_datum(bcx));
            return single_result(Result::new(bcx, lit_datum.val));
        }
        var(disr_val, ref repr) => {
            return adt::trans_case(bcx, &**repr, disr_val);
        }
        range(ref l1, ref l2) => {
            let (l1, _) = consts::const_expr(ccx, &**l1, true);
            let (l2, _) = consts::const_expr(ccx, &**l2, true);
            return range_result(Result::new(bcx, l1), Result::new(bcx, l2));
        }
        vec_len(n, vec_len_eq, _) => {
            return single_result(Result::new(bcx, C_int(ccx, n as int)));
        }
        vec_len(n, vec_len_ge(_), _) => {
            return lower_bound(Result::new(bcx, C_int(ccx, n as int)));
        }
    }
}

fn variant_opt(bcx: &Block, pat_id: ast::NodeId) -> Opt {
    let ccx = bcx.ccx();
    let def = ccx.tcx.def_map.borrow().get_copy(&pat_id);
    match def {
        def::DefVariant(enum_id, var_id, _) => {
            let variants = ty::enum_variants(ccx.tcx(), enum_id);
            for v in (*variants).iter() {
                if var_id == v.id {
                    return var(v.disr_val,
                               adt::represent_node(bcx, pat_id))
                }
            }
            unreachable!();
        }
        def::DefFn(..) |
        def::DefStruct(_) => {
            return lit(UnitLikeStructLit(pat_id));
        }
        _ => {
            ccx.sess().bug("non-variant or struct in variant_opt()");
        }
    }
}

#[deriving(Clone)]
pub enum TransBindingMode {
    TrByCopy(/* llbinding */ ValueRef),
    TrByMove,
    TrByRef,
}

/**
 * Information about a pattern binding:
 * - `llmatch` is a pointer to a stack slot.  The stack slot contains a
 *   pointer into the value being matched.  Hence, llmatch has type `T**`
 *   where `T` is the value being matched.
 * - `trmode` is the trans binding mode
 * - `id` is the node id of the binding
 * - `ty` is the Rust type of the binding */
 #[deriving(Clone)]
pub struct BindingInfo {
    pub llmatch: ValueRef,
    pub trmode: TransBindingMode,
    pub id: ast::NodeId,
    pub span: Span,
    pub ty: ty::t,
}

type BindingsMap = HashMap<Ident, BindingInfo>;

struct ArmData<'a, 'b> {
    bodycx: &'b Block<'b>,
    arm: &'a ast::Arm,
    bindings_map: BindingsMap
}

/**
 * Info about Match.
 * If all `pats` are matched then arm `data` will be executed.
 * As we proceed `bound_ptrs` are filled with pointers to values to be bound,
 * these pointers are stored in llmatch variables just before executing `data` arm.
 */
struct Match<'a, 'b> {
    pats: Vec<Gc<ast::Pat>>,
    data: &'a ArmData<'a, 'b>,
    bound_ptrs: Vec<(Ident, ValueRef)>
}

impl<'a, 'b> Repr for Match<'a, 'b> {
    fn repr(&self, tcx: &ty::ctxt) -> String {
        if tcx.sess.verbose() {
            // for many programs, this just take too long to serialize
            self.pats.repr(tcx)
        } else {
            format!("{} pats", self.pats.len())
        }
    }
}

fn has_nested_bindings(m: &[Match], col: uint) -> bool {
    for br in m.iter() {
        match br.pats.get(col).node {
            ast::PatIdent(_, _, Some(_)) => return true,
            _ => ()
        }
    }
    return false;
}

fn expand_nested_bindings<'a, 'b>(
                          bcx: &'b Block<'b>,
                          m: &'a [Match<'a, 'b>],
                          col: uint,
                          val: ValueRef)
                          -> Vec<Match<'a, 'b>> {
    debug!("expand_nested_bindings(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    m.iter().map(|br| {
        match br.pats.get(col).node {
            ast::PatIdent(_, ref path, Some(inner)) => {
                let pats = Vec::from_slice(br.pats.slice(0u, col))
                           .append((vec!(inner))
                                   .append(br.pats.slice(col + 1u, br.pats.len())).as_slice());

                let mut bound_ptrs = br.bound_ptrs.clone();
                bound_ptrs.push((path_to_ident(path), val));
                Match {
                    pats: pats,
                    data: &*br.data,
                    bound_ptrs: bound_ptrs
                }
            }
            _ => Match {
                pats: br.pats.clone(),
                data: &*br.data,
                bound_ptrs: br.bound_ptrs.clone()
            }
        }
    }).collect()
}

fn assert_is_binding_or_wild(bcx: &Block, p: Gc<ast::Pat>) {
    if !pat_is_binding_or_wild(&bcx.tcx().def_map, &*p) {
        bcx.sess().span_bug(
            p.span,
            format!("expected an identifier pattern but found p: {}",
                    p.repr(bcx.tcx())).as_slice());
    }
}

type enter_pat<'a> = |Gc<ast::Pat>|: 'a -> Option<Vec<Gc<ast::Pat>>>;

fn enter_match<'a, 'b>(
               bcx: &'b Block<'b>,
               dm: &DefMap,
               m: &'a [Match<'a, 'b>],
               col: uint,
               val: ValueRef,
               e: enter_pat)
               -> Vec<Match<'a, 'b>> {
    debug!("enter_match(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    m.iter().filter_map(|br| {
        e(*br.pats.get(col)).map(|sub| {
            let pats = sub.append(br.pats.slice(0u, col))
                            .append(br.pats.slice(col + 1u, br.pats.len()));

            let this = *br.pats.get(col);
            let mut bound_ptrs = br.bound_ptrs.clone();
            match this.node {
                ast::PatIdent(_, ref path, None) => {
                    if pat_is_binding(dm, &*this) {
                        bound_ptrs.push((path_to_ident(path), val));
                    }
                }
                _ => {}
            }

            Match {
                pats: pats,
                data: br.data,
                bound_ptrs: bound_ptrs
            }
        })
    }).collect()
}

fn enter_default<'a, 'b>(
                 bcx: &'b Block<'b>,
                 dm: &DefMap,
                 m: &'a [Match<'a, 'b>],
                 col: uint,
                 val: ValueRef)
                 -> Vec<Match<'a, 'b>> {
    debug!("enter_default(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    // Collect all of the matches that can match against anything.
    enter_match(bcx, dm, m, col, val, |p| {
        match p.node {
          ast::PatWild | ast::PatWildMulti => Some(Vec::new()),
          ast::PatIdent(_, _, None) if pat_is_binding(dm, &*p) => Some(Vec::new()),
          _ => None
        }
    })
}

// <pcwalton> nmatsakis: what does enter_opt do?
// <pcwalton> in trans/match
// <pcwalton> trans/match.rs is like stumbling around in a dark cave
// <nmatsakis> pcwalton: the enter family of functions adjust the set of
//             patterns as needed
// <nmatsakis> yeah, at some point I kind of achieved some level of
//             understanding
// <nmatsakis> anyhow, they adjust the patterns given that something of that
//             kind has been found
// <nmatsakis> pcwalton: ok, right, so enter_XXX() adjusts the patterns, as I
//             said
// <nmatsakis> enter_match() kind of embodies the generic code
// <nmatsakis> it is provided with a function that tests each pattern to see
//             if it might possibly apply and so forth
// <nmatsakis> so, if you have a pattern like {a: _, b: _, _} and one like _
// <nmatsakis> then _ would be expanded to (_, _)
// <nmatsakis> one spot for each of the sub-patterns
// <nmatsakis> enter_opt() is one of the more complex; it covers the fallible
//             cases
// <nmatsakis> enter_rec_or_struct() or enter_tuple() are simpler, since they
//             are infallible patterns
// <nmatsakis> so all patterns must either be records (resp. tuples) or
//             wildcards

fn enter_opt<'a, 'b>(
             bcx: &'b Block<'b>,
             m: &'a [Match<'a, 'b>],
             opt: &Opt,
             col: uint,
             variant_size: uint,
             val: ValueRef)
             -> Vec<Match<'a, 'b>> {
    debug!("enter_opt(bcx={}, m={}, opt={:?}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           *opt,
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    let tcx = bcx.tcx();
    let dummy = box(GC) ast::Pat {id: 0, node: ast::PatWild, span: DUMMY_SP};
    let mut i = 0;
    enter_match(bcx, &tcx.def_map, m, col, val, |p| {
        let answer = match p.node {
            ast::PatEnum(..) |
            ast::PatIdent(_, _, None) if pat_is_const(&tcx.def_map, &*p) => {
                let const_def = tcx.def_map.borrow().get_copy(&p.id);
                let const_def_id = const_def.def_id();
                if opt_eq(tcx, &lit(ConstLit(const_def_id)), opt) {
                    Some(Vec::new())
                } else {
                    None
                }
            }
            ast::PatEnum(_, ref subpats) => {
                if opt_eq(tcx, &variant_opt(bcx, p.id), opt) {
                    // FIXME: Must we clone?
                    match *subpats {
                        None => Some(Vec::from_elem(variant_size, dummy)),
                        Some(ref subpats) => {
                            Some((*subpats).iter().map(|x| *x).collect())
                        }
                    }
                } else {
                    None
                }
            }
            ast::PatIdent(_, _, None)
                    if pat_is_variant_or_struct(&tcx.def_map, &*p) => {
                if opt_eq(tcx, &variant_opt(bcx, p.id), opt) {
                    Some(Vec::new())
                } else {
                    None
                }
            }
            ast::PatLit(l) => {
                if opt_eq(tcx, &lit(ExprLit(l)), opt) { Some(Vec::new()) }
                else { None }
            }
            ast::PatRange(l1, l2) => {
                if opt_eq(tcx, &range(l1, l2), opt) { Some(Vec::new()) }
                else { None }
            }
            ast::PatStruct(_, ref field_pats, _) => {
                if opt_eq(tcx, &variant_opt(bcx, p.id), opt) {
                    // Look up the struct variant ID.
                    let struct_id;
                    match tcx.def_map.borrow().get_copy(&p.id) {
                        def::DefVariant(_, found_struct_id, _) => {
                            struct_id = found_struct_id;
                        }
                        _ => {
                            tcx.sess.span_bug(p.span, "expected enum variant def");
                        }
                    }

                    // Reorder the patterns into the same order they were
                    // specified in the struct definition. Also fill in
                    // unspecified fields with dummy.
                    let mut reordered_patterns = Vec::new();
                    let r = ty::lookup_struct_fields(tcx, struct_id);
                    for field in r.iter() {
                            match field_pats.iter().find(|p| p.ident.name
                                                         == field.name) {
                                None => reordered_patterns.push(dummy),
                                Some(fp) => reordered_patterns.push(fp.pat)
                            }
                    }
                    Some(reordered_patterns)
                } else {
                    None
                }
            }
            ast::PatVec(ref before, slice, ref after) => {
                let (lo, hi) = match *opt {
                    vec_len(_, _, (lo, hi)) => (lo, hi),
                    _ => tcx.sess.span_bug(p.span,
                                           "vec pattern but not vec opt")
                };

                match slice {
                    Some(slice) if i >= lo && i <= hi => {
                        let n = before.len() + after.len();
                        let this_opt = vec_len(n, vec_len_ge(before.len()),
                                               (lo, hi));
                        if opt_eq(tcx, &this_opt, opt) {
                            let mut new_before = Vec::new();
                            for pat in before.iter() {
                                new_before.push(*pat);
                            }
                            new_before.push(slice);
                            for pat in after.iter() {
                                new_before.push(*pat);
                            }
                            Some(new_before)
                        } else {
                            None
                        }
                    }
                    None if i >= lo && i <= hi => {
                        let n = before.len();
                        if opt_eq(tcx, &vec_len(n, vec_len_eq, (lo,hi)), opt) {
                            let mut new_before = Vec::new();
                            for pat in before.iter() {
                                new_before.push(*pat);
                            }
                            Some(new_before)
                        } else {
                            None
                        }
                    }
                    _ => None
                }
            }
            _ => {
                assert_is_binding_or_wild(bcx, p);
                Some(Vec::from_elem(variant_size, dummy))
            }
        };
        i += 1;
        answer
    })
}

fn enter_rec_or_struct<'a, 'b>(
                       bcx: &'b Block<'b>,
                       dm: &DefMap,
                       m: &'a [Match<'a, 'b>],
                       col: uint,
                       fields: &[ast::Ident],
                       val: ValueRef)
                       -> Vec<Match<'a, 'b>> {
    debug!("enter_rec_or_struct(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    let dummy = box(GC) ast::Pat {id: 0, node: ast::PatWild, span: DUMMY_SP};
    enter_match(bcx, dm, m, col, val, |p| {
        match p.node {
            ast::PatStruct(_, ref fpats, _) => {
                let mut pats = Vec::new();
                for fname in fields.iter() {
                    match fpats.iter().find(|p| p.ident.name == fname.name) {
                        None => pats.push(dummy),
                        Some(pat) => pats.push(pat.pat)
                    }
                }
                Some(pats)
            }
            _ => {
                assert_is_binding_or_wild(bcx, p);
                Some(Vec::from_elem(fields.len(), dummy))
            }
        }
    })
}

fn enter_tup<'a, 'b>(
             bcx: &'b Block<'b>,
             dm: &DefMap,
             m: &'a [Match<'a, 'b>],
             col: uint,
             val: ValueRef,
             n_elts: uint)
             -> Vec<Match<'a, 'b>> {
    debug!("enter_tup(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    let dummy = box(GC) ast::Pat {id: 0, node: ast::PatWild, span: DUMMY_SP};
    enter_match(bcx, dm, m, col, val, |p| {
        match p.node {
            ast::PatTup(ref elts) => {
                let mut new_elts = Vec::new();
                for elt in elts.iter() {
                    new_elts.push((*elt).clone())
                }
                Some(new_elts)
            }
            _ => {
                assert_is_binding_or_wild(bcx, p);
                Some(Vec::from_elem(n_elts, dummy))
            }
        }
    })
}

fn enter_tuple_struct<'a, 'b>(
                      bcx: &'b Block<'b>,
                      dm: &DefMap,
                      m: &'a [Match<'a, 'b>],
                      col: uint,
                      val: ValueRef,
                      n_elts: uint)
                      -> Vec<Match<'a, 'b>> {
    debug!("enter_tuple_struct(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    let dummy = box(GC) ast::Pat {id: 0, node: ast::PatWild, span: DUMMY_SP};
    enter_match(bcx, dm, m, col, val, |p| {
        match p.node {
            ast::PatEnum(_, Some(ref elts)) => {
                Some(elts.iter().map(|x| (*x)).collect())
            }
            ast::PatEnum(_, None) => {
                Some(Vec::from_elem(n_elts, dummy))
            }
            _ => {
                assert_is_binding_or_wild(bcx, p);
                Some(Vec::from_elem(n_elts, dummy))
            }
        }
    })
}

fn enter_uniq<'a, 'b>(
              bcx: &'b Block<'b>,
              dm: &DefMap,
              m: &'a [Match<'a, 'b>],
              col: uint,
              val: ValueRef)
              -> Vec<Match<'a, 'b>> {
    debug!("enter_uniq(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    let dummy = box(GC) ast::Pat {id: 0, node: ast::PatWild, span: DUMMY_SP};
    enter_match(bcx, dm, m, col, val, |p| {
        match p.node {
            ast::PatBox(sub) => {
                Some(vec!(sub))
            }
            _ => {
                assert_is_binding_or_wild(bcx, p);
                Some(vec!(dummy))
            }
        }
    })
}

fn enter_region<'a, 'b>(
                bcx: &'b Block<'b>,
                dm: &DefMap,
                m: &'a [Match<'a, 'b>],
                col: uint,
                val: ValueRef)
                -> Vec<Match<'a, 'b>> {
    debug!("enter_region(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_str(val));
    let _indenter = indenter();

    let dummy = box(GC) ast::Pat { id: 0, node: ast::PatWild, span: DUMMY_SP };
    enter_match(bcx, dm, m, col, val, |p| {
        match p.node {
            ast::PatRegion(sub) => {
                Some(vec!(sub))
            }
            _ => {
                assert_is_binding_or_wild(bcx, p);
                Some(vec!(dummy))
            }
        }
    })
}

// Returns the options in one column of matches. An option is something that
// needs to be conditionally matched at runtime; for example, the discriminant
// on a set of enum variants or a literal.
fn get_options(bcx: &Block, m: &[Match], col: uint) -> Vec<Opt> {
    let ccx = bcx.ccx();
    fn add_to_set(tcx: &ty::ctxt, set: &mut Vec<Opt>, val: Opt) {
        if set.iter().any(|l| opt_eq(tcx, l, &val)) {return;}
        set.push(val);
    }
    // Vector comparisons are special in that since the actual
    // conditions over-match, we need to be careful about them. This
    // means that in order to properly handle things in order, we need
    // to not always merge conditions.
    fn add_veclen_to_set(set: &mut Vec<Opt> , i: uint,
                         len: uint, vlo: VecLenOpt) {
        match set.last() {
            // If the last condition in the list matches the one we want
            // to add, then extend its range. Otherwise, make a new
            // vec_len with a range just covering the new entry.
            Some(&vec_len(len2, vlo2, (start, end)))
                 if len == len2 && vlo == vlo2 => {
                let length = set.len();
                 *set.get_mut(length - 1) =
                     vec_len(len, vlo, (start, end+1))
            }
            _ => set.push(vec_len(len, vlo, (i, i)))
        }
    }

    let mut found = Vec::new();
    for (i, br) in m.iter().enumerate() {
        let cur = *br.pats.get(col);
        match cur.node {
            ast::PatLit(l) => {
                add_to_set(ccx.tcx(), &mut found, lit(ExprLit(l)));
            }
            ast::PatIdent(..) => {
                // This is one of: an enum variant, a unit-like struct, or a
                // variable binding.
                let opt_def = ccx.tcx.def_map.borrow().find_copy(&cur.id);
                match opt_def {
                    Some(def::DefVariant(..)) => {
                        add_to_set(ccx.tcx(), &mut found,
                                   variant_opt(bcx, cur.id));
                    }
                    Some(def::DefStruct(..)) => {
                        add_to_set(ccx.tcx(), &mut found,
                                   lit(UnitLikeStructLit(cur.id)));
                    }
                    Some(def::DefStatic(const_did, false)) => {
                        add_to_set(ccx.tcx(), &mut found,
                                   lit(ConstLit(const_did)));
                    }
                    _ => {}
                }
            }
            ast::PatEnum(..) | ast::PatStruct(..) => {
                // This could be one of: a tuple-like enum variant, a
                // struct-like enum variant, or a struct.
                let opt_def = ccx.tcx.def_map.borrow().find_copy(&cur.id);
                match opt_def {
                    Some(def::DefFn(..)) |
                    Some(def::DefVariant(..)) => {
                        add_to_set(ccx.tcx(), &mut found,
                                   variant_opt(bcx, cur.id));
                    }
                    Some(def::DefStatic(const_did, false)) => {
                        add_to_set(ccx.tcx(), &mut found,
                                   lit(ConstLit(const_did)));
                    }
                    _ => {}
                }
            }
            ast::PatRange(l1, l2) => {
                add_to_set(ccx.tcx(), &mut found, range(l1, l2));
            }
            ast::PatVec(ref before, slice, ref after) => {
                let (len, vec_opt) = match slice {
                    None => (before.len(), vec_len_eq),
                    Some(_) => (before.len() + after.len(),
                                vec_len_ge(before.len()))
                };
                add_veclen_to_set(&mut found, i, len, vec_opt);
            }
            _ => {}
        }
    }
    return found;
}

struct ExtractedBlock<'a> {
    vals: Vec<ValueRef> ,
    bcx: &'a Block<'a>,
}

fn extract_variant_args<'a>(
                        bcx: &'a Block<'a>,
                        repr: &adt::Repr,
                        disr_val: ty::Disr,
                        val: ValueRef)
                        -> ExtractedBlock<'a> {
    let _icx = push_ctxt("match::extract_variant_args");
    let args = Vec::from_fn(adt::num_args(repr, disr_val), |i| {
        adt::trans_field_ptr(bcx, repr, val, disr_val, i)
    });

    ExtractedBlock { vals: args, bcx: bcx }
}

fn match_datum(bcx: &Block,
               val: ValueRef,
               pat_id: ast::NodeId)
               -> Datum<Lvalue> {
    /*!
     * Helper for converting from the ValueRef that we pass around in
     * the match code, which is always an lvalue, into a Datum. Eventually
     * we should just pass around a Datum and be done with it.
     */

    let ty = node_id_type(bcx, pat_id);
    Datum::new(val, ty, Lvalue)
}


fn extract_vec_elems<'a>(
                     bcx: &'a Block<'a>,
                     pat_id: ast::NodeId,
                     elem_count: uint,
                     slice: Option<uint>,
                     val: ValueRef)
                     -> ExtractedBlock<'a> {
    let _icx = push_ctxt("match::extract_vec_elems");
    let vec_datum = match_datum(bcx, val, pat_id);
    let (base, len) = vec_datum.get_vec_base_and_len(bcx);
    let vec_ty = node_id_type(bcx, pat_id);
    let vt = tvec::vec_types(bcx, ty::sequence_element_type(bcx.tcx(), vec_ty));

    let mut elems = Vec::from_fn(elem_count, |i| {
        match slice {
            None => GEPi(bcx, base, [i]),
            Some(n) if i < n => GEPi(bcx, base, [i]),
            Some(n) if i > n => {
                InBoundsGEP(bcx, base, [
                    Sub(bcx, len,
                        C_int(bcx.ccx(), (elem_count - i) as int))])
            }
            _ => unsafe { llvm::LLVMGetUndef(vt.llunit_ty.to_ref()) }
        }
    });
    if slice.is_some() {
        let n = slice.unwrap();
        let slice_byte_offset = Mul(bcx, vt.llunit_size, C_uint(bcx.ccx(), n));
        let slice_begin = tvec::pointer_add_byte(bcx, base, slice_byte_offset);
        let slice_len_offset = C_uint(bcx.ccx(), elem_count - 1u);
        let slice_len = Sub(bcx, len, slice_len_offset);
        let slice_ty = ty::mk_slice(bcx.tcx(),
                                    ty::ReStatic,
                                    ty::mt {ty: vt.unit_ty, mutbl: ast::MutImmutable});
        let scratch = rvalue_scratch_datum(bcx, slice_ty, "");
        Store(bcx, slice_begin,
              GEPi(bcx, scratch.val, [0u, abi::slice_elt_base]));
        Store(bcx, slice_len, GEPi(bcx, scratch.val, [0u, abi::slice_elt_len]));
        *elems.get_mut(n) = scratch.val;
    }

    ExtractedBlock { vals: elems, bcx: bcx }
}

/// Checks every pattern in `m` at `col` column.
/// If there are a struct pattern among them function
/// returns list of all fields that are matched in these patterns.
/// Function returns None if there is no struct pattern.
/// Function doesn't collect fields from struct-like enum variants.
/// Function can return empty list if there is only wildcard struct pattern.
fn collect_record_or_struct_fields<'a>(
                                   bcx: &'a Block<'a>,
                                   m: &[Match],
                                   col: uint)
                                   -> Option<Vec<ast::Ident> > {
    let mut fields: Vec<ast::Ident> = Vec::new();
    let mut found = false;
    for br in m.iter() {
        match br.pats.get(col).node {
          ast::PatStruct(_, ref fs, _) => {
            match ty::get(node_id_type(bcx, br.pats.get(col).id)).sty {
              ty::ty_struct(..) => {
                   extend(&mut fields, fs.as_slice());
                   found = true;
              }
              _ => ()
            }
          }
          _ => ()
        }
    }
    if found {
        return Some(fields);
    } else {
        return None;
    }

    fn extend(idents: &mut Vec<ast::Ident> , field_pats: &[ast::FieldPat]) {
        for field_pat in field_pats.iter() {
            let field_ident = field_pat.ident;
            if !idents.iter().any(|x| x.name == field_ident.name) {
                idents.push(field_ident);
            }
        }
    }
}

// Macro for deciding whether any of the remaining matches fit a given kind of
// pattern.  Note that, because the macro is well-typed, either ALL of the
// matches should fit that sort of pattern or NONE (however, some of the
// matches may be wildcards like _ or identifiers).
macro_rules! any_pat (
    ($m:expr, $pattern:pat) => (
        ($m).iter().any(|br| {
            match br.pats.get(col).node {
                $pattern => true,
                _ => false
            }
        })
    )
)

fn any_uniq_pat(m: &[Match], col: uint) -> bool {
    any_pat!(m, ast::PatBox(_))
}

fn any_region_pat(m: &[Match], col: uint) -> bool {
    any_pat!(m, ast::PatRegion(_))
}

fn any_tup_pat(m: &[Match], col: uint) -> bool {
    any_pat!(m, ast::PatTup(_))
}

fn any_tuple_struct_pat(bcx: &Block, m: &[Match], col: uint) -> bool {
    m.iter().any(|br| {
        let pat = *br.pats.get(col);
        match pat.node {
            ast::PatEnum(_, _) => {
                match bcx.tcx().def_map.borrow().find(&pat.id) {
                    Some(&def::DefFn(..)) |
                    Some(&def::DefStruct(..)) => true,
                    _ => false
                }
            }
            _ => false
        }
    })
}

struct DynamicFailureHandler<'a> {
    bcx: &'a Block<'a>,
    sp: Span,
    msg: InternedString,
    finished: Cell<Option<BasicBlockRef>>,
}

impl<'a> DynamicFailureHandler<'a> {
    fn handle_fail(&self) -> BasicBlockRef {
        match self.finished.get() {
            Some(bb) => return bb,
            _ => (),
        }

        let fcx = self.bcx.fcx;
        let fail_cx = fcx.new_block(false, "case_fallthrough", None);
        controlflow::trans_fail(fail_cx, self.sp, self.msg.clone());
        self.finished.set(Some(fail_cx.llbb));
        fail_cx.llbb
    }
}

/// What to do when the pattern match fails.
enum FailureHandler<'a> {
    Infallible,
    JumpToBasicBlock(BasicBlockRef),
    DynamicFailureHandlerClass(Box<DynamicFailureHandler<'a>>),
}

impl<'a> FailureHandler<'a> {
    fn is_infallible(&self) -> bool {
        match *self {
            Infallible => true,
            _ => false,
        }
    }

    fn is_fallible(&self) -> bool {
        !self.is_infallible()
    }

    fn handle_fail(&self) -> BasicBlockRef {
        match *self {
            Infallible => {
                fail!("attempted to fail in infallible failure handler!")
            }
            JumpToBasicBlock(basic_block) => basic_block,
            DynamicFailureHandlerClass(ref dynamic_failure_handler) => {
                dynamic_failure_handler.handle_fail()
            }
        }
    }
}

fn pick_col(m: &[Match]) -> uint {
    fn score(p: &ast::Pat) -> uint {
        match p.node {
          ast::PatLit(_) | ast::PatEnum(_, _) | ast::PatRange(_, _) => 1u,
          ast::PatIdent(_, _, Some(ref p)) => score(&**p),
          _ => 0u
        }
    }
    let mut scores = Vec::from_elem(m[0].pats.len(), 0u);
    for br in m.iter() {
        for (i, ref p) in br.pats.iter().enumerate() {
            *scores.get_mut(i) += score(&***p);
        }
    }
    let mut max_score = 0u;
    let mut best_col = 0u;
    for (i, score) in scores.iter().enumerate() {
        let score = *score;

        // Irrefutable columns always go first, they'd only be duplicated in
        // the branches.
        if score == 0u { return i; }
        // If no irrefutable ones are found, we pick the one with the biggest
        // branching factor.
        if score > max_score { max_score = score; best_col = i; }
    }
    return best_col;
}

#[deriving(PartialEq)]
pub enum branch_kind { no_branch, single, switch, compare, compare_vec_len, }

// Compiles a comparison between two things.
fn compare_values<'a>(
                  cx: &'a Block<'a>,
                  lhs: ValueRef,
                  rhs: ValueRef,
                  rhs_t: ty::t)
                  -> Result<'a> {
    fn compare_str<'a>(cx: &'a Block<'a>,
                       lhs: ValueRef,
                       rhs: ValueRef,
                       rhs_t: ty::t)
                       -> Result<'a> {
        let did = langcall(cx,
                           None,
                           format!("comparison of `{}`",
                                   cx.ty_to_str(rhs_t)).as_slice(),
                           StrEqFnLangItem);
        callee::trans_lang_call(cx, did, [lhs, rhs], None)
    }

    let _icx = push_ctxt("compare_values");
    if ty::type_is_scalar(rhs_t) {
        let rs = compare_scalar_types(cx, lhs, rhs, rhs_t, ast::BiEq);
        return Result::new(rs.bcx, rs.val);
    }

    match ty::get(rhs_t).sty {
        ty::ty_uniq(t) => match ty::get(t).sty {
            ty::ty_str => {
                let scratch_lhs = alloca(cx, val_ty(lhs), "__lhs");
                Store(cx, lhs, scratch_lhs);
                let scratch_rhs = alloca(cx, val_ty(rhs), "__rhs");
                Store(cx, rhs, scratch_rhs);
                let did = langcall(cx,
                                   None,
                                   format!("comparison of `{}`",
                                           cx.ty_to_str(rhs_t)).as_slice(),
                                   UniqStrEqFnLangItem);
                callee::trans_lang_call(cx, did, [scratch_lhs, scratch_rhs], None)
            }
            _ => cx.sess().bug("only strings supported in compare_values"),
        },
        ty::ty_rptr(_, mt) => match ty::get(mt.ty).sty {
            ty::ty_str => compare_str(cx, lhs, rhs, rhs_t),
            ty::ty_vec(mt, _) => match ty::get(mt.ty).sty {
                ty::ty_uint(ast::TyU8) => {
                    // NOTE: cast &[u8] to &str and abuse the str_eq lang item,
                    // which calls memcmp().
                    let t = ty::mk_str_slice(cx.tcx(), ty::ReStatic, ast::MutImmutable);
                    let lhs = BitCast(cx, lhs, type_of::type_of(cx.ccx(), t).ptr_to());
                    let rhs = BitCast(cx, rhs, type_of::type_of(cx.ccx(), t).ptr_to());
                    compare_str(cx, lhs, rhs, rhs_t)
                },
                _ => cx.sess().bug("only byte strings supported in compare_values"),
            },
            _ => cx.sess().bug("on string and byte strings supported in compare_values"),
        },
        _ => cx.sess().bug("only scalars, byte strings, and strings supported in compare_values"),
    }
}

fn insert_lllocals<'a>(mut bcx: &'a Block<'a>,
                       bindings_map: &BindingsMap)
                       -> &'a Block<'a> {
    /*!
     * For each binding in `data.bindings_map`, adds an appropriate entry into
     * the `fcx.lllocals` map
     */

    for (&ident, &binding_info) in bindings_map.iter() {
        let llval = match binding_info.trmode {
            // By value mut binding for a copy type: load from the ptr
            // into the matched value and copy to our alloca
            TrByCopy(llbinding) => {
                let llval = Load(bcx, binding_info.llmatch);
                let datum = Datum::new(llval, binding_info.ty, Lvalue);
                bcx = datum.store_to(bcx, llbinding);

                llbinding
            },

            // By value move bindings: load from the ptr into the matched value
            TrByMove => Load(bcx, binding_info.llmatch),

            // By ref binding: use the ptr into the matched value
            TrByRef => binding_info.llmatch
        };

        let datum = Datum::new(llval, binding_info.ty, Lvalue);

        debug!("binding {:?} to {}",
               binding_info.id,
               bcx.val_to_str(llval));
        bcx.fcx.lllocals.borrow_mut().insert(binding_info.id, datum);

        if bcx.sess().opts.debuginfo == FullDebugInfo {
            debuginfo::create_match_binding_metadata(bcx,
                                                     ident,
                                                     binding_info);
        }
    }
    bcx
}

fn compile_guard<'a, 'b>(
                 bcx: &'b Block<'b>,
                 guard_expr: &ast::Expr,
                 data: &ArmData,
                 m: &'a [Match<'a, 'b>],
                 vals: &[ValueRef],
                 chk: &FailureHandler,
                 has_genuine_default: bool)
                 -> &'b Block<'b> {
    debug!("compile_guard(bcx={}, guard_expr={}, m={}, vals={})",
           bcx.to_str(),
           bcx.expr_to_str(guard_expr),
           m.repr(bcx.tcx()),
           vec_map_to_str(vals, |v| bcx.val_to_str(*v)));
    let _indenter = indenter();

    let mut bcx = insert_lllocals(bcx, &data.bindings_map);

    let val = unpack_datum!(bcx, expr::trans(bcx, guard_expr));
    let val = val.to_llbool(bcx);

    return with_cond(bcx, Not(bcx, val), |bcx| {
        // Guard does not match: remove all bindings from the lllocals table
        for (_, &binding_info) in data.bindings_map.iter() {
            bcx.fcx.lllocals.borrow_mut().remove(&binding_info.id);
        }
        match chk {
            // If the default arm is the only one left, move on to the next
            // condition explicitly rather than (possibly) falling back to
            // the default arm.
            &JumpToBasicBlock(_) if m.len() == 1 && has_genuine_default => {
                Br(bcx, chk.handle_fail());
            }
            _ => {
                compile_submatch(bcx, m, vals, chk, has_genuine_default);
            }
        };
        bcx
    });
}

fn compile_submatch<'a, 'b>(
                    bcx: &'b Block<'b>,
                    m: &'a [Match<'a, 'b>],
                    vals: &[ValueRef],
                    chk: &FailureHandler,
                    has_genuine_default: bool) {
    debug!("compile_submatch(bcx={}, m={}, vals={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           vec_map_to_str(vals, |v| bcx.val_to_str(*v)));
    let _indenter = indenter();
    let _icx = push_ctxt("match::compile_submatch");
    let mut bcx = bcx;
    if m.len() == 0u {
        if chk.is_fallible() {
            Br(bcx, chk.handle_fail());
        }
        return;
    }
    if m[0].pats.len() == 0u {
        let data = &m[0].data;
        for &(ref ident, ref value_ptr) in m[0].bound_ptrs.iter() {
            let llmatch = data.bindings_map.get(ident).llmatch;
            Store(bcx, *value_ptr, llmatch);
        }
        match data.arm.guard {
            Some(ref guard_expr) => {
                bcx = compile_guard(bcx,
                                    &**guard_expr,
                                    m[0].data,
                                    m.slice(1, m.len()),
                                    vals,
                                    chk,
                                    has_genuine_default);
            }
            _ => ()
        }
        Br(bcx, data.bodycx.llbb);
        return;
    }

    let col = pick_col(m);
    let val = vals[col];

    if has_nested_bindings(m, col) {
        let expanded = expand_nested_bindings(bcx, m, col, val);
        compile_submatch_continue(bcx,
                                  expanded.as_slice(),
                                  vals,
                                  chk,
                                  col,
                                  val,
                                  has_genuine_default)
    } else {
        compile_submatch_continue(bcx, m, vals, chk, col, val, has_genuine_default)
    }
}

fn compile_submatch_continue<'a, 'b>(
                             mut bcx: &'b Block<'b>,
                             m: &'a [Match<'a, 'b>],
                             vals: &[ValueRef],
                             chk: &FailureHandler,
                             col: uint,
                             val: ValueRef,
                             has_genuine_default: bool) {
    let fcx = bcx.fcx;
    let tcx = bcx.tcx();
    let dm = &tcx.def_map;

    let vals_left = Vec::from_slice(vals.slice(0u, col)).append(vals.slice(col + 1u, vals.len()));
    let ccx = bcx.fcx.ccx;
    let mut pat_id = 0;
    for br in m.iter() {
        // Find a real id (we're adding placeholder wildcard patterns, but
        // each column is guaranteed to have at least one real pattern)
        if pat_id == 0 {
            pat_id = br.pats.get(col).id;
        }
    }

    match collect_record_or_struct_fields(bcx, m, col) {
        Some(ref rec_fields) => {
            let pat_ty = node_id_type(bcx, pat_id);
            let pat_repr = adt::represent_type(bcx.ccx(), pat_ty);
            expr::with_field_tys(tcx, pat_ty, Some(pat_id), |discr, field_tys| {
                let rec_vals = rec_fields.iter().map(|field_name| {
                        let ix = ty::field_idx_strict(tcx, field_name.name, field_tys);
                        adt::trans_field_ptr(bcx, &*pat_repr, val, discr, ix)
                        }).collect::<Vec<_>>();
                compile_submatch(
                        bcx,
                        enter_rec_or_struct(bcx,
                                            dm,
                                            m,
                                            col,
                                            rec_fields.as_slice(),
                                            val).as_slice(),
                        rec_vals.append(vals_left.as_slice()).as_slice(),
                        chk, has_genuine_default);
            });
            return;
        }
        None => {}
    }

    if any_tup_pat(m, col) {
        let tup_ty = node_id_type(bcx, pat_id);
        let tup_repr = adt::represent_type(bcx.ccx(), tup_ty);
        let n_tup_elts = match ty::get(tup_ty).sty {
          ty::ty_tup(ref elts) => elts.len(),
          _ => ccx.sess().bug("non-tuple type in tuple pattern")
        };
        let tup_vals = Vec::from_fn(n_tup_elts, |i| {
            adt::trans_field_ptr(bcx, &*tup_repr, val, 0, i)
        });
        compile_submatch(bcx,
                         enter_tup(bcx,
                                   dm,
                                   m,
                                   col,
                                   val,
                                   n_tup_elts).as_slice(),
                         tup_vals.append(vals_left.as_slice()).as_slice(),
                         chk, has_genuine_default);
        return;
    }

    if any_tuple_struct_pat(bcx, m, col) {
        let struct_ty = node_id_type(bcx, pat_id);
        let struct_element_count;
        match ty::get(struct_ty).sty {
            ty::ty_struct(struct_id, _) => {
                struct_element_count =
                    ty::lookup_struct_fields(tcx, struct_id).len();
            }
            _ => {
                ccx.sess().bug("non-struct type in tuple struct pattern");
            }
        }

        let struct_repr = adt::represent_type(bcx.ccx(), struct_ty);
        let llstructvals = Vec::from_fn(struct_element_count, |i| {
            adt::trans_field_ptr(bcx, &*struct_repr, val, 0, i)
        });
        compile_submatch(bcx,
                         enter_tuple_struct(bcx, dm, m, col, val,
                                            struct_element_count).as_slice(),
                         llstructvals.append(vals_left.as_slice()).as_slice(),
                         chk, has_genuine_default);
        return;
    }

    if any_uniq_pat(m, col) {
        let llbox = Load(bcx, val);
        compile_submatch(bcx,
                         enter_uniq(bcx, dm, m, col, val).as_slice(),
                         (vec!(llbox)).append(vals_left.as_slice()).as_slice(),
                         chk, has_genuine_default);
        return;
    }

    if any_region_pat(m, col) {
        let loaded_val = Load(bcx, val);
        compile_submatch(bcx,
                         enter_region(bcx, dm, m, col, val).as_slice(),
                         (vec!(loaded_val)).append(vals_left.as_slice()).as_slice(),
                         chk, has_genuine_default);
        return;
    }

    // Decide what kind of branch we need
    let opts = get_options(bcx, m, col);
    debug!("options={:?}", opts);
    let mut kind = no_branch;
    let mut test_val = val;
    debug!("test_val={}", bcx.val_to_str(test_val));
    if opts.len() > 0u {
        match *opts.get(0) {
            var(_, ref repr) => {
                let (the_kind, val_opt) = adt::trans_switch(bcx, &**repr, val);
                kind = the_kind;
                for &tval in val_opt.iter() { test_val = tval; }
            }
            lit(_) => {
                let pty = node_id_type(bcx, pat_id);
                test_val = load_if_immediate(bcx, val, pty);
                kind = if ty::type_is_integral(pty) { switch }
                else { compare };
            }
            range(_, _) => {
                test_val = Load(bcx, val);
                kind = compare;
            },
            vec_len(..) => {
                let vec_ty = node_id_type(bcx, pat_id);
                let (_, len) = tvec::get_base_and_len(bcx, val, vec_ty);
                test_val = len;
                kind = compare_vec_len;
            }
        }
    }
    for o in opts.iter() {
        match *o {
            range(_, _) => { kind = compare; break }
            _ => ()
        }
    }
    let else_cx = match kind {
        no_branch | single => bcx,
        _ => bcx.fcx.new_temp_block("match_else")
    };
    let sw = if kind == switch {
        Switch(bcx, test_val, else_cx.llbb, opts.len())
    } else {
        C_int(ccx, 0) // Placeholder for when not using a switch
    };

    let defaults = enter_default(else_cx, dm, m, col, val);
    let exhaustive = chk.is_infallible() && defaults.len() == 0u;
    let len = opts.len();

    // Compile subtrees for each option
    for (i, opt) in opts.iter().enumerate() {
        // In some cases of range and vector pattern matching, we need to
        // override the failure case so that instead of failing, it proceeds
        // to try more matching. branch_chk, then, is the proper failure case
        // for the current conditional branch.
        let mut branch_chk = None;
        let mut opt_cx = else_cx;
        if !exhaustive || i+1 < len {
            opt_cx = bcx.fcx.new_temp_block("match_case");
            match kind {
              single => Br(bcx, opt_cx.llbb),
              switch => {
                  match trans_opt(bcx, opt) {
                      single_result(r) => {
                        unsafe {
                          llvm::LLVMAddCase(sw, r.val, opt_cx.llbb);
                          bcx = r.bcx;
                        }
                      }
                      _ => {
                          bcx.sess().bug(
                              "in compile_submatch, expected \
                               trans_opt to return a single_result")
                      }
                  }
              }
              compare => {
                  let t = node_id_type(bcx, pat_id);
                  let Result {bcx: after_cx, val: matches} = {
                      match trans_opt(bcx, opt) {
                          single_result(Result {bcx, val}) => {
                              compare_values(bcx, test_val, val, t)
                          }
                          lower_bound(Result {bcx, val}) => {
                              compare_scalar_types(
                                  bcx, test_val, val,
                                  t, ast::BiGe)
                          }
                          range_result(Result {val: vbegin, ..},
                                       Result {bcx, val: vend}) => {
                              let Result {bcx, val: llge} =
                                  compare_scalar_types(
                                  bcx, test_val,
                                  vbegin, t, ast::BiGe);
                              let Result {bcx, val: llle} =
                                  compare_scalar_types(
                                  bcx, test_val, vend,
                                  t, ast::BiLe);
                              Result::new(bcx, And(bcx, llge, llle))
                          }
                      }
                  };
                  bcx = fcx.new_temp_block("compare_next");

                  // If none of the sub-cases match, and the current condition
                  // is guarded or has multiple patterns, move on to the next
                  // condition, if there is any, rather than falling back to
                  // the default.
                  let guarded = m[i].data.arm.guard.is_some();
                  let multi_pats = m[i].pats.len() > 1;
                  if i+1 < len && (guarded || multi_pats) {
                      branch_chk = Some(JumpToBasicBlock(bcx.llbb));
                  }
                  CondBr(after_cx, matches, opt_cx.llbb, bcx.llbb);
              }
              compare_vec_len => {
                  let Result {bcx: after_cx, val: matches} = {
                      match trans_opt(bcx, opt) {
                          single_result(
                              Result {bcx, val}) => {
                              let value = compare_scalar_values(
                                  bcx, test_val, val,
                                  signed_int, ast::BiEq);
                              Result::new(bcx, value)
                          }
                          lower_bound(
                              Result {bcx, val: val}) => {
                              let value = compare_scalar_values(
                                  bcx, test_val, val,
                                  signed_int, ast::BiGe);
                              Result::new(bcx, value)
                          }
                          range_result(
                              Result {val: vbegin, ..},
                              Result {bcx, val: vend}) => {
                              let llge =
                                  compare_scalar_values(
                                  bcx, test_val,
                                  vbegin, signed_int, ast::BiGe);
                              let llle =
                                  compare_scalar_values(
                                  bcx, test_val, vend,
                                  signed_int, ast::BiLe);
                              Result::new(bcx, And(bcx, llge, llle))
                          }
                      }
                  };
                  bcx = fcx.new_temp_block("compare_vec_len_next");

                  // If none of these subcases match, move on to the
                  // next condition if there is any.
                  if i+1 < len {
                      branch_chk = Some(JumpToBasicBlock(bcx.llbb));
                  }
                  CondBr(after_cx, matches, opt_cx.llbb, bcx.llbb);
              }
              _ => ()
            }
        } else if kind == compare || kind == compare_vec_len {
            Br(bcx, else_cx.llbb);
        }

        let mut size = 0u;
        let mut unpacked = Vec::new();
        match *opt {
            var(disr_val, ref repr) => {
                let ExtractedBlock {vals: argvals, bcx: new_bcx} =
                    extract_variant_args(opt_cx, &**repr, disr_val, val);
                size = argvals.len();
                unpacked = argvals;
                opt_cx = new_bcx;
            }
            vec_len(n, vt, _) => {
                let (n, slice) = match vt {
                    vec_len_ge(i) => (n + 1u, Some(i)),
                    vec_len_eq => (n, None)
                };
                let args = extract_vec_elems(opt_cx, pat_id, n,
                                             slice, val);
                size = args.vals.len();
                unpacked = args.vals.clone();
                opt_cx = args.bcx;
            }
            lit(_) | range(_, _) => ()
        }
        let opt_ms = enter_opt(opt_cx, m, opt, col, size, val);
        let opt_vals = unpacked.append(vals_left.as_slice());

        match branch_chk {
            None => {
                compile_submatch(opt_cx,
                                 opt_ms.as_slice(),
                                 opt_vals.as_slice(),
                                 chk,
                                 has_genuine_default)
            }
            Some(branch_chk) => {
                compile_submatch(opt_cx,
                                 opt_ms.as_slice(),
                                 opt_vals.as_slice(),
                                 &branch_chk,
                                 has_genuine_default)
            }
        }
    }

    // Compile the fall-through case, if any
    if !exhaustive && kind != single {
        if kind == compare || kind == compare_vec_len {
            Br(bcx, else_cx.llbb);
        }
        match chk {
            // If there is only one default arm left, move on to the next
            // condition explicitly rather than (eventually) falling back to
            // the last default arm.
            &JumpToBasicBlock(_) if defaults.len() == 1 && has_genuine_default => {
                Br(else_cx, chk.handle_fail());
            }
            _ => {
                compile_submatch(else_cx,
                                 defaults.as_slice(),
                                 vals_left.as_slice(),
                                 chk,
                                 has_genuine_default);
            }
        }
    }
}

pub fn trans_match<'a>(
                   bcx: &'a Block<'a>,
                   match_expr: &ast::Expr,
                   discr_expr: &ast::Expr,
                   arms: &[ast::Arm],
                   dest: Dest)
                   -> &'a Block<'a> {
    let _icx = push_ctxt("match::trans_match");
    trans_match_inner(bcx, match_expr.id, discr_expr, arms, dest)
}

fn create_bindings_map(bcx: &Block, pat: Gc<ast::Pat>) -> BindingsMap {
    // Create the bindings map, which is a mapping from each binding name
    // to an alloca() that will be the value for that local variable.
    // Note that we use the names because each binding will have many ids
    // from the various alternatives.
    let ccx = bcx.ccx();
    let tcx = bcx.tcx();
    let mut bindings_map = HashMap::new();
    pat_bindings(&tcx.def_map, &*pat, |bm, p_id, span, path| {
        let ident = path_to_ident(path);
        let variable_ty = node_id_type(bcx, p_id);
        let llvariable_ty = type_of::type_of(ccx, variable_ty);
        let tcx = bcx.tcx();

        let llmatch;
        let trmode;
        match bm {
            ast::BindByValue(_)
                if !ty::type_moves_by_default(tcx, variable_ty) => {
                llmatch = alloca(bcx,
                                 llvariable_ty.ptr_to(),
                                 "__llmatch");
                trmode = TrByCopy(alloca(bcx,
                                         llvariable_ty,
                                         bcx.ident(ident).as_slice()));
            }
            ast::BindByValue(_) => {
                // in this case, the final type of the variable will be T,
                // but during matching we need to store a *T as explained
                // above
                llmatch = alloca(bcx,
                                 llvariable_ty.ptr_to(),
                                 bcx.ident(ident).as_slice());
                trmode = TrByMove;
            }
            ast::BindByRef(_) => {
                llmatch = alloca(bcx,
                                 llvariable_ty,
                                 bcx.ident(ident).as_slice());
                trmode = TrByRef;
            }
        };
        bindings_map.insert(ident, BindingInfo {
            llmatch: llmatch,
            trmode: trmode,
            id: p_id,
            span: span,
            ty: variable_ty
        });
    });
    return bindings_map;
}

fn trans_match_inner<'a>(scope_cx: &'a Block<'a>,
                         match_id: ast::NodeId,
                         discr_expr: &ast::Expr,
                         arms: &[ast::Arm],
                         dest: Dest) -> &'a Block<'a> {
    let _icx = push_ctxt("match::trans_match_inner");
    let fcx = scope_cx.fcx;
    let mut bcx = scope_cx;
    let tcx = bcx.tcx();

    let discr_datum = unpack_datum!(bcx, expr::trans_to_lvalue(bcx, discr_expr,
                                                               "match"));
    if bcx.unreachable.get() {
        return bcx;
    }

    let t = node_id_type(bcx, discr_expr.id);
    let chk = {
        if ty::type_is_empty(tcx, t) {
            // Special case for empty types
            let fail_cx = Cell::new(None);
            let fail_handler = box DynamicFailureHandler {
                bcx: scope_cx,
                sp: discr_expr.span,
                msg: InternedString::new("scrutinizing value that can't \
                                          exist"),
                finished: fail_cx,
            };
            DynamicFailureHandlerClass(fail_handler)
        } else {
            Infallible
        }
    };

    let arm_datas: Vec<ArmData> = arms.iter().map(|arm| ArmData {
        bodycx: fcx.new_id_block("case_body", arm.body.id),
        arm: arm,
        bindings_map: create_bindings_map(bcx, *arm.pats.get(0))
    }).collect();

    let mut matches = Vec::new();
    for arm_data in arm_datas.iter() {
        matches.extend(arm_data.arm.pats.iter().map(|p| Match {
            pats: vec!(*p),
            data: arm_data,
            bound_ptrs: Vec::new(),
        }));
    }

    // `compile_submatch` works one column of arm patterns a time and
    // then peels that column off. So as we progress, it may become
    // impossible to tell whether we have a genuine default arm, i.e.
    // `_ => foo` or not. Sometimes it is important to know that in order
    // to decide whether moving on to the next condition or falling back
    // to the default arm.
    let has_default = arms.last().map_or(false, |arm| {
        arm.pats.len() == 1
        && arm.pats.last().unwrap().node == ast::PatWild
    });

    compile_submatch(bcx, matches.as_slice(), [discr_datum.val], &chk, has_default);

    let mut arm_cxs = Vec::new();
    for arm_data in arm_datas.iter() {
        let mut bcx = arm_data.bodycx;

        // insert bindings into the lllocals map
        bcx = insert_lllocals(bcx, &arm_data.bindings_map);
        bcx = expr::trans_into(bcx, &*arm_data.arm.body, dest);
        arm_cxs.push(bcx);
    }

    bcx = scope_cx.fcx.join_blocks(match_id, arm_cxs.as_slice());
    return bcx;
}

enum IrrefutablePatternBindingMode {
    // Stores the association between node ID and LLVM value in `lllocals`.
    BindLocal,
    // Stores the association between node ID and LLVM value in `llargs`.
    BindArgument
}

pub fn store_local<'a>(bcx: &'a Block<'a>,
                       local: &ast::Local)
                       -> &'a Block<'a> {
    /*!
     * Generates code for a local variable declaration like
     * `let <pat>;` or `let <pat> = <opt_init_expr>`.
     */
    let _icx = push_ctxt("match::store_local");
    let mut bcx = bcx;
    let tcx = bcx.tcx();
    let pat = local.pat;
    let opt_init_expr = local.init;

    return match opt_init_expr {
        Some(init_expr) => {
            // Optimize the "let x = expr" case. This just writes
            // the result of evaluating `expr` directly into the alloca
            // for `x`. Often the general path results in similar or the
            // same code post-optimization, but not always. In particular,
            // in unsafe code, you can have expressions like
            //
            //    let x = intrinsics::uninit();
            //
            // In such cases, the more general path is unsafe, because
            // it assumes it is matching against a valid value.
            match simple_identifier(&*pat) {
                Some(path) => {
                    let var_scope = cleanup::var_scope(tcx, local.id);
                    return mk_binding_alloca(
                        bcx, pat.id, path, BindLocal, var_scope, (),
                        |(), bcx, v, _| expr::trans_into(bcx, &*init_expr,
                                                         expr::SaveIn(v)));
                }

                None => {}
            }

            // General path.
            let init_datum =
                unpack_datum!(bcx, expr::trans_to_lvalue(bcx, &*init_expr, "let"));
            if ty::type_is_bot(expr_ty(bcx, &*init_expr)) {
                create_dummy_locals(bcx, pat)
            } else {
                if bcx.sess().asm_comments() {
                    add_comment(bcx, "creating zeroable ref llval");
                }
                let var_scope = cleanup::var_scope(tcx, local.id);
                bind_irrefutable_pat(bcx, pat, init_datum.val, BindLocal, var_scope)
            }
        }
        None => {
            create_dummy_locals(bcx, pat)
        }
    };

    fn create_dummy_locals<'a>(mut bcx: &'a Block<'a>,
                               pat: Gc<ast::Pat>)
                               -> &'a Block<'a> {
        // create dummy memory for the variables if we have no
        // value to store into them immediately
        let tcx = bcx.tcx();
        pat_bindings(&tcx.def_map, &*pat, |_, p_id, _, path| {
                let scope = cleanup::var_scope(tcx, p_id);
                bcx = mk_binding_alloca(
                    bcx, p_id, path, BindLocal, scope, (),
                    |(), bcx, llval, ty| { zero_mem(bcx, llval, ty); bcx });
            });
        bcx
    }
}

pub fn store_arg<'a>(mut bcx: &'a Block<'a>,
                     pat: Gc<ast::Pat>,
                     arg: Datum<Rvalue>,
                     arg_scope: cleanup::ScopeId)
                     -> &'a Block<'a> {
    /*!
     * Generates code for argument patterns like `fn foo(<pat>: T)`.
     * Creates entries in the `llargs` map for each of the bindings
     * in `pat`.
     *
     * # Arguments
     *
     * - `pat` is the argument pattern
     * - `llval` is a pointer to the argument value (in other words,
     *   if the argument type is `T`, then `llval` is a `T*`). In some
     *   cases, this code may zero out the memory `llval` points at.
     */

    let _icx = push_ctxt("match::store_arg");

    match simple_identifier(&*pat) {
        Some(path) => {
            // Generate nicer LLVM for the common case of fn a pattern
            // like `x: T`
            let arg_ty = node_id_type(bcx, pat.id);
            if type_of::arg_is_indirect(bcx.ccx(), arg_ty)
                && bcx.sess().opts.debuginfo != FullDebugInfo {
                // Don't copy an indirect argument to an alloca, the caller
                // already put it in a temporary alloca and gave it up, unless
                // we emit extra-debug-info, which requires local allocas :(.
                let arg_val = arg.add_clean(bcx.fcx, arg_scope);
                bcx.fcx.llargs.borrow_mut()
                   .insert(pat.id, Datum::new(arg_val, arg_ty, Lvalue));
                bcx
            } else {
                mk_binding_alloca(
                    bcx, pat.id, path, BindArgument, arg_scope, arg,
                    |arg, bcx, llval, _| arg.store_to(bcx, llval))
            }
        }

        None => {
            // General path. Copy out the values that are used in the
            // pattern.
            let arg = unpack_datum!(
                bcx, arg.to_lvalue_datum_in_scope(bcx, "__arg", arg_scope));
            bind_irrefutable_pat(bcx, pat, arg.val,
                                 BindArgument, arg_scope)
        }
    }
}

fn mk_binding_alloca<'a,A>(bcx: &'a Block<'a>,
                           p_id: ast::NodeId,
                           path: &ast::Path,
                           binding_mode: IrrefutablePatternBindingMode,
                           cleanup_scope: cleanup::ScopeId,
                           arg: A,
                           populate: |A, &'a Block<'a>, ValueRef, ty::t| -> &'a Block<'a>)
                         -> &'a Block<'a> {
    let var_ty = node_id_type(bcx, p_id);
    let ident = ast_util::path_to_ident(path);

    // Allocate memory on stack for the binding.
    let llval = alloc_ty(bcx, var_ty, bcx.ident(ident).as_slice());

    // Subtle: be sure that we *populate* the memory *before*
    // we schedule the cleanup.
    let bcx = populate(arg, bcx, llval, var_ty);
    bcx.fcx.schedule_drop_mem(cleanup_scope, llval, var_ty);

    // Now that memory is initialized and has cleanup scheduled,
    // create the datum and insert into the local variable map.
    let datum = Datum::new(llval, var_ty, Lvalue);
    let mut llmap = match binding_mode {
        BindLocal => bcx.fcx.lllocals.borrow_mut(),
        BindArgument => bcx.fcx.llargs.borrow_mut()
    };
    llmap.insert(p_id, datum);
    bcx
}

fn bind_irrefutable_pat<'a>(
                        bcx: &'a Block<'a>,
                        pat: Gc<ast::Pat>,
                        val: ValueRef,
                        binding_mode: IrrefutablePatternBindingMode,
                        cleanup_scope: cleanup::ScopeId)
                        -> &'a Block<'a> {
    /*!
     * A simple version of the pattern matching code that only handles
     * irrefutable patterns. This is used in let/argument patterns,
     * not in match statements. Unifying this code with the code above
     * sounds nice, but in practice it produces very inefficient code,
     * since the match code is so much more general. In most cases,
     * LLVM is able to optimize the code, but it causes longer compile
     * times and makes the generated code nigh impossible to read.
     *
     * # Arguments
     * - bcx: starting basic block context
     * - pat: the irrefutable pattern being matched.
     * - val: the value being matched -- must be an lvalue (by ref, with cleanup)
     * - binding_mode: is this for an argument or a local variable?
     */

    debug!("bind_irrefutable_pat(bcx={}, pat={}, binding_mode={:?})",
           bcx.to_str(),
           pat.repr(bcx.tcx()),
           binding_mode);

    if bcx.sess().asm_comments() {
        add_comment(bcx, format!("bind_irrefutable_pat(pat={})",
                                 pat.repr(bcx.tcx())).as_slice());
    }

    let _indenter = indenter();

    let _icx = push_ctxt("match::bind_irrefutable_pat");
    let mut bcx = bcx;
    let tcx = bcx.tcx();
    let ccx = bcx.ccx();
    match pat.node {
        ast::PatIdent(pat_binding_mode, ref path, inner) => {
            if pat_is_binding(&tcx.def_map, &*pat) {
                // Allocate the stack slot where the value of this
                // binding will live and place it into the appropriate
                // map.
                bcx = mk_binding_alloca(
                    bcx, pat.id, path, binding_mode, cleanup_scope, (),
                    |(), bcx, llval, ty| {
                        match pat_binding_mode {
                            ast::BindByValue(_) => {
                                // By value binding: move the value that `val`
                                // points at into the binding's stack slot.
                                let d = Datum::new(val, ty, Lvalue);
                                d.store_to(bcx, llval)
                            }

                            ast::BindByRef(_) => {
                                // By ref binding: the value of the variable
                                // is the pointer `val` itself.
                                Store(bcx, val, llval);
                                bcx
                            }
                        }
                    });
            }

            for &inner_pat in inner.iter() {
                bcx = bind_irrefutable_pat(bcx, inner_pat, val,
                                           binding_mode, cleanup_scope);
            }
        }
        ast::PatEnum(_, ref sub_pats) => {
            let opt_def = bcx.tcx().def_map.borrow().find_copy(&pat.id);
            match opt_def {
                Some(def::DefVariant(enum_id, var_id, _)) => {
                    let repr = adt::represent_node(bcx, pat.id);
                    let vinfo = ty::enum_variant_with_id(ccx.tcx(),
                                                         enum_id,
                                                         var_id);
                    let args = extract_variant_args(bcx,
                                                    &*repr,
                                                    vinfo.disr_val,
                                                    val);
                    for sub_pat in sub_pats.iter() {
                        for (i, argval) in args.vals.iter().enumerate() {
                            bcx = bind_irrefutable_pat(bcx, *sub_pat.get(i),
                                                       *argval, binding_mode,
                                                       cleanup_scope);
                        }
                    }
                }
                Some(def::DefFn(..)) |
                Some(def::DefStruct(..)) => {
                    match *sub_pats {
                        None => {
                            // This is a unit-like struct. Nothing to do here.
                        }
                        Some(ref elems) => {
                            // This is the tuple struct case.
                            let repr = adt::represent_node(bcx, pat.id);
                            for (i, elem) in elems.iter().enumerate() {
                                let fldptr = adt::trans_field_ptr(bcx, &*repr,
                                                                  val, 0, i);
                                bcx = bind_irrefutable_pat(bcx, *elem,
                                                           fldptr, binding_mode,
                                                           cleanup_scope);
                            }
                        }
                    }
                }
                Some(def::DefStatic(_, false)) => {
                }
                _ => {
                    // Nothing to do here.
                }
            }
        }
        ast::PatStruct(_, ref fields, _) => {
            let tcx = bcx.tcx();
            let pat_ty = node_id_type(bcx, pat.id);
            let pat_repr = adt::represent_type(bcx.ccx(), pat_ty);
            expr::with_field_tys(tcx, pat_ty, Some(pat.id), |discr, field_tys| {
                for f in fields.iter() {
                    let ix = ty::field_idx_strict(tcx, f.ident.name, field_tys);
                    let fldptr = adt::trans_field_ptr(bcx, &*pat_repr, val,
                                                      discr, ix);
                    bcx = bind_irrefutable_pat(bcx, f.pat, fldptr,
                                               binding_mode, cleanup_scope);
                }
            })
        }
        ast::PatTup(ref elems) => {
            let repr = adt::represent_node(bcx, pat.id);
            for (i, elem) in elems.iter().enumerate() {
                let fldptr = adt::trans_field_ptr(bcx, &*repr, val, 0, i);
                bcx = bind_irrefutable_pat(bcx, *elem, fldptr,
                                           binding_mode, cleanup_scope);
            }
        }
        ast::PatBox(inner) => {
            let llbox = Load(bcx, val);
            bcx = bind_irrefutable_pat(bcx, inner, llbox, binding_mode, cleanup_scope);
        }
        ast::PatRegion(inner) => {
            let loaded_val = Load(bcx, val);
            bcx = bind_irrefutable_pat(bcx, inner, loaded_val, binding_mode, cleanup_scope);
        }
        ast::PatVec(ref before, ref slice, ref after) => {
            let extracted = extract_vec_elems(
                bcx, pat.id, before.len() + 1u + after.len(),
                slice.map(|_| before.len()), val
            );
            bcx = before
                .iter().map(|v| Some(*v))
                .chain(Some(*slice).move_iter())
                .chain(after.iter().map(|v| Some(*v)))
                .zip(extracted.vals.iter())
                .fold(bcx, |bcx, (inner, elem)| {
                    inner.map_or(bcx, |inner| {
                        bind_irrefutable_pat(bcx, inner, *elem, binding_mode, cleanup_scope)
                    })
                });
        }
        ast::PatMac(..) => {
            bcx.sess().span_bug(pat.span, "unexpanded macro");
        }
        ast::PatWild | ast::PatWildMulti | ast::PatLit(_) | ast::PatRange(_, _) => ()
    }
    return bcx;
}
