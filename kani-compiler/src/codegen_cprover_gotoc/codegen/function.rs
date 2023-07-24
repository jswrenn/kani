// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! This file contains functions related to codegenning MIR functions into gotoc

use crate::codegen_cprover_gotoc::GotocCtx;
use crate::kani_middle::contracts::GFnContract;
use cbmc::goto_program::{Expr, FunctionContract, Lambda, Stmt, Symbol, Type};
use cbmc::InternString;
use rustc_middle::mir::traversal::reverse_postorder;
use rustc_middle::mir::{Body, HasLocalDecls, Local};
use rustc_middle::ty::{self, Instance};
use std::collections::BTreeMap;
use std::iter::FromIterator;
use tracing::{debug, debug_span};

/// Codegen MIR functions into gotoc
impl<'tcx> GotocCtx<'tcx> {
    /// Get the number of parameters that the current function expects.
    fn get_params_size(&self) -> usize {
        let sig = self.current_fn().sig();
        let sig = self.tcx.normalize_erasing_late_bound_regions(ty::ParamEnv::reveal_all(), sig);
        // we don't call [codegen_function_sig] because we want to get a bit more metainformation.
        sig.inputs().len()
    }

    /// Declare variables according to their index.
    /// - Index 0 represents the return value.
    /// - Indices [1, N] represent the function parameters where N is the number of parameters.
    /// - Indices that are greater than N represent local variables.
    fn codegen_declare_variables(&mut self) {
        let mir = self.current_fn().mir();
        let ldecls = mir.local_decls();
        let num_args = self.get_params_size();
        ldecls.indices().enumerate().for_each(|(idx, lc)| {
            if Some(lc) == mir.spread_arg {
                // We have already added this local in the function prelude, so
                // skip adding it again here.
                return;
            }
            let base_name = self.codegen_var_base_name(&lc);
            let name = self.codegen_var_name(&lc);
            let ldata = &ldecls[lc];
            let var_ty = self.monomorphize(ldata.ty);
            let var_type = self.codegen_ty(var_ty);
            let loc = self.codegen_span(&ldata.source_info.span);
            // Indices [1, N] represent the function parameters where N is the number of parameters.
            // Except that ZST fields are not included as parameters.
            let sym = Symbol::variable(
                name,
                base_name,
                var_type,
                self.codegen_span(&ldata.source_info.span),
            )
            .with_is_hidden(!self.is_user_variable(&lc))
            .with_is_parameter((idx > 0 && idx <= num_args) && !self.is_zst(var_ty));
            let sym_e = sym.to_expr();
            self.symbol_table.insert(sym);

            // Index 0 represents the return value, which does not need to be
            // declared in the first block
            if lc.index() < 1 || lc.index() > mir.arg_count {
                let init = self.codegen_default_initializer(&sym_e);
                self.current_fn_mut().push_onto_block(Stmt::decl(sym_e, init, loc));
            }
        });
    }

    pub fn codegen_function(&mut self, instance: Instance<'tcx>) {
        self.set_current_fn(instance);
        let name = self.current_fn().name();
        let old_sym = self.symbol_table.lookup(&name).unwrap();

        let _trace_span =
            debug_span!("CodegenFunction", name = self.current_fn().readable_name()).entered();
        if old_sym.is_function_definition() {
            debug!("Double codegen of {:?}", old_sym);
        } else {
            assert!(old_sym.is_function());
            let mir = self.current_fn().mir();
            self.print_instance(instance, mir);
            self.codegen_function_prelude();
            self.codegen_declare_variables();

            reverse_postorder(mir).for_each(|(bb, bbd)| self.codegen_block(bb, bbd));

            let loc = self.codegen_span(&mir.span);
            let stmts = self.current_fn_mut().extract_block();
            let body = Stmt::block(stmts, loc);
            self.symbol_table.update_fn_declaration_with_definition(&name, body);
        }
        self.reset_current_fn();
    }

    /// Codegen changes required due to the function ABI.
    /// We currently untuple arguments for RustCall ABI where the `spread_arg` is set.
    fn codegen_function_prelude(&mut self) {
        let mir = self.current_fn().mir();
        if let Some(spread_arg) = mir.spread_arg {
            self.codegen_spread_arg(mir, spread_arg);
        }
    }

