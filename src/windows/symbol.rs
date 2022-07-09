// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use hashbrown::{hash_map, HashMap};
use log::warn;
use pdb::{
    AddressMap, FrameTable, PdbInternalRva, PdbInternalSectionOffset, ProcedureSymbol,
    PublicSymbol, RegisterRelativeSymbol, TypeIndex,
};
use pdb_addr2line::pdb;
use pdb_addr2line::TypeFormatter;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::rc::Rc;
use symbolic::common::{Language, Name, NameMangling};
use symbolic::debuginfo::pe::{ExceptionData, PeSymbolIterator};
use symbolic::demangle::Demangle;

use super::pdb::{PDBContributions, PDBSections};
use super::source::SourceLineCollector;
use crate::common;
use crate::common::demangle_options;
use crate::common::LineFinalizer;
use crate::line::Lines;

pub enum FuncName {
    // The undecorated name even in case of failure
    // (there is a bug somewhere else but the name should be undecorated)
    Undecorated(String),
    // The name hasn't been undecorated because the language is unknown
    Unknown((String, u32)),
}

impl FuncName {
    pub fn get_unknown(name: String) -> Self {
        // https://docs.microsoft.com/en-us/cpp/build/reference/decorated-names?view=vs-2019
        // __cdecl Leading underscore (_)
        // __stdcall Leading underscore (_) and a trailing at sign (@) followed by the number of bytes in the parameter list in decimal
        // __fastcall Leading and trailing at signs (@) followed by a decimal number representing the number of bytes in the parameter list

        if name.is_empty() {
            return FuncName::Unknown((name, 0));
        }

        let (first, sub) = name.split_at(1);

        if (first != "_" && first != "@") || sub.find(|c: char| c == ':' || c == '(').is_some() {
            return FuncName::Unknown((name, 0));
        }

        let parts: Vec<_> = sub.rsplitn(2, '@').collect();
        if parts.len() <= 1 {
            let name = if first == "_" { sub.to_string() } else { name };
            return FuncName::Unknown((name, 0));
        }

        if let Ok(stack_param_size) = parts[0].parse::<u32>() {
            let sps = if first == "@" {
                // __fastcall: the two first args are put in ECX and EDX
                if stack_param_size > 8 {
                    stack_param_size - 8
                } else {
                    0
                }
            } else {
                stack_param_size
            };
            return FuncName::Unknown((parts[1].to_string(), sps));
        }

        let name = if first == "_" { sub.to_string() } else { name };

        FuncName::Unknown((name, 0))
    }
}

pub(super) struct BlockInfo {
    pub rva: u32,
    pub offset: PdbInternalSectionOffset,
    pub len: u32,
}

pub(super) type PDBSymbols = BTreeMap<u32, PDBSymbol>;

#[derive(Clone, Debug)]
pub(super) struct EBPInfo {
    type_index: TypeIndex,
    offset: u32,
}

#[derive(Debug)]
pub(super) struct SelectedSymbol {
    pub name: String,
    pub type_index: TypeIndex,
    pub module_index: Option<usize>,
    pub is_public: bool,
    pub is_multiple: bool,
    pub offset: PdbInternalSectionOffset,
    pub sym_offset: Option<PdbInternalSectionOffset>,
    pub len: u32,
    pub parameter_size: u32,
    pub source: Lines,
    pub ebp: Vec<EBPInfo>,
    pub id: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct PDBSymbol {
    pub name: String,
    pub is_public: bool,
    pub is_multiple: bool,
    pub rva: u32,
    pub len: u32,
    pub parameter_size: u32,
    pub source: Rc<Lines>,
    pub id: usize,
}

// it's safe because source (with Rc) isn't shared: it's just an internal thing
unsafe impl Send for PDBSymbol {}

impl PDBSymbol {
    fn get_from(&self, rva: u32, len: u32) -> PDBSymbol {
        PDBSymbol {
            name: self.name.clone(),
            is_public: self.is_public,
            is_multiple: self.is_multiple,
            rva,
            len,
            parameter_size: self.parameter_size,
            source: if let Some(source) = self.source.retain(rva, len) {
                Rc::new(source)
            } else {
                Rc::clone(&self.source)
            },
            id: self.id,
        }
    }
}

impl Display for PDBSymbol {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        if self.is_public {
            writeln!(
                f,
                "PUBLIC {}{:x} {:x} {}",
                if self.is_multiple { "m " } else { "" },
                self.rva,
                self.parameter_size,
                self.name,
            )?;
        } else {
            writeln!(
                f,
                "FUNC {}{:x} {:x} {:x} {}",
                if self.is_multiple { "m " } else { "" },
                self.rva,
                self.len,
                self.parameter_size,
                self.name,
            )?;

            write!(f, "{}", self.source)?;
        }

        Ok(())
    }
}

