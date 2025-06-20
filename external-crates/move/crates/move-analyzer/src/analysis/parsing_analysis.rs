// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    analysis::{DefMap, add_member_use_def},
    symbols::{
        cursor::{CursorContext, CursorDefinition, CursorPosition},
        ignored_function,
        mod_defs::{AutoImportInsertionInfo, AutoImportInsertionKind, CallInfo, ModuleDefs},
        parsed_address,
        use_def::{References, UseDef, UseDefMap},
    },
    utils::{loc_end_to_lsp_position_opt, loc_start_to_lsp_position_opt},
};

use lsp_types::Position;

use std::collections::BTreeMap;

use move_compiler::{
    parser::ast as P,
    shared::{Identifier, Name, NamedAddressMap, NamedAddressMaps, files::MappedFiles},
};
use move_ir_types::location::*;

pub struct ParsingAnalysisContext<'a> {
    /// Outermost definitions in a module (structs, consts, functions), keyd on a ModuleIdent
    /// string so that we can access it regardless of the ModuleIdent representation
    /// (e.g., in the parsing AST or in the typing AST)
    pub mod_outer_defs: &'a mut BTreeMap<String, ModuleDefs>,
    /// Mapped file information for translating locations into positions
    pub files: &'a MappedFiles,
    /// Associates uses for a given definition to allow displaying all references
    pub references: &'a mut References,
    /// Additional information about definitions
    pub def_info: &'a mut DefMap,
    /// A UseDefMap for a given module (needs to be appropriately set before the module
    /// processing starts)
    pub use_defs: UseDefMap,
    /// Current module identifier string (needs to be appropriately set before the module
    /// processing starts)
    pub current_mod_ident_str: Option<String>,
    /// Module name lengths in access paths for a given module (needs to be appropriately
    /// set before the module processing starts)
    pub alias_lengths: BTreeMap<Position, usize>,
    /// A per-package mapping from package names to their addresses (needs to be appropriately set
    /// before the package processint starts)
    pub pkg_addresses: &'a NamedAddressMap,
    /// Cursor contextual information, computed as part of the traversal.
    pub cursor: Option<&'a mut CursorContext>,
}

macro_rules! update_cursor {
    ($cursor:expr, $subject:expr, $kind:ident) => {
        if let Some(cursor) = &mut $cursor {
            if $subject.loc.contains(&cursor.loc) {
                cursor.position = CursorPosition::$kind($subject.clone());
            }
        };
    };
    (IDENT, $cursor:expr, $subject:expr, $kind:ident) => {
        if let Some(cursor) = &mut $cursor {
            if $subject.loc().contains(&cursor.loc) {
                cursor.position = CursorPosition::$kind($subject.clone());
            }
        };
    };
}

impl<'a> ParsingAnalysisContext<'a> {
    /// Get symbols for the whole program
    pub fn prog_symbols(
        &mut self,
        prog: &'a P::Program,
        mod_use_defs: &mut BTreeMap<String, UseDefMap>,
        mod_to_alias_lengths: &mut BTreeMap<String, BTreeMap<Position, usize>>,
    ) {
        prog.source_definitions.iter().for_each(|pkg_def| {
            self.pkg_symbols(
                &prog.named_address_maps,
                pkg_def,
                mod_use_defs,
                mod_to_alias_lengths,
            )
        });
        prog.lib_definitions.iter().for_each(|pkg_def| {
            self.pkg_symbols(
                &prog.named_address_maps,
                pkg_def,
                mod_use_defs,
                mod_to_alias_lengths,
            )
        });
    }

    /// Get symbols for the whole package
    fn pkg_symbols(
        &mut self,
        pkg_address_maps: &'a NamedAddressMaps,
        pkg_def: &P::PackageDefinition,
        mod_use_defs: &mut BTreeMap<String, UseDefMap>,
        mod_to_alias_lengths: &mut BTreeMap<String, BTreeMap<Position, usize>>,
    ) {
        if let P::Definition::Module(mod_def) = &pkg_def.def {
            let pkg_addresses = pkg_address_maps.get(pkg_def.named_address_map);
            let old_addresses = std::mem::replace(&mut self.pkg_addresses, pkg_addresses);
            self.mod_symbols(mod_def, mod_use_defs, mod_to_alias_lengths);
            self.current_mod_ident_str = None;
            let _ = std::mem::replace(&mut self.pkg_addresses, old_addresses);
        }
    }

