use std::{ops::Deref, path::Path};

use dashmap::DashMap;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

use crate::{
    containers::{List, Set, Symbol},
    context::{Ctx, CtxErr, CtxResult, ModuleId, ToCtx, ToCtxErr},
    grammar::{parse_program, RawConstExpr, RawDefn, RawExpr, RawProgram, RawTypeExpr},
};

/// A struct that encapsulates a parallel demodularizer that eliminates "require" and "provide" in a raw AST.
pub struct Demodularizer {
    cache: DashMap<ModuleId, Ctx<RawProgram>>,
    fallback: Box<dyn Fn(ModuleId) -> anyhow::Result<String> + Send + Sync + 'static>,
}

impl Demodularizer {
    /// Creates a new demodularizer, rooted at some filesystem.
    pub fn new_at_fs(root: &Path) -> Self {
        let root = root.to_owned();
        let fallback = move |mid: ModuleId| {
            let mut root = root.clone();
            root.push(&mid.to_string());
            Ok(std::fs::read_to_string(&root)?)
        };
        Self {
            cache: DashMap::new(),
            fallback: Box::new(fallback),
        }
    }

    /// Return the demodularized version of some module ID.
    pub fn demod(&self, id: ModuleId) -> CtxResult<Ctx<RawProgram>> {
        if let Some(res) = self.cache.get(&id) {
            log::debug!("demod {} HIT!", id);
            Ok(res.deref().clone())
        } else {
            log::debug!("demod {} MISS!", id);
            // populate the cache
            let raw_string = (self.fallback)(id).err_ctx(None)?;
            let parsed = parse_program(&raw_string, id)?;
            // go through the dependencies in parallel, demodularizing as we go
            let new_defs = parsed
                .definitions
                .par_iter()
                .fold(
                    || Ok::<_, CtxErr>(List::new()),
                    |accum, def| {
                        let mut accum = accum?;
                        match def.deref() {
                            RawDefn::Require(other) => {
                                let other_demodularized = self.demod(*other)?;
                                accum.append(mangle(
                                    other_demodularized.definitions.clone(),
                                    *other,
                                ));
                            }
                            _ => accum.push_back(def.clone()),
                        }
                        Ok(accum)
                    },
                )
                .reduce(
                    || Ok::<_, CtxErr>(List::new()),
                    |a, b| {
                        let mut a = a?;
                        a.append(b?);
                        Ok(a)
                    },
                )?;
            Ok(RawProgram {
                definitions: new_defs,
                body: parsed.body.clone(),
            }
            .with_ctx(parsed.ctx()))
        }
    }
}

fn mangle(defs: List<Ctx<RawDefn>>, source: ModuleId) -> List<Ctx<RawDefn>> {
    let no_mangle: Set<Symbol> = defs
        .iter()
        .filter_map(|a| {
            if let RawDefn::Provide(a) = a.deref() {
                Some(*a)
            } else {
                None
            }
        })
        .collect();
    log::debug!("no_mangle for {}: {:?}", source, no_mangle);
    defs.into_iter()
        .filter_map(|defn| {
            match defn.deref().clone() {
                RawDefn::Function {
                    name,
                    cgvars,
                    genvars,
                    args,
                    rettype,
                    body,
                } => {
                    let inner_nomangle = cgvars
                        .iter()
                        .chain(genvars.iter())
                        .map(|a| **a)
                        .chain(args.iter().map(|a| *a.name))
                        .fold(no_mangle.clone(), |mut acc, s| {
                            acc.insert(s);
                            acc
                        });
                    Some(RawDefn::Function {
                        name: mangle_ctx_sym(name, source, &no_mangle),
                        cgvars,
                        genvars,
                        args: args
                            .into_iter()
                            .map(|arg| {
                                let ctx = arg.ctx();
                                let mut arg = arg.deref().clone();
                                let new_bind =
                                    mangle_type_expr(arg.bind.clone(), source, &inner_nomangle);
                                arg.bind = new_bind;
                                arg.with_ctx(ctx)
                            })
                            .collect(),
                        rettype: rettype.map(|rt| mangle_type_expr(rt, source, &no_mangle)),
                        body: mangle_expr(body, source, &inner_nomangle),
                    })
                }
                RawDefn::Struct { name, fields } => Some(RawDefn::Struct {
                    name: mangle_ctx_sym(name, source, &no_mangle),
                    fields,
                }),
                RawDefn::Constant(_, _) => todo!(),
                RawDefn::Require(_) => None,
                RawDefn::Provide(_) => None,
            }
            .map(|c| c.with_ctx(defn.ctx()))
        })
        .collect()
}