impl SelectedSymbol {
    fn get_und(&self, formatter: &TypeFormatter) -> FuncName {
        if self.name.is_empty() {
            FuncName::Undecorated("<name omitted>".to_string())
        } else if self.type_index == TypeIndex(0) {
            demangle(&self.name)
        } else {
            match formatter.format_function(
                &self.name,
                self.module_index.unwrap_or(0),
                self.type_index,
            ) {
                Ok(function) => FuncName::Undecorated(function),
                Err(_) => FuncName::get_unknown(self.name.clone()),
            }
        }
    }

    fn get_stack_param_size(
        &mut self,
        formatter: &TypeFormatter,
        _address_map: &AddressMap,
        _frame_table: &FrameTable,
    ) -> u32 {
        // TODO: check if this value is the correct one
        // For now (legacy) return 0
        /*if frame_table.is_empty() {
            return 0;
        }

        let internal_rva = self.offset.to_internal_rva(address_map).unwrap();
        let mut frames = frame_table.iter_at_rva(internal_rva);
        if let Ok(frame) = frames.next() {
            if let Some(frame) =  frame {
                return frame.params_size;
            }
        }*/

        if self.ebp.is_empty() {
            return self.parameter_size;
        }

        let module_index = self.module_index.unwrap_or(0);
        let (min_start, max_end) = self.ebp.drain(..).fold((std::u32::MAX, 0), |acc, i| {
            (
                acc.0.min(i.offset),
                acc.1
                    .max(i.offset + formatter.get_type_size(module_index, i.type_index) as u32),
            )
        });

        let min_start = min_start.max(4);
        let sps = if min_start < max_end {
            // round max_end to next multiple of 4 (if not)
            let max_end = (max_end + 3) & !3;
            max_end - min_start
        } else {
            0
        };

        self.parameter_size = sps;

        sps
    }

    pub(super) fn update_public(&mut self, symbol: PublicSymbol) {
        if self.is_public {
            self.is_multiple = true;
            let sym_name = symbol.name.to_string().into_owned();
            if sym_name < self.name {
                self.name = sym_name;
                self.offset = symbol.offset;
            }
        } else {
            let fun_name = symbol.name.to_string().into_owned();
            if self.type_index == TypeIndex(0x0) {
                // The symbol doesn't have any type info so use the (mangled) name from the public
                if let Some(sym_offset) = self.sym_offset {
                    if sym_offset == symbol.offset {
                        self.name = fun_name.clone();
                    }
                }
            }
            // The public name may contain parameter_size info so get it
            if let FuncName::Unknown((name, sps)) = FuncName::get_unknown(fun_name.clone()) {
                if name == self.name || fun_name == self.name {
                    self.parameter_size = sps;
                }
            }
        }
    }

    pub(super) fn mv_to_pdb_symbol(
        mut self,
        formatter: &TypeFormatter,
        rva: u32,
        address_map: &AddressMap,
        frame_table: &FrameTable,
    ) -> (PDBSymbol, PdbInternalSectionOffset) {
        let name = self.get_und(formatter);
        let (name, stack_param_size) = match name {
            FuncName::Undecorated(name) => (
                name,
                self.get_stack_param_size(formatter, address_map, frame_table),
            ),
            FuncName::Unknown((name, sps)) => (name, sps),
        };

        self.source.finalize(rva, self.len, address_map);

        (
            PDBSymbol {
                name,
                is_public: self.is_public,
                is_multiple: self.is_multiple,
                rva: 0,
                len: self.len,
                parameter_size: stack_param_size,
                source: Rc::new(self.source),
                id: self.id,
            },
            self.offset,
        )
    }
}

#[derive(Default)]
pub(super) struct RvaSymbols {
    map: HashMap<u32, SelectedSymbol>,
    rva: u32,
    symbol: Option<SelectedSymbol>,
    last_id: usize,
}

impl RvaSymbols {
    pub(super) fn get_symbol_at(&self, rva: u32) -> Option<&SelectedSymbol> {
        self.map.get(&rva)
    }

    pub(super) fn add_procedure_symbol(
        &mut self,
        line_collector: &SourceLineCollector,
        function: ProcedureSymbol,
        block_info: BlockInfo,
        module_index: usize,
    ) {
        // Since several symbols may have the same rva (because they've the same disassembly code)
        // we need to "select" the a symbol for a rva.
        // Anyway it could lead to strange backtraces.

        let fun_name = function.name.to_string().into_owned();
        if let Some(selected) = self.map.get_mut(&block_info.rva) {
            selected.is_multiple = true;
        } else {
            let source = line_collector.collect_source_lines(block_info.offset, block_info.len);
            self.rva = block_info.rva;
            self.symbol = Some(SelectedSymbol {
                name: fun_name,
                type_index: function.type_index,
                module_index: Some(module_index),
                is_public: false,
                is_multiple: false,
                offset: block_info.offset,
                sym_offset: Some(function.offset),
                len: block_info.len,
                parameter_size: 0,
                source,
                ebp: Vec::new(),
                id: self.last_id,
            });
            self.last_id += 1;
        }
    }