    fn attr_symbols(&mut self, sp!(_attr_loc, attr): &P::Attribute) {
        use P::Attribute_ as A;
        // TODO: This is under-supported in the current cursor position system. We could make
        // pretty good suggestions with a little work, though.
        match attr {
            A::BytecodeInstruction
            | A::DefinesPrimitive(..)
            | A::Deprecation { .. }
            | A::Error { .. }
            | A::Mode { .. }
            | A::Syntax { .. }
            | A::Allow { .. }
            | A::LintAllow { .. } => (),
            A::External { attrs } => {
                // attrs: Spanned<Vec<ParsedAttribute>>
                for parsed in &attrs.value {
                    self.parsed_attr_symbols(parsed);
                }
            }
            A::Test | A::RandomTest => {}
            A::ExpectedFailure {
                minor_status,
                failure_kind,
                ..
            } => {
                let failure_kind = &failure_kind.as_ref().value;
                match failure_kind {
                    P::ExpectedFailureKind_::Empty => (),
                    P::ExpectedFailureKind_::Name(_) => (),
                    P::ExpectedFailureKind_::MajorStatus(_) => (),
                    P::ExpectedFailureKind_::AbortCode(value) => {
                        update_cursor!(self.cursor, *value, Attribute);
                    }
                }
                if let Some(value) = minor_status {
                    update_cursor!(self.cursor, *value, Attribute);
                }
            }
        }
    }

    /// Walk a `ParsedAttribute` (used by `External`) similarly.
    fn parsed_attr_symbols(&mut self, sp!(_loc, pattr): &P::ParsedAttribute) {
        use P::ParsedAttribute_ as P;
        match pattr {
            P::Name(_) => (),
            P::Assigned(_, boxed_val) => {
                update_cursor!(self.cursor, **boxed_val, Attribute);
            }
            P::Parameterized(_, sp!(_, args)) => {
                for arg in args {
                    self.parsed_attr_symbols(arg);
                }
            }
        }
    }

