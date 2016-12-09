// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use {AmbiguityError, Resolver, ResolutionError, resolve_error};
use {Module, ModuleKind, NameBinding, NameBindingKind, PathScope, PathResult};
use Namespace::{self, MacroNS};
use build_reduced_graph::BuildReducedGraphVisitor;
use resolve_imports::ImportResolver;
use rustc::hir::def_id::{DefId, BUILTIN_MACROS_CRATE, CRATE_DEF_INDEX, DefIndex};
use rustc::hir::def::{Def, Export};
use rustc::hir::map::{self, DefCollector};
use rustc::ty;
use std::cell::Cell;
use std::rc::Rc;
use syntax::ast::{self, Name};
use syntax::errors::DiagnosticBuilder;
use syntax::ext::base::{self, Determinacy, MultiModifier, MultiDecorator};
use syntax::ext::base::{NormalTT, SyntaxExtension};
use syntax::ext::expand::Expansion;
use syntax::ext::hygiene::Mark;
use syntax::ext::tt::macro_rules;
use syntax::feature_gate::{emit_feature_err, GateIssue};
use syntax::fold::Folder;
use syntax::ptr::P;
use syntax::util::lev_distance::find_best_match_for_name;
use syntax::visit::Visitor;
use syntax_pos::{Span, DUMMY_SP};

#[derive(Clone)]
pub struct InvocationData<'a> {
    pub module: Cell<Module<'a>>,
    pub def_index: DefIndex,
    // True if this expansion is in a `const_integer` position, for example `[u32; m!()]`.
    // c.f. `DefCollector::visit_ast_const_integer`.
    pub const_integer: bool,
    // The scope in which the invocation path is resolved.
    pub legacy_scope: Cell<LegacyScope<'a>>,
    // The smallest scope that includes this invocation's expansion,
    // or `Empty` if this invocation has not been expanded yet.
    pub expansion: Cell<LegacyScope<'a>>,
}

impl<'a> InvocationData<'a> {
    pub fn root(graph_root: Module<'a>) -> Self {
        InvocationData {
            module: Cell::new(graph_root),
            def_index: CRATE_DEF_INDEX,
            const_integer: false,
            legacy_scope: Cell::new(LegacyScope::Empty),
            expansion: Cell::new(LegacyScope::Empty),
        }
    }
}

#[derive(Copy, Clone)]
pub enum LegacyScope<'a> {
    Empty,
    Invocation(&'a InvocationData<'a>), // The scope of the invocation, not including its expansion
    Expansion(&'a InvocationData<'a>), // The scope of the invocation, including its expansion
    Binding(&'a LegacyBinding<'a>),
}

pub struct LegacyBinding<'a> {
    pub parent: Cell<LegacyScope<'a>>,
    pub name: ast::Name,
    ext: Rc<SyntaxExtension>,
    pub span: Span,
}

pub enum MacroBinding<'a> {
    Legacy(&'a LegacyBinding<'a>),
    Modern(&'a NameBinding<'a>),
}

impl<'a> base::Resolver for Resolver<'a> {
    fn next_node_id(&mut self) -> ast::NodeId {
        self.session.next_node_id()
    }

    fn get_module_scope(&mut self, id: ast::NodeId) -> Mark {
        let mark = Mark::fresh();
        let module = self.module_map[&id];
        self.invocations.insert(mark, self.arenas.alloc_invocation_data(InvocationData {
            module: Cell::new(module),
            def_index: module.def_id().unwrap().index,
            const_integer: false,
            legacy_scope: Cell::new(LegacyScope::Empty),
            expansion: Cell::new(LegacyScope::Empty),
        }));
        mark
    }

    fn eliminate_crate_var(&mut self, item: P<ast::Item>) -> P<ast::Item> {
        struct EliminateCrateVar<'b, 'a: 'b>(&'b mut Resolver<'a>);

        impl<'a, 'b> Folder for EliminateCrateVar<'a, 'b> {
            fn fold_path(&mut self, mut path: ast::Path) -> ast::Path {
                let ident = path.segments[0].identifier;
                if ident.name == "$crate" {
                    path.global = true;
                    let module = self.0.resolve_crate_var(ident.ctxt);
                    if module.is_local() {
                        path.segments.remove(0);
                    } else {
                        path.segments[0].identifier = match module.kind {
                            ModuleKind::Def(_, name) => ast::Ident::with_empty_ctxt(name),
                            _ => unreachable!(),
                        };
                    }
                }
                path
            }
        }

        EliminateCrateVar(self).fold_item(item).expect_one("")
    }