fn mangle_expr(expr: Ctx<RawExpr>, source: ModuleId, no_mangle: &Set<Symbol>) -> Ctx<RawExpr> {
    let recurse = |expr| mangle_expr(expr, source, no_mangle);
    let ctx = expr.ctx();
    match expr.deref().clone() {
        RawExpr::Let(sym, bind, body) => {
            let mut inner_no_mangle = no_mangle.clone();
            inner_no_mangle.insert(*sym);
            RawExpr::Let(sym, bind, mangle_expr(body, source, &inner_no_mangle))
        }
        RawExpr::If(cond, a, b) => RawExpr::If(recurse(cond), recurse(a), recurse(b)),
        RawExpr::BinOp(op, a, b) => RawExpr::BinOp(op, recurse(a), recurse(b)),
        RawExpr::LitNum(a) => RawExpr::LitNum(a),
        RawExpr::LitVec(v) => RawExpr::LitVec(v.into_iter().map(recurse).collect()),
        RawExpr::LitStruct(a, fields) => RawExpr::LitStruct(
            mangle_sym(a, source, no_mangle),
            fields.into_iter().map(|(k, b)| (k, recurse(b))).collect(),
        ),
        RawExpr::Var(v) => RawExpr::Var(mangle_sym(v, source, no_mangle)),
        RawExpr::CgVar(v) => RawExpr::CgVar(mangle_sym(v, source, no_mangle)),
        RawExpr::Apply(f, args) => {
            RawExpr::Apply(recurse(f), args.into_iter().map(recurse).collect())
        }
        RawExpr::Field(a, b) => RawExpr::Field(recurse(a), b),
        RawExpr::VectorRef(v, i) => RawExpr::VectorRef(recurse(v), recurse(i)),
        RawExpr::VectorSlice(v, i, j) => RawExpr::VectorSlice(recurse(v), recurse(i), recurse(j)),
        RawExpr::VectorUpdate(v, i, x) => RawExpr::VectorUpdate(recurse(v), recurse(i), recurse(x)),
        RawExpr::Loop(n, bod, end) => RawExpr::Loop(
            mangle_const_expr(n, source, no_mangle),
            bod.into_iter()
                .map(|(k, v)| (mangle_sym(k, source, no_mangle), recurse(v)))
                .collect(),
            recurse(end),
        ),
        RawExpr::IsType(a, t) => RawExpr::IsType(
            mangle_sym(a, source, no_mangle),
            mangle_type_expr(t, source, no_mangle),
        ),
        RawExpr::AsType(a, t) => {
            RawExpr::AsType(recurse(a), mangle_type_expr(t, source, no_mangle))
        }
        RawExpr::Fail => RawExpr::Fail,
    }
    .with_ctx(ctx)
}

fn mangle_const_expr(
    sym: Ctx<RawConstExpr>,
    source: ModuleId,
    no_mangle: &Set<Symbol>,
) -> Ctx<RawConstExpr> {
    let recurse = |sym| mangle_const_expr(sym, source, no_mangle);
    match sym.deref().clone() {
        RawConstExpr::Sym(s) => RawConstExpr::Sym(mangle_sym(s, source, no_mangle)),
        RawConstExpr::Lit(l) => RawConstExpr::Lit(l),
        RawConstExpr::Plus(a, b) => RawConstExpr::Plus(recurse(a), recurse(b)),
        RawConstExpr::Mult(a, b) => RawConstExpr::Mult(recurse(a), recurse(b)),
    }
    .with_ctx(sym.ctx())
}

fn mangle_ctx_sym(sym: Ctx<Symbol>, source: ModuleId, no_mangle: &Set<Symbol>) -> Ctx<Symbol> {
    mangle_sym(*sym, source, no_mangle).with_ctx(sym.ctx())
}

fn mangle_sym(sym: Symbol, source: ModuleId, no_mangle: &Set<Symbol>) -> Symbol {
    if no_mangle.contains(&sym) {
        sym
    } else {
        Symbol::from(format!("{:?}-{}", sym, source.uniqid()).as_str())
    }
}

fn mangle_type_expr(
    bind: Ctx<RawTypeExpr>,
    source: ModuleId,
    no_mangle: &Set<Symbol>,
) -> Ctx<RawTypeExpr> {
    let recurse = |bind| mangle_type_expr(bind, source, no_mangle);
    match bind.deref().clone() {
        RawTypeExpr::Sym(s) => RawTypeExpr::Sym(mangle_sym(s, source, no_mangle)),
        RawTypeExpr::Union(a, b) => RawTypeExpr::Union(recurse(a), recurse(b)),
        RawTypeExpr::Vector(v) => RawTypeExpr::Vector(v.into_iter().map(recurse).collect()),
        RawTypeExpr::Vectorof(v, n) => {
            RawTypeExpr::Vectorof(recurse(v), mangle_const_expr(n, source, no_mangle))
        }
        RawTypeExpr::NatRange(i, j) => RawTypeExpr::NatRange(
            mangle_const_expr(i, source, no_mangle),
            mangle_const_expr(j, source, no_mangle),
        ),
    }
    .with_ctx(bind.ctx())
}