    /// Get symbols for the whole module
    fn mod_symbols(
        &mut self,
        mod_def: &P::ModuleDefinition,
        mod_use_defs: &mut BTreeMap<String, UseDefMap>,
        mod_to_alias_lengths: &mut BTreeMap<String, BTreeMap<Position, usize>>,
    ) {
        fn latest_loc(latest_loc: Loc, new_loc: Loc) -> Loc {
            if new_loc.end() > latest_loc.end() {
                new_loc
            } else {
                latest_loc
            }
        }
        fn earliest_loc(earliest_loc: Loc, new_loc: Loc) -> Loc {
            if new_loc.start() < earliest_loc.start() {
                new_loc
            } else {
                earliest_loc
            }
        }
        // parsing symbolicator is currently only responsible for processing use declarations
        let Some(mod_ident_str) = parsing_mod_def_to_map_key(self.pkg_addresses, mod_def) else {
            return;
        };
        assert!(self.current_mod_ident_str.is_none());
        self.current_mod_ident_str = Some(mod_ident_str.clone());

        let use_defs = mod_use_defs.remove(&mod_ident_str).unwrap();
        let old_defs = std::mem::replace(&mut self.use_defs, use_defs);
        let alias_lengths: BTreeMap<Position, usize> = BTreeMap::new();
        let old_alias_lengths = std::mem::replace(&mut self.alias_lengths, alias_lengths);

        mod_def
            .attributes
            .iter()
            .for_each(|sp!(_, attrs)| attrs.0.iter().for_each(|a| self.attr_symbols(a)));

        // location of the latest use declaration (if any)
        let mut latest_use_loc = Loc::new(mod_def.loc.file_hash(), 0, 0);
        // location of the earliest member (if any)
        let mut earliest_member_loc = Loc::new(mod_def.loc.file_hash(), u32::MAX, u32::MAX);
        // we need this to avoid adding auto-imports for `use` declarations
        // in "use blocks" that follow the follow the initial one (and come
        // after other module members)
        let mut non_use_member_after_use = false;
        for m in &mod_def.members {
            use P::ModuleMember as MM;
            match m {
                MM::Function(fun) => {
                    if ignored_function(fun.name.value()) {
                        continue;
                    }
                    if latest_use_loc.end() > 0 {
                        non_use_member_after_use = true;
                    }
                    earliest_member_loc =
                        earliest_loc(earliest_member_loc, fun.doc.loc().unwrap_or(fun.loc));
                    // Unit returns span the entire function signature, so we process them first
                    // for cursor ordering.
                    self.type_symbols(&fun.signature.return_type);

                    // If the cursor is in this item, mark that down.
                    // This may be overridden by the recursion below.
                    if let Some(cursor) = &mut self.cursor {
                        if fun.name.loc().contains(&cursor.loc) {
                            cursor.position = CursorPosition::DefName;
                            debug_assert!(cursor.defn_name.is_none());
                            cursor.defn_name = Some(CursorDefinition::Function(fun.name));
                        } else if fun.loc.contains(&cursor.loc) {
                            cursor.defn_name = Some(CursorDefinition::Function(fun.name));
                        }
                    };

                    fun.attributes.iter().for_each(|sp!(_, attrs)| {
                        attrs.0.iter().for_each(|a| self.attr_symbols(a))
                    });

                    for (_, x, t) in fun.signature.parameters.iter() {
                        update_cursor!(IDENT, self.cursor, x, Parameter);
                        self.type_symbols(t)
                    }

                    if fun.macro_.is_some() {
                        // we currently do not process macro function bodies
                        // in the parsing symbolicator (and do very limited
                        // processing in typing symbolicator)
                        continue;
                    }
                    if let P::FunctionBody_::Defined(seq) = &fun.body.value {
                        self.seq_symbols(seq);
                    };
                }
                MM::Struct(sdef) => {
                    if latest_use_loc.end() > 0 {
                        non_use_member_after_use = true;
                    }
                    earliest_member_loc =
                        earliest_loc(earliest_member_loc, sdef.doc.loc().unwrap_or(sdef.loc));
                    // If the cursor is in this item, mark that down.
                    // This may be overridden by the recursion below.
                    if let Some(cursor) = &mut self.cursor {
                        if sdef.name.loc().contains(&cursor.loc) {
                            cursor.position = CursorPosition::DefName;
                            debug_assert!(cursor.defn_name.is_none());
                            cursor.defn_name = Some(CursorDefinition::Struct(sdef.name));
                        } else if sdef.loc.contains(&cursor.loc) {
                            cursor.defn_name = Some(CursorDefinition::Struct(sdef.name));
                        }
                    };

                    sdef.attributes.iter().for_each(|sp!(_, attrs)| {
                        attrs.0.iter().for_each(|a| self.attr_symbols(a))
                    });

                    match &sdef.fields {
                        P::StructFields::Named(v) => v.iter().for_each(|(_, x, t)| {
                            self.field_defn(x);
                            self.type_symbols(t)
                        }),
                        P::StructFields::Positional(v) => {
                            v.iter().for_each(|(_, t)| self.type_symbols(t))
                        }
                        P::StructFields::Native(_) => (),
                    }
                }
                MM::Enum(edef) => {
                    if latest_use_loc.end() > 0 {
                        non_use_member_after_use = true;
                    }
                    earliest_member_loc =
                        earliest_loc(earliest_member_loc, edef.doc.loc().unwrap_or(edef.loc));
                    // If the cursor is in this item, mark that down.
                    // This may be overridden by the recursion below.
                    if let Some(cursor) = &mut self.cursor {
                        if edef.name.loc().contains(&cursor.loc) {
                            cursor.position = CursorPosition::DefName;
                            debug_assert!(cursor.defn_name.is_none());
                            cursor.defn_name = Some(CursorDefinition::Enum(edef.name));
                        } else if edef.loc.contains(&cursor.loc) {
                            cursor.defn_name = Some(CursorDefinition::Enum(edef.name));
                        }
                    };

                    edef.attributes.iter().for_each(|sp!(_, attrs)| {
                        attrs.0.iter().for_each(|a| self.attr_symbols(a))
                    });

                    let P::EnumDefinition { variants, .. } = edef;
                    for variant in variants {
                        let P::VariantDefinition { fields, .. } = variant;
                        match fields {
                            P::VariantFields::Named(v) => v.iter().for_each(|(_, x, t)| {
                                self.field_defn(x);
                                self.type_symbols(t)
                            }),
                            P::VariantFields::Positional(v) => {
                                v.iter().for_each(|(_, t)| self.type_symbols(t))
                            }
                            P::VariantFields::Empty => (),
                        }
                    }
                }
                MM::Use(use_decl) => {
                    if !non_use_member_after_use {
                        // update only if we have not seen a use that was already
                        // followed by a non-use member
                        latest_use_loc = latest_loc(latest_use_loc, use_decl.loc);
                    }
                    self.use_decl_symbols(use_decl)
                }
                MM::Friend(fdecl) => {
                    if latest_use_loc.end() > 0 {
                        non_use_member_after_use = true;
                    }
                    earliest_member_loc = earliest_loc(earliest_member_loc, fdecl.loc);
                    self.chain_symbols(&fdecl.friend)
                }
                MM::Constant(c) => {
                    if latest_use_loc.end() > 0 {
                        non_use_member_after_use = true;
                    }
                    earliest_member_loc =
                        earliest_loc(earliest_member_loc, c.doc.loc().unwrap_or(c.loc));
                    // If the cursor is in this item, mark that down.
                    // This may be overridden by the recursion below.
                    if let Some(cursor) = &mut self.cursor {
                        if c.name.loc().contains(&cursor.loc) {
                            cursor.position = CursorPosition::DefName;
                            debug_assert!(cursor.defn_name.is_none());
                            cursor.defn_name = Some(CursorDefinition::Constant(c.name));
                        } else if c.loc.contains(&cursor.loc) {
                            cursor.defn_name = Some(CursorDefinition::Constant(c.name));
                        }
                    };

                    c.attributes.iter().for_each(|sp!(_, attrs)| {
                        attrs.0.iter().for_each(|a| self.attr_symbols(a))
                    });

                    self.type_symbols(&c.signature);
                    self.exp_symbols(&c.value);
                }
                MM::Spec(s) => {
                    if latest_use_loc.end() > 0 {
                        non_use_member_after_use = true;
                    }
                    earliest_member_loc = earliest_loc(earliest_member_loc, s.loc);
                }
            }
        }
        self.add_import_insert_info(latest_use_loc, earliest_member_loc);

        self.current_mod_ident_str = None;
        let processed_defs = std::mem::replace(&mut self.use_defs, old_defs);
        mod_use_defs.insert(mod_ident_str.clone(), processed_defs);
        let processed_alias_lengths = std::mem::replace(&mut self.alias_lengths, old_alias_lengths);
        mod_to_alias_lengths.insert(mod_ident_str, processed_alias_lengths);
    }