    fn visit_expansion(&mut self, mark: Mark, expansion: &Expansion) {
        let invocation = self.invocations[&mark];
        self.collect_def_ids(invocation, expansion);

        self.current_module = invocation.module.get();
        self.current_module.unresolved_invocations.borrow_mut().remove(&mark);
        let mut visitor = BuildReducedGraphVisitor {
            resolver: self,
            legacy_scope: LegacyScope::Invocation(invocation),
            expansion: mark,
        };
        expansion.visit_with(&mut visitor);
        self.current_module.unresolved_invocations.borrow_mut().remove(&mark);
        invocation.expansion.set(visitor.legacy_scope);
    }

    fn add_macro(&mut self, scope: Mark, mut def: ast::MacroDef, export: bool) {
        if def.ident.name == "macro_rules" {
            self.session.span_err(def.span, "user-defined macros may not be named `macro_rules`");
        }

        let invocation = self.invocations[&scope];
        let binding = self.arenas.alloc_legacy_binding(LegacyBinding {
            parent: Cell::new(invocation.legacy_scope.get()),
            name: def.ident.name,
            ext: Rc::new(macro_rules::compile(&self.session.parse_sess, &def)),
            span: def.span,
        });
        invocation.legacy_scope.set(LegacyScope::Binding(binding));
        self.macro_names.insert(def.ident.name);

        if export {
            def.id = self.next_node_id();
            DefCollector::new(&mut self.definitions).with_parent(CRATE_DEF_INDEX, |collector| {
                collector.visit_macro_def(&def)
            });
            self.macro_exports.push(Export {
                name: def.ident.name,
                def: Def::Macro(self.definitions.local_def_id(def.id)),
            });
            self.exported_macros.push(def);
        }
    }

    fn add_ext(&mut self, ident: ast::Ident, ext: Rc<SyntaxExtension>) {
        if let NormalTT(..) = *ext {
            self.macro_names.insert(ident.name);
        }
        let def_id = DefId {
            krate: BUILTIN_MACROS_CRATE,
            index: DefIndex::new(self.macro_map.len()),
        };
        self.macro_map.insert(def_id, ext);
        let binding = self.arenas.alloc_name_binding(NameBinding {
            kind: NameBindingKind::Def(Def::Macro(def_id)),
            span: DUMMY_SP,
            vis: ty::Visibility::PrivateExternal,
            expansion: Mark::root(),
        });
        self.builtin_macros.insert(ident.name, binding);
    }

    fn add_expansions_at_stmt(&mut self, id: ast::NodeId, macros: Vec<Mark>) {
        self.macros_at_scope.insert(id, macros);
    }

    fn resolve_imports(&mut self) {
        ImportResolver { resolver: self }.resolve_imports()
    }

    fn find_attr_invoc(&mut self, attrs: &mut Vec<ast::Attribute>) -> Option<ast::Attribute> {
        for i in 0..attrs.len() {
            match self.builtin_macros.get(&attrs[i].name()).cloned() {
                Some(binding) => match *binding.get_macro(self) {
                    MultiModifier(..) | MultiDecorator(..) | SyntaxExtension::AttrProcMacro(..) => {
                        return Some(attrs.remove(i))
                    }
                    _ => {}
                },
                None => {}
            }
        }
        None
    }