    /// MIR functions have a `spread_arg` field that specifies whether the
    /// final argument to the function is "spread" at the LLVM/codegen level
    /// from a tuple into its individual components. (Used for the "rust-
    /// call" ABI, necessary because the function traits and closures cannot have an
    /// argument list in MIR that is both generic and variadic, so Rust
    /// allows a generic tuple).
    ///
    /// These tuples are used in the MIR to invoke a shim, and it's used in the shim body.
    ///
    /// The `spread_arg` represents the the local variable that is to be "spread"/untupled.
    /// However, the function body itself may refer to the members of
    /// the tuple instead of the individual spread parameters, so we need to add to the
    /// function prelude code that _retuples_, that is, writes the arguments
    /// back to a local tuple that can be used in the body.
    ///
    /// See:
    /// <https://rust-lang.zulipchat.com/#narrow/stream/182449-t-compiler.2Fhelp/topic/Determine.20untupled.20closure.20args.20from.20Instance.3F>
    fn codegen_spread_arg(&mut self, mir: &Body<'tcx>, spread_arg: Local) {
        tracing::debug!(current=?self.current_fn, "codegen_spread_arg");
        let spread_data = &mir.local_decls()[spread_arg];
        let tup_ty = self.monomorphize(spread_data.ty);
        if self.is_zst(tup_ty) {
            // No need to spread a ZST since it will be ignored.
            return;
        }

        let loc = self.codegen_span(&spread_data.source_info.span);

        // Get the function signature from MIR, _before_ we untuple
        let fntyp = self.current_fn().instance().ty(self.tcx, ty::ParamEnv::reveal_all());
        let sig = match fntyp.kind() {
            ty::FnPtr(..) | ty::FnDef(..) => fntyp.fn_sig(self.tcx).skip_binder(),
            // Closures themselves will have their arguments already untupled,
            // see Zulip link above.
            ty::Closure(..) => unreachable!(
                "Unexpected `spread arg` set for closure, got: {:?}, {:?}",
                fntyp,
                self.current_fn().readable_name()
            ),
            _ => unreachable!(
                "Expected function type for `spread arg` prelude, got: {:?}, {:?}",
                fntyp,
                self.current_fn().readable_name()
            ),
        };

        // When we codegen the function signature elsewhere, we will codegen the untupled version.
        // We then marshall the arguments into a local variable holding the expected tuple.
        // For a function with args f(a: t1, b: t2, c: t3), the tuple type will look like
        // ```
        //    struct T {
        //        0: t1,
        //        1: t2,
        //        2: t3,
        // }
        // ```
        // For e.g., in the test `tupled_closure.rs`, the tuple type looks like:
        // ```
        // struct _8098103865751214180
        // {
        //    unsigned long int 1;
        //    unsigned char 0;
        //    struct _3159196586427472662 2;
        // };
        // ```
        // Note how the compiler has reordered the fields to improve packing.
        let tup_type = self.codegen_ty(tup_ty);

        // We need to marshall the arguments into the tuple
        // The arguments themselves have been tacked onto the explicit function paramaters by
        // the code in `pub fn fn_typ(&mut self) -> Type {` in `typ.rs`.
        // By convention, they are given the names `spread<i>`.
        // For e.g., in the test `tupled_closure.rs`, the actual function looks like
        // ```
        // unsigned long int _RNvYNvCscgV8bIzQQb7_14tupled_closure1hINtNtNtCsaGHNm3cehi1_4core3ops8function2FnThjINtNtBH_6option6OptionNtNtNtBH_3num7nonzero12NonZeroUsizeEEE4callB4_(
        //        unsigned long int (*var_1)(unsigned char, unsigned long int, struct _3159196586427472662),
        //        unsigned char spread_2,
        //        unsigned long int spread_3,
        //        struct _3159196586427472662 spread_4) {
        //  struct _8098103865751214180 var_2={ .1=spread_3, .0=spread_2, .2=spread_4 };
        //  unsigned long int var_0=(_RNvCscgV8bIzQQb7_14tupled_closure1h)(var_2.0, var_2.1, var_2.2);
        //  return var_0;
        // }
        // ```

        let tupe = sig.inputs().last().unwrap();
        let args = match tupe.kind() {
            ty::Tuple(substs) => *substs,
            _ => unreachable!("a function's spread argument must be a tuple"),
        };
        let starting_idx = sig.inputs().len();
        let marshalled_tuple_fields =
            BTreeMap::from_iter(args.iter().enumerate().map(|(arg_i, arg_t)| {
                // The components come at the end, so offset by the untupled length.
                // This follows the naming convention defined in `typ.rs`.
                let lc = Local::from_usize(arg_i + starting_idx);
                let (name, base_name) = self.codegen_spread_arg_name(&lc);
                let sym = Symbol::variable(name, base_name, self.codegen_ty(arg_t), loc)
                    .with_is_hidden(false)
                    .with_is_parameter(!self.is_zst(arg_t));
                // The spread arguments are additional function paramaters that are patched in
                // They are to the function signature added in the `fn_typ` function.
                // But they were never added to the symbol table, which we currently do here.
                // https://github.com/model-checking/kani/issues/686 to track a better solution.
                self.symbol_table.insert(sym.clone());
                // As discussed above, fields are named like `0: t1`.
                // Follow that pattern for the marshalled data.
                // name:value map is resilliant to rustc reordering fields (see above)
                (arg_i.to_string().intern(), sym.to_expr())
            }));
        let marshalled_tuple_value =
            Expr::struct_expr(tup_type.clone(), marshalled_tuple_fields, &self.symbol_table)
                .with_location(loc);
        self.declare_variable(
            self.codegen_var_name(&spread_arg),
            self.codegen_var_base_name(&spread_arg),
            tup_type,
            Some(marshalled_tuple_value),
            loc,
        );
    }