    fn add_import_insert_info(&mut self, latest_use_loc: Loc, earliest_member_loc: Loc) {
        let Some(mod_defs) = self
            .mod_outer_defs
            .get_mut(&self.current_mod_ident_str.clone().unwrap())
        else {
            return;
        };
        mod_defs.import_insert_info = if latest_use_loc.end() > 0 {
            // imports exist, auto-imports position is at the end of the last
            // auto import
            if let Some(use_start) = loc_start_to_lsp_position_opt(self.files, &latest_use_loc) {
                loc_end_to_lsp_position_opt(self.files, &latest_use_loc).map(|pos| {
                    AutoImportInsertionInfo {
                        kind: AutoImportInsertionKind::AfterLastImport,
                        pos,
                        tabulation: use_start.character as usize,
                    }
                })
            } else {
                None
            }
        } else if earliest_member_loc.start() < u32::MAX {
            // Imports don't exist, but some members do, and auto-imports position
            // is at the beginning of the line where the first member starts, with
            // the intention to insert auto-imports on the same line pushing
            // the member down. The reason why we don't insert auto-imports
            // on the previous line is that if we are unlucky, we may
            // hit a curly starting module or module declaration itself.
            loc_start_to_lsp_position_opt(self.files, &earliest_member_loc).map(|pos| {
                AutoImportInsertionInfo {
                    kind: AutoImportInsertionKind::BeforeFirstMember,
                    pos,
                    tabulation: pos.character as usize,
                }
            })
        } else {
            // Otherwise it's unclear where to insert auto-imports.
            // We could speculate here but this should not really happen
            // since for auto-import to make sense we need some code
            // in the module.
            None
        }
    }

    /// Get symbols for a sequence item
    fn seq_item_symbols(&mut self, seq_item: &P::SequenceItem) {
        use P::SequenceItem_ as I;

        // If the cursor is in this item, mark that down.
        // This may be overridden by the recursion below.
        update_cursor!(self.cursor, seq_item, SeqItem);

        match &seq_item.value {
            I::Seq(e) => self.exp_symbols(e),
            I::Declare(v, to) => {
                v.value
                    .iter()
                    .for_each(|bind| self.bind_symbols(bind, to.is_some()));
                if let Some(t) = to {
                    self.type_symbols(t);
                }
            }
            I::Bind(v, to, e) => {
                v.value
                    .iter()
                    .for_each(|bind| self.bind_symbols(bind, to.is_some()));
                if let Some(t) = to {
                    self.type_symbols(t);
                }
                self.exp_symbols(e);
            }
        }
    }