    fn is_constant_string(name: &str) -> bool {
        name.starts_with("??_C")
    }

    fn is_constant_number(name: &str) -> bool {
        if let Some(name) = name.strip_prefix("__") {
            name.starts_with("real@") || name.starts_with("xmm@") || name.starts_with("ymm@")
        } else {
            false
        }
    }

    fn filter_public(name: &str) -> bool {
        Self::is_constant_string(name) || Self::is_constant_number(name)
    }

    pub(super) fn add_public_symbol(
        &mut self,
        symbol: PublicSymbol,
        pdb_sections: &PDBSections,
        pdb_contributions: &PDBContributions,
        address_map: &AddressMap,
    ) {
        let rva = match symbol.offset.to_rva(address_map) {
            Some(rva) => rva,
            _ => return,
        };

        if symbol.code
            || symbol.function
            || (pdb_sections.is_code(symbol.offset.section)
                && pdb_contributions.is_code(symbol.offset.section, symbol.offset.offset))
        {
            let sym_name = symbol.name.to_string().into_owned();

            // For any reasons we can have public symbols which are in executable section and are constants (strings, numbers, ...).
            // It's the case in ntdll.dll
            if Self::filter_public(&sym_name) {
                return;
            }

            match self.map.entry(rva.0) {
                hash_map::Entry::Occupied(selected) => {
                    let selected = selected.into_mut();
                    selected.update_public(symbol);
                }
                hash_map::Entry::Vacant(e) => {
                    let offset = symbol.offset;
                    e.insert(SelectedSymbol {
                        name: sym_name,
                        type_index: TypeIndex(0),
                        module_index: None,
                        is_public: true,
                        is_multiple: false,
                        offset,
                        sym_offset: None,
                        len: 0,
                        parameter_size: 0,
                        source: Lines::new(),
                        ebp: Vec::new(),
                        id: self.last_id,
                    });
                    self.last_id += 1;
                }
            }
        }
    }

    pub(super) fn add_symbol(&mut self, function: SelectedSymbol, block_info: BlockInfo) {
        match self.map.entry(block_info.rva) {
            hash_map::Entry::Occupied(selected) => {
                let selected = selected.into_mut();
                selected.is_multiple = true;
            }
            hash_map::Entry::Vacant(e) => {
                e.insert(function);
            }
        }
    }

    pub(super) fn add_ebp(&mut self, ebp: RegisterRelativeSymbol) {
        if let Some(symbol) = self.symbol.as_mut() {
            symbol.ebp.push(EBPInfo {
                type_index: ebp.type_index,
                offset: ebp.offset as u32,
            });
        }
    }

    pub(super) fn close_procedure(&mut self) {
        if let Some(symbol) = self.symbol.take() {
            self.map.insert(self.rva, symbol);
        }
    }

    fn split_and_collect(
        mut self,
        formatter: TypeFormatter,
        address_map: &AddressMap,
        frame_table: FrameTable,
    ) -> (Vec<PDBSymbol>, BTreeMap<(u32, u32), usize>) {
        // The value in ranges is the index in all_syms
        let mut ranges: BTreeMap<(u32, u32), usize> = BTreeMap::default();
        let mut all_syms = Vec::with_capacity(self.map.len());

        for (rva, sym) in self.map.drain() {
            let (sym, offset) = sym.mv_to_pdb_symbol(&formatter, rva, address_map, &frame_table);
            let last = all_syms.len();
            if sym.len == 0 {
                ranges.insert((rva, 0), last);
            } else {
                let start = offset.to_internal_rva(address_map).unwrap();
                let end = PdbInternalRva(start.0 + sym.len);
                for (rva, len) in address_map
                    .rva_ranges(start..end)
                    .map(|r| (r.start.0, r.end.0 - r.start.0))
                {
                    ranges.insert((rva, len), last);
                }
            }
            all_syms.push(sym);
        }

        (all_syms, ranges)
    }

    fn fill_the_gaps(all_syms: Vec<PDBSymbol>, ranges: BTreeMap<(u32, u32), usize>) -> PDBSymbols {
        let mut syms = PDBSymbols::default();

        // We initialize for first symbol
        let mut iterator = ranges.iter();
        let ((rva, len), sym_pos) = iterator.next().unwrap();

        let mut last_rva = *rva;
        let mut last_len = *len;
        let mut last_sym = &all_syms[*sym_pos];
        let mut last_id = last_sym.id;

        // We merge ranges ([a; b] + [c; d] = [a; d]) which consecutively have the same function id
        // So the hole between [a; b] and [c; d] will become a part of the range for the function
        for ((rva, len), sym_pos) in iterator {
            let sym = &all_syms[*sym_pos];
            if last_id == sym.id {
                last_len = rva - last_rva + len;
            } else {
                syms.insert(last_rva, last_sym.get_from(last_rva, last_len));
                last_sym = sym;
                last_id = sym.id;
                last_rva = *rva;
                last_len = *len;
            }
        }

        syms.insert(last_rva, last_sym.get_from(last_rva, last_len));

        syms
    }