    fn resolve_macro(&mut self, scope: Mark, path: &ast::Path, force: bool)
                     -> Result<Rc<SyntaxExtension>, Determinacy> {
        let ast::Path { ref segments, global, span } = *path;
        if segments.iter().any(|segment| !segment.parameters.is_empty()) {
            let kind =
                if segments.last().unwrap().parameters.is_empty() { "module" } else { "macro" };
            let msg = format!("type parameters are not allowed on {}s", kind);
            self.session.span_err(path.span, &msg);
            return Err(Determinacy::Determined);
        }

        let path_scope = if global { PathScope::Global } else { PathScope::Lexical };
        let path: Vec<_> = segments.iter().map(|seg| seg.identifier).collect();
        let invocation = self.invocations[&scope];
        self.current_module = invocation.module.get();

        if path.len() > 1 || global {
            if !self.use_extern_macros {
                let msg = "non-ident macro paths are experimental";
                let feature = "use_extern_macros";
                emit_feature_err(&self.session.parse_sess, feature, span, GateIssue::Language, msg);
                return Err(Determinacy::Determined);
            }

            let ext = match self.resolve_path(&path, path_scope, Some(MacroNS), None) {
                PathResult::NonModule(path_res) => Ok(self.get_macro(path_res.base_def)),
                PathResult::Module(..) => unreachable!(),
                PathResult::Indeterminate if !force => return Err(Determinacy::Undetermined),
                _ => Err(Determinacy::Determined),
            };
            self.current_module.macro_resolutions.borrow_mut()
                .push((path.into_boxed_slice(), path_scope, span));
            return ext;
        }

        let name = path[0].name;
        let result = match self.resolve_legacy_scope(&invocation.legacy_scope, name, false) {
            Some(MacroBinding::Legacy(binding)) => Ok(binding.ext.clone()),
            Some(MacroBinding::Modern(binding)) => Ok(binding.get_macro(self)),
            None => match self.resolve_lexical_macro_path_segment(name, MacroNS, None) {
                Ok(binding) => Ok(binding.get_macro(self)),
                Err(Determinacy::Undetermined) if !force => return Err(Determinacy::Undetermined),
                _ => {
                    let msg = format!("macro undefined: '{}!'", name);
                    let mut err = self.session.struct_span_err(span, &msg);
                    self.suggest_macro_name(&name.as_str(), &mut err);
                    err.emit();
                    return Err(Determinacy::Determined);
                },
            },
        };

        if self.use_extern_macros {
            self.current_module.legacy_macro_resolutions.borrow_mut().push((scope, name, span));
        }
        result
    }
}

impl<'a> Resolver<'a> {
    // Resolve the initial segment of a non-global macro path (e.g. `foo` in `foo::bar!();`)
    pub fn resolve_lexical_macro_path_segment(&mut self,
                                              name: Name,
                                              ns: Namespace,
                                              record_used: Option<Span>)
                                              -> Result<&'a NameBinding<'a>, Determinacy> {
        let mut module = self.current_module;
        let mut potential_expanded_shadower: Option<&NameBinding> = None;
        loop {
            // Since expanded macros may not shadow the lexical scope (enforced below),
            // we can ignore unresolved invocations (indicated by the penultimate argument).
            match self.resolve_name_in_module(module, name, ns, true, record_used) {
                Ok(binding) => {
                    let span = match record_used {
                        Some(span) => span,
                        None => return Ok(binding),
                    };
                    match potential_expanded_shadower {
                        Some(shadower) if shadower.def() != binding.def() => {
                            self.ambiguity_errors.push(AmbiguityError {
                                span: span, name: name, b1: shadower, b2: binding, lexical: true,
                                legacy: false,
                            });
                            return Ok(shadower);
                        }
                        _ if binding.expansion == Mark::root() => return Ok(binding),
                        _ => potential_expanded_shadower = Some(binding),
                    }
                },
                Err(Determinacy::Undetermined) => return Err(Determinacy::Undetermined),
                Err(Determinacy::Determined) => {}
            }

            match module.kind {
                ModuleKind::Block(..) => module = module.parent.unwrap(),
                ModuleKind::Def(..) => return match potential_expanded_shadower {
                    Some(binding) => Ok(binding),
                    None if record_used.is_some() => Err(Determinacy::Determined),
                    None => Err(Determinacy::Undetermined),
                },
            }
        }
    }

    pub fn resolve_legacy_scope(&mut self,
                                mut scope: &'a Cell<LegacyScope<'a>>,
                                name: Name,
                                record_used: bool)
                                -> Option<MacroBinding<'a>> {
        let mut possible_time_travel = None;
        let mut relative_depth: u32 = 0;
        let mut binding = None;
        loop {
            match scope.get() {
                LegacyScope::Empty => break,
                LegacyScope::Expansion(invocation) => {
                    match invocation.expansion.get() {
                        LegacyScope::Invocation(_) => scope.set(invocation.legacy_scope.get()),
                        LegacyScope::Empty => {
                            if possible_time_travel.is_none() {
                                possible_time_travel = Some(scope);
                            }
                            scope = &invocation.legacy_scope;
                        }
                        _ => {
                            relative_depth += 1;
                            scope = &invocation.expansion;
                        }
                    }
                }
                LegacyScope::Invocation(invocation) => {
                    relative_depth = relative_depth.saturating_sub(1);
                    scope = &invocation.legacy_scope;
                }
                LegacyScope::Binding(potential_binding) => {
                    if potential_binding.name == name {
                        if (!self.use_extern_macros || record_used) && relative_depth > 0 {
                            self.disallowed_shadowing.push(potential_binding);
                        }
                        binding = Some(potential_binding);
                        break
                    }
                    scope = &potential_binding.parent;
                }
            };
        }