    fn path_entry_symbols(&mut self, path: &P::PathEntry) {
        let P::PathEntry {
            name: _,
            tyargs,
            is_macro: _,
        } = path;
        if let Some(sp!(_, tyargs)) = tyargs {
            tyargs.iter().for_each(|t| self.type_symbols(t));
        }
    }

    fn root_path_entry_symbols(&mut self, path: &P::RootPathEntry) {
        let P::RootPathEntry {
            name: _,
            tyargs,
            is_macro: _,
        } = path;
        if let Some(sp!(_, tyargs)) = tyargs {
            tyargs.iter().for_each(|t| self.type_symbols(t));
        }
    }

    /// Get symbols for an expression
    fn exp_symbols(&mut self, exp: &P::Exp) {
        use P::Exp_ as E;
        fn last_chain_symbol_loc(sp!(_, chain): &P::NameAccessChain) -> Loc {
            use P::NameAccessChain_ as NA;
            match chain {
                NA::Single(entry) => entry.name.loc,
                NA::Path(path) => {
                    if path.entries.is_empty() {
                        path.root.name.loc
                    } else {
                        path.entries.last().unwrap().name.loc
                    }
                }
            }
        }

        // If the cursor is in this item, mark that down.
        // This may be overridden by the recursion below.
        update_cursor!(self.cursor, exp, Exp);

        match &exp.value {
            E::Move(_, e) => self.exp_symbols(e),
            E::Copy(_, e) => self.exp_symbols(e),
            E::Name(chain) => self.chain_symbols(chain),
            E::Call(chain, v) => {
                self.chain_symbols(chain);
                v.value.iter().for_each(|e| self.exp_symbols(e));
                assert!(self.current_mod_ident_str.is_some());
                if let Some(mod_defs) = self
                    .mod_outer_defs
                    .get_mut(&self.current_mod_ident_str.clone().unwrap())
                {
                    mod_defs.call_infos.insert(
                        last_chain_symbol_loc(chain),
                        CallInfo::new(/* do_call */ false, &v.value),
                    );
                };
            }
            E::Pack(chain, v) => {
                self.chain_symbols(chain);
                v.iter().for_each(|(_, e)| self.exp_symbols(e));
            }
            E::Vector(_, vo, v) => {
                if let Some(v) = vo {
                    v.iter().for_each(|t| self.type_symbols(t));
                }
                v.value.iter().for_each(|e| self.exp_symbols(e));
            }
            E::IfElse(e1, e2, oe) => {
                self.exp_symbols(e1);
                self.exp_symbols(e2);
                if let Some(e) = oe.as_ref() {
                    self.exp_symbols(e)
                }
            }
            E::Match(e, sp!(_, v)) => {
                self.exp_symbols(e);
                v.iter().for_each(|sp!(_, arm)| {
                    self.match_pattern_symbols(&arm.pattern);
                    if let Some(g) = &arm.guard {
                        self.exp_symbols(g);
                    }
                    self.exp_symbols(&arm.rhs);
                })
            }
            E::While(e1, e2) => {
                self.exp_symbols(e1);
                self.exp_symbols(e2);
            }
            E::Loop(e) => self.exp_symbols(e),
            E::Labeled(_, e) => self.exp_symbols(e),
            E::Block(seq) => self.seq_symbols(seq),
            E::Lambda(sp!(_, bindings), to, e) => {
                for (sp!(_, v), bto) in bindings {
                    if let Some(bt) = bto {
                        self.type_symbols(bt);
                    }
                    v.iter()
                        .for_each(|bind| self.bind_symbols(bind, to.is_some()));
                }
                if let Some(t) = to {
                    self.type_symbols(t);
                }
                self.exp_symbols(e);
            }
            E::ExpList(l) => l.iter().for_each(|e| self.exp_symbols(e)),
            E::Parens(e) => self.exp_symbols(e),
            E::Assign(e1, e2) => {
                self.exp_symbols(e1);
                self.exp_symbols(e2);
            }
            E::Abort(oe) => {
                if let Some(e) = oe.as_ref() {
                    self.exp_symbols(e)
                }
            }
            E::Return(_, oe) => {
                if let Some(e) = oe.as_ref() {
                    self.exp_symbols(e)
                }
            }
            E::Break(_, oe) => {
                if let Some(e) = oe.as_ref() {
                    self.exp_symbols(e)
                }
            }
            E::Dereference(e) => self.exp_symbols(e),
            E::UnaryExp(_, e) => self.exp_symbols(e),
            E::BinopExp(e1, _, e2) => {
                self.exp_symbols(e1);
                self.exp_symbols(e2);
            }
            E::Borrow(_, e) => self.exp_symbols(e),
            E::Dot(e, _, _) => self.exp_symbols(e),
            E::DotCall(e, _, name, _, vo, v) => {
                self.exp_symbols(e);
                if let Some(v) = vo {
                    v.iter().for_each(|t| self.type_symbols(t));
                }
                v.value.iter().for_each(|e| self.exp_symbols(e));
                assert!(self.current_mod_ident_str.is_some());
                if let Some(mod_defs) = self
                    .mod_outer_defs
                    .get_mut(&self.current_mod_ident_str.clone().unwrap())
                {
                    mod_defs
                        .call_infos
                        .insert(name.loc, CallInfo::new(/* do_call */ true, &v.value));
                };
            }
            E::Index(e, v) => {
                self.exp_symbols(e);
                v.value.iter().for_each(|e| self.exp_symbols(e));
            }
            E::Cast(e, t) => {
                self.exp_symbols(e);
                self.type_symbols(t);
            }
            E::Annotate(e, t) => {
                self.exp_symbols(e);
                self.type_symbols(t);
            }
            E::DotUnresolved(_, e) => self.exp_symbols(e),
            E::Value(_)
            | E::Quant(..)
            | E::Unit
            | E::Continue(_)
            | E::Spec(_)
            | E::UnresolvedError => (),
        }
    }