    /// Convert the Kani level contract into a CBMC level contract by creating a
    /// lambda that calls the contract implementation function.
    ///
    /// For instance say we are processing a contract on `f`
    ///
    /// ```rs
    /// as_goto_contract(..., GFnContract { requires: <contact_impl_fn>, .. })
    ///     = Contract {
    ///         requires: [
    ///             Lambda {
    ///                 arguments: <return arg, args of f...>,
    ///                 body: Call(codegen_fn_expr(contract_impl_fn), [args of f..., return arg])
    ///             }
    ///         ],
    ///         ...
    ///     }
    /// ```
    ///
    /// A spec lambda in GOTO receives as its first argument the return value of
    /// the annotated function. However at the top level we must receive `self`
    /// as first argument, because rust requires it. As a result the generated
    /// lambda takes the return value as first argument and then immediately
    /// calls the generated spec function, but passing the return value as the
    /// last argument.
    fn as_goto_contract(&mut self, fn_contract: &GFnContract<Instance<'tcx>>) -> FunctionContract {
        use rustc_middle::mir;
        let mut handle_contract_expr = |instance| {
            let mir = self.current_fn().mir();
            assert!(mir.spread_arg.is_none());
            let func_expr = self.codegen_func_expr(instance, None);
            let mut mir_arguments: Vec<_> =
                std::iter::successors(Some(mir::RETURN_PLACE + 1), |i| Some(*i + 1))
                    .take(mir.arg_count + 1) // one extra for return value
                    .collect();
            let return_arg = mir_arguments.pop().unwrap();
            let mir_operands: Vec<_> =
                mir_arguments.iter().map(|l| mir::Operand::Copy((*l).into())).collect();
            let mut arguments = self.codegen_funcall_args(&mir_operands, true);
            let goto_argument_types: Vec<_> = [mir::RETURN_PLACE]
                .into_iter()
                .chain(mir_arguments.iter().copied())
                .map(|a| self.codegen_ty(self.monomorphize(mir.local_decls()[a].ty)))
                .collect();

            mir_arguments.insert(0, return_arg);
            arguments.push(Expr::symbol_expression(
                self.codegen_var_name(&return_arg),
                goto_argument_types.first().unwrap().clone(),
            ));
            Lambda {
                arguments: mir_arguments
                    .into_iter()
                    .map(|l| self.codegen_var_name(&l).into())
                    .zip(goto_argument_types)
                    .collect(),
                body: func_expr.call(arguments).cast_to(Type::Bool),
            }
        };

        let requires =
            fn_contract.requires().iter().copied().map(&mut handle_contract_expr).collect();
        let ensures =
            fn_contract.ensures().iter().copied().map(&mut handle_contract_expr).collect();
        FunctionContract::new(requires, ensures, vec![])
    }

    /// Convert the contract to a CBMC contract, then attach it to `instance`.
    /// `instance` must have previously been declared.
    ///
    /// This does not overwrite prior contracts but merges with them.
    pub fn attach_contract(
        &mut self,
        instance: Instance<'tcx>,
        contract: &GFnContract<Instance<'tcx>>,
    ) {
        // This should be safe, since the contract is pretty much evaluated as
        // though it was the first (or last) assertion in the function.
        self.set_current_fn(instance);
        let goto_contract = self.as_goto_contract(contract);
        let name = self.current_fn().name();
        self.symbol_table.attach_contract(name, goto_contract);
        self.reset_current_fn()
    }

    pub fn declare_function(&mut self, instance: Instance<'tcx>) {
        debug!("declaring {}; {:?}", instance, instance);
        self.set_current_fn(instance);
        debug!(krate = self.current_fn().krate().as_str());
        debug!(is_std = self.current_fn().is_std());
        self.ensure(&self.current_fn().name(), |ctx, fname| {
            let mir = ctx.current_fn().mir();
            Symbol::function(
                fname,
                ctx.fn_typ(),
                None,
                ctx.current_fn().readable_name(),
                ctx.codegen_span(&mir.span),
            )
        });
        self.reset_current_fn();
    }
}