    pub(super) fn mv_to_pdb_symbols(
        self,
        formatter: TypeFormatter,
        address_map: &AddressMap,
        frame_table: FrameTable,
    ) -> PDBSymbols {
        if self.map.is_empty() {
            return PDBSymbols::default();
        }

        let (all_syms, ranges) = self.split_and_collect(formatter, address_map, frame_table);
        Self::fill_the_gaps(all_syms, ranges)
    }
}

pub(super) fn append_dummy_symbol(mut syms: PDBSymbols, name: &str) -> PDBSymbols {
    let (rva, len, id) = if let Some((_, last_sym)) = syms.iter().next_back() {
        (last_sym.rva, last_sym.len, last_sym.id)
    } else {
        return syms;
    };

    let rva = if len == 0 { rva + len + 1 } else { rva + len };

    let name = if name.is_empty() {
        String::from("<unknown>")
    } else {
        format!("<unknown in {}>", name)
    };

    syms.insert(
        rva,
        PDBSymbol {
            name,
            is_public: true,
            is_multiple: false,
            rva,
            len: 0,
            parameter_size: 0,
            source: Rc::new(Lines::new()),
            id: id + 1,
        },
    );

    syms
}

#[inline(always)]
fn fix_mangled_name(name: String) -> String {
    name.replace("__cdecl", "")
        .replace("public: ", "")
        .replace("protected: ", "")
        .replace("private: ", "")
        .replace("(void)", "")
        .replace("  ", " ")
}

pub fn demangle(ident: &str) -> FuncName {
    // If the name is not mangled maybe we can guess stacksize in using it.
    // So the boolean flag in the returned value is here for that (true == known language)
    // For information:
    //  - msvc-demangler has no problem with symbols containing ".llvm."
    let lang = Name::new(ident, NameMangling::Mangled, Language::Unknown).detect_language();
    if lang == Language::Unknown {
        return FuncName::get_unknown(ident.to_string());
    }

    let name = Name::new(ident, NameMangling::Mangled, lang);
    let name = common::fix_symbol_name(&name);

    match name.demangle(demangle_options()) {
        Some(demangled) => {
            if demangled == ident {
                // Maybe the langage detection was finally wrong
                FuncName::get_unknown(demangled)
            } else {
                let demangled = fix_mangled_name(demangled);
                FuncName::Undecorated(demangled)
            }
        }
        None => {
            warn!("Didn't manage to demangle {}", ident);
            FuncName::Undecorated(ident.to_string())
        }
    }
}

pub(super) fn symbolic_to_pdb_symbols(
    syms: PeSymbolIterator,
    exception_data: Option<&ExceptionData<'_>>,
    module_name: &str,
) -> PDBSymbols {
    let mut pdb_syms = PDBSymbols::default();

    let module_name = if module_name.is_empty() {
        String::from("<unknown>")
    } else {
        format!("<unknown in {}>", module_name)
    };

    if let Some(data) = exception_data {
        data.into_iter()
            .filter_map(|result| result.ok())
            .filter(|function| function.end_address > function.begin_address)
            .for_each(|function| {
                pdb_syms.insert(
                    function.begin_address,
                    PDBSymbol {
                        name: module_name.clone(),
                        is_public: false,
                        is_multiple: false,
                        rva: function.begin_address,
                        len: function.end_address - function.begin_address,
                        parameter_size: 0,
                        source: Rc::new(Lines::new()),
                        id: 0,
                    },
                );
            });
    };

    for sym in syms {
        if let Some(name) = sym.name() {
            let demangled_name = demangle(name);
            let (name, parameter_size) = match demangled_name {
                FuncName::Undecorated(name) => (name, 0),
                FuncName::Unknown((name, parameter_size)) => (name, parameter_size),
            };
            let rva = sym.address as u32;
            pdb_syms
                .entry(rva)
                .and_modify(|e| {
                    e.name = name.clone();
                    e.parameter_size = parameter_size;
                })
                .or_insert(PDBSymbol {
                    name,
                    is_public: true,
                    is_multiple: false,
                    rva: sym.address as u32,
                    len: 0,
                    parameter_size,
                    source: Rc::new(Lines::new()),
                    id: 0,
                });
        }
    }

    pdb_syms
}