        let binding = match binding {
            Some(binding) => MacroBinding::Legacy(binding),
            None => match self.builtin_macros.get(&name).cloned() {
                Some(binding) => MacroBinding::Modern(binding),
                None => return None,
            },
        };

        if !self.use_extern_macros {
            if let Some(scope) = possible_time_travel {
                // Check for disallowed shadowing later
                self.lexical_macro_resolutions.push((name, scope));
            }
        }

        Some(binding)
    }

    pub fn finalize_current_module_macro_resolutions(&mut self) {
        let module = self.current_module;
        for &(ref path, scope, span) in module.macro_resolutions.borrow().iter() {
            match self.resolve_path(path, scope, Some(MacroNS), Some(span)) {
                PathResult::NonModule(_) => {},
                PathResult::Failed(msg, _) => {
                    resolve_error(self, span, ResolutionError::FailedToResolve(&msg));
                }
                _ => unreachable!(),
            }
        }

        for &(mark, name, span) in module.legacy_macro_resolutions.borrow().iter() {
            let legacy_scope = &self.invocations[&mark].legacy_scope;
            let legacy_resolution = self.resolve_legacy_scope(legacy_scope, name, true);
            let resolution = self.resolve_lexical_macro_path_segment(name, MacroNS, Some(span));
            let (legacy_resolution, resolution) = match (legacy_resolution, resolution) {
                (Some(legacy_resolution), Ok(resolution)) => (legacy_resolution, resolution),
                _ => continue,
            };
            let (legacy_span, participle) = match legacy_resolution {
                MacroBinding::Modern(binding) if binding.def() == resolution.def() => continue,
                MacroBinding::Modern(binding) => (binding.span, "imported"),
                MacroBinding::Legacy(binding) => (binding.span, "defined"),
            };
            let msg1 = format!("`{}` could resolve to the macro {} here", name, participle);
            let msg2 = format!("`{}` could also resolve to the macro imported here", name);
            self.session.struct_span_err(span, &format!("`{}` is ambiguous", name))
                .span_note(legacy_span, &msg1)
                .span_note(resolution.span, &msg2)
                .emit();
        }
    }

    fn suggest_macro_name(&mut self, name: &str, err: &mut DiagnosticBuilder<'a>) {
        if let Some(suggestion) = find_best_match_for_name(self.macro_names.iter(), name, None) {
            if suggestion != name {
                err.help(&format!("did you mean `{}!`?", suggestion));
            } else {
                err.help(&format!("have you added the `#[macro_use]` on the module/import?"));
            }
        }
    }

    fn collect_def_ids(&mut self, invocation: &'a InvocationData<'a>, expansion: &Expansion) {
        let Resolver { ref mut invocations, arenas, graph_root, .. } = *self;
        let InvocationData { def_index, const_integer, .. } = *invocation;

        let visit_macro_invoc = &mut |invoc: map::MacroInvocationData| {
            invocations.entry(invoc.mark).or_insert_with(|| {
                arenas.alloc_invocation_data(InvocationData {
                    def_index: invoc.def_index,
                    const_integer: invoc.const_integer,
                    module: Cell::new(graph_root),
                    expansion: Cell::new(LegacyScope::Empty),
                    legacy_scope: Cell::new(LegacyScope::Empty),
                })
            });
        };

        let mut def_collector = DefCollector::new(&mut self.definitions);
        def_collector.visit_macro_invoc = Some(visit_macro_invoc);
        def_collector.with_parent(def_index, |def_collector| {
            if const_integer {
                if let Expansion::Expr(ref expr) = *expansion {
                    def_collector.visit_ast_const_integer(expr);
                }
            }
            expansion.visit_with(def_collector)
        });
    }
}