    fn match_pattern_symbols(&mut self, pattern: &P::MatchPattern) {
        use P::MatchPattern_ as MP;
        // If the cursor is in this match pattern, mark that down.
        // This may be overridden by the recursion below.
        update_cursor!(self.cursor, pattern, MatchPattern);

        match &pattern.value {
            MP::PositionalConstructor(chain, sp!(_, v)) => {
                self.chain_symbols(chain);
                v.iter().for_each(|e| {
                    if let P::Ellipsis::Binder(m) = e {
                        self.match_pattern_symbols(m);
                    }
                })
            }
            MP::FieldConstructor(chain, sp!(_, v)) => {
                self.chain_symbols(chain);
                v.iter().for_each(|e| {
                    if let P::Ellipsis::Binder((_, m)) = e {
                        self.match_pattern_symbols(m);
                    }
                })
            }
            MP::Name(_, chain) => {
                self.chain_symbols(chain);
                assert!(self.current_mod_ident_str.is_some());
                if let Some(mod_defs) = self
                    .mod_outer_defs
                    .get_mut(&self.current_mod_ident_str.clone().unwrap())
                {
                    mod_defs.untyped_defs.insert(chain.loc);
                };
            }
            MP::Or(m1, m2) => {
                self.match_pattern_symbols(m2);
                self.match_pattern_symbols(m1);
            }
            MP::At(_, m) => self.match_pattern_symbols(m),
            MP::Literal(_) => (),
        }
    }

    /// Get symbols for a sequence
    fn seq_symbols(&mut self, (use_decls, seq_items, _, oe): &P::Sequence) {
        use_decls
            .iter()
            .for_each(|use_decl| self.use_decl_symbols(use_decl));

        seq_items
            .iter()
            .for_each(|seq_item| self.seq_item_symbols(seq_item));
        if let Some(e) = oe.as_ref().as_ref() {
            self.exp_symbols(e)
        }
    }

    /// Get symbols for a use declaration
    fn use_decl_symbols(&mut self, use_decl: &P::UseDecl) {
        use_decl
            .attributes
            .iter()
            .for_each(|sp!(_, attrs)| attrs.0.iter().for_each(|a| self.attr_symbols(a)));

        update_cursor!(self.cursor, sp(use_decl.loc, use_decl.use_.clone()), Use);

        match &use_decl.use_ {
            P::Use::ModuleUse(mod_ident, mod_use) => {
                let mod_ident_str =
                    parsing_mod_ident_to_map_key(self.pkg_addresses, &mod_ident.value);
                self.mod_name_symbol(&mod_ident.value.module, &mod_ident_str);
                self.mod_use_symbols(mod_use, &mod_ident_str);
            }
            P::Use::NestedModuleUses(leading_name, uses) => {
                for (mod_name, mod_use) in uses {
                    let mod_ident_str = parsing_leading_and_mod_names_to_map_key(
                        self.pkg_addresses,
                        *leading_name,
                        *mod_name,
                    );

                    self.mod_name_symbol(mod_name, &mod_ident_str);
                    self.mod_use_symbols(mod_use, &mod_ident_str);
                }
            }
            P::Use::Fun {
                visibility: _,
                function,
                ty,
                method: _,
            } => {
                self.chain_symbols(function);
                self.chain_symbols(ty);
            }
            P::Use::Partial { .. } => (),
        }
    }

    /// Get module name symbol
    fn mod_name_symbol(&mut self, mod_name: &P::ModuleName, mod_ident_str: &String) {
        let Some(mod_defs) = self.mod_outer_defs.get_mut(mod_ident_str) else {
            return;
        };
        let Some(mod_name_start) = loc_start_to_lsp_position_opt(self.files, &mod_name.loc())
        else {
            debug_assert!(false);
            return;
        };
        self.use_defs.insert(
            mod_name_start.line,
            UseDef::new(
                self.references,
                &BTreeMap::new(),
                mod_name.loc().file_hash(),
                mod_name_start,
                mod_defs.name_loc,
                &mod_name.value(),
                None,
            ),
        );
    }

    /// Get symbols for a module use
    fn mod_use_symbols(&mut self, mod_use: &P::ModuleUse, mod_ident_str: &String) {
        match mod_use {
            P::ModuleUse::Module(Some(alias_name)) => {
                self.mod_name_symbol(alias_name, mod_ident_str);
            }
            P::ModuleUse::Module(None) => (), // nothing more to do
            P::ModuleUse::Members(v) => {
                for (name, alias_opt) in v {
                    self.use_decl_member_symbols(mod_ident_str.clone(), name, alias_opt);
                }
            }
            P::ModuleUse::Partial { .. } => (),
        }
    }

    /// Get symbols for a module member in the use declaration (can be a struct or a function)
    fn use_decl_member_symbols(
        &mut self,
        mod_ident_str: String,
        name: &Name,
        alias_opt: &Option<Name>,
    ) {
        let Some(mod_defs) = self.mod_outer_defs.get(&mod_ident_str) else {
            return;
        };
        if let Some(mut ud) = add_member_use_def(
            &name.value,
            self.files,
            mod_defs,
            &name.value,
            &name.loc,
            self.references,
            self.def_info,
            &mut self.use_defs,
            &BTreeMap::new(),
        ) {
            // it's a struct - add it for the alias as well
            if let Some(alias) = alias_opt {
                let Some(alias_start) = loc_start_to_lsp_position_opt(self.files, &alias.loc)
                else {
                    debug_assert!(false);
                    return;
                };
                ud.rename_use(
                    self.references,
                    alias.value,
                    alias_start,
                    alias.loc.file_hash(),
                );
                self.use_defs.insert(alias_start.line, ud);
            }
            return;
        }
        if let Some(mut ud) = add_member_use_def(
            &name.value,
            self.files,
            mod_defs,
            &name.value,
            &name.loc,
            self.references,
            self.def_info,
            &mut self.use_defs,
            &BTreeMap::new(),
        ) {
            // it's a function - add it for the alias as well
            if let Some(alias) = alias_opt {
                let Some(alias_start) = loc_start_to_lsp_position_opt(self.files, &alias.loc)
                else {
                    debug_assert!(false);
                    return;
                };
                ud.rename_use(
                    self.references,
                    alias.value,
                    alias_start,
                    alias.loc.file_hash(),
                );
                self.use_defs.insert(alias_start.line, ud);
            }
        }
    }

    /// Get symbols for a type
    fn type_symbols(&mut self, type_: &P::Type) {
        use P::Type_ as T;

        // If the cursor is in this item, mark that down.
        // This may be overridden by the recursion below.
        update_cursor!(self.cursor, type_, Type);

        match &type_.value {
            T::Apply(chain) => {
                self.chain_symbols(chain);
            }
            T::Ref(_, t) => self.type_symbols(t),
            T::Fun(v, t) => {
                v.iter().for_each(|t| self.type_symbols(t));
                self.type_symbols(t);
            }
            T::Multiple(v) => v.iter().for_each(|t| self.type_symbols(t)),
            T::Unit => (),
            T::UnresolvedError => (),
        }
    }

    /// Get symbols for a bind statement
    fn bind_symbols(&mut self, bind: &P::Bind, explicitly_typed: bool) {
        use P::Bind_ as B;

        // If the cursor is in this item, mark that down.
        // This may be overridden by the recursion below.
        update_cursor!(self.cursor, bind, Binding);

        match &bind.value {
            B::Unpack(chain, bindings) => {
                self.chain_symbols(chain);
                match bindings {
                    P::FieldBindings::Named(v) => {
                        for symbol in v {
                            match symbol {
                                P::Ellipsis::Binder((_, x)) => self.bind_symbols(x, false),
                                P::Ellipsis::Ellipsis(_) => (),
                            }
                        }
                    }
                    P::FieldBindings::Positional(v) => {
                        for symbol in v.iter() {
                            match symbol {
                                P::Ellipsis::Binder(x) => self.bind_symbols(x, false),
                                P::Ellipsis::Ellipsis(_) => (),
                            }
                        }
                    }
                }
            }
            B::Var(_, var) => {
                if !explicitly_typed {
                    assert!(self.current_mod_ident_str.is_some());
                    if let Some(mod_defs) = self
                        .mod_outer_defs
                        .get_mut(&self.current_mod_ident_str.clone().unwrap())
                    {
                        mod_defs.untyped_defs.insert(var.loc());
                    };
                }
            }
        }
    }

    /// Get symbols for a name access chain
    fn chain_symbols(&mut self, sp!(_, chain): &P::NameAccessChain) {
        use P::NameAccessChain_ as NA;
        // Record the length of all identifiers representing a potentially
        // aliased module, struct, enum or function name in an access chain.
        // We can conservatively record all identifiers as they are only
        // accessed by-location so those irrelevant will never be queried.
        match chain {
            NA::Single(entry) => {
                self.path_entry_symbols(entry);
                if let Some(loc) = loc_start_to_lsp_position_opt(self.files, &entry.name.loc) {
                    self.alias_lengths.insert(loc, entry.name.value.len());
                };
            }
            NA::Path(path) => {
                let P::NamePath {
                    root,
                    entries,
                    is_incomplete: _,
                } = path;
                self.root_path_entry_symbols(root);
                if let Some(root_loc) = loc_start_to_lsp_position_opt(self.files, &root.name.loc) {
                    if let P::LeadingNameAccess_::Name(n) = root.name.value {
                        self.alias_lengths.insert(root_loc, n.value.len());
                    }
                };
                entries.iter().for_each(|entry| {
                    self.path_entry_symbols(entry);
                    if let Some(loc) = loc_start_to_lsp_position_opt(self.files, &entry.name.loc) {
                        self.alias_lengths.insert(loc, entry.name.value.len());
                    };
                });
            }
        };
    }

    fn field_defn(&mut self, field: &P::Field) {
        // If the cursor is in this item, mark that down.
        update_cursor!(IDENT, self.cursor, field, FieldDefn);
    }
}

/// Produces module ident string of the form pkg::module to be used as a map key.
/// It's important that these are consistent between parsing AST and typed AST.
pub fn parsing_mod_def_to_map_key(
    pkg_addresses: &NamedAddressMap,
    mod_def: &P::ModuleDefinition,
) -> Option<String> {
    // we assume that modules are declared using the PkgName::ModName pattern (which seems to be the
    // standard practice) and while Move allows other ways of defining modules (i.e., with address
    // preceding a sequence of modules), this method is now deprecated.
    //
    // TODO: make this function simply return String when the other way of defining modules is
    // removed
    mod_def
        .address
        .map(|a| parsing_leading_and_mod_names_to_map_key(pkg_addresses, a, mod_def.name))
}

/// Produces module ident string of the form pkg::module to be used as a map key.
/// It's important that these are consistent between parsing AST and typed AST.
fn parsing_leading_and_mod_names_to_map_key(
    pkg_addresses: &NamedAddressMap,
    ln: P::LeadingNameAccess,
    name: P::ModuleName,
) -> String {
    let parsed_addr = parsed_address(ln, pkg_addresses);
    format!("{}::{}", parsed_addr, name).to_string()
}

/// Produces module ident string of the form pkg::module to be used as a map key.
/// It's important that these are consistent between parsing AST and typed AST,
fn parsing_mod_ident_to_map_key(
    pkg_addresses: &NamedAddressMap,
    mod_ident: &P::ModuleIdent_,
) -> String {
    format!(
        "{}::{}",
        parsed_address(mod_ident.address, pkg_addresses),
        mod_ident.module
    )
    .to_string()
}
