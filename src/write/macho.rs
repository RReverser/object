use std::mem;
use std::string::String;
use std::vec::Vec;

use crate::endian::*;
use crate::macho;
use crate::pod::BytesMut;
use crate::write::string::*;
use crate::write::util::*;
use crate::write::*;

#[derive(Default, Clone, Copy)]
struct SectionOffsets {
    index: usize,
    offset: usize,
    address: u64,
    reloc_offset: usize,
}

#[derive(Default, Clone, Copy)]
struct SymbolOffsets {
    index: usize,
    str_id: Option<StringId>,
}

impl Object {
    pub(crate) fn macho_set_subsections_via_symbols(&mut self) {
        let flags = match self.flags {
            FileFlags::MachO { flags } => flags,
            _ => 0,
        };
        self.flags = FileFlags::MachO {
            flags: flags | macho::MH_SUBSECTIONS_VIA_SYMBOLS,
        };
    }

    pub(crate) fn macho_segment_name(&self, segment: StandardSegment) -> &'static [u8] {
        match segment {
            StandardSegment::Text => &b"__TEXT"[..],
            StandardSegment::Data => &b"__DATA"[..],
            StandardSegment::Debug => &b"__DWARF"[..],
        }
    }

    pub(crate) fn macho_section_info(
        &self,
        section: StandardSection,
    ) -> (&'static [u8], &'static [u8], SectionKind) {
        match section {
            StandardSection::Text => (&b"__TEXT"[..], &b"__text"[..], SectionKind::Text),
            StandardSection::Data => (&b"__DATA"[..], &b"__data"[..], SectionKind::Data),
            StandardSection::ReadOnlyData => {
                (&b"__TEXT"[..], &b"__const"[..], SectionKind::ReadOnlyData)
            }
            StandardSection::ReadOnlyDataWithRel => {
                (&b"__DATA"[..], &b"__const"[..], SectionKind::ReadOnlyData)
            }
            StandardSection::ReadOnlyString => (
                &b"__TEXT"[..],
                &b"__cstring"[..],
                SectionKind::ReadOnlyString,
            ),
            StandardSection::UninitializedData => (
                &b"__DATA"[..],
                &b"__bss"[..],
                SectionKind::UninitializedData,
            ),
            StandardSection::Tls => (&b"__DATA"[..], &b"__thread_data"[..], SectionKind::Tls),
            StandardSection::UninitializedTls => (
                &b"__DATA"[..],
                &b"__thread_bss"[..],
                SectionKind::UninitializedTls,
            ),
            StandardSection::TlsVariables => (
                &b"__DATA"[..],
                &b"__thread_vars"[..],
                SectionKind::TlsVariables,
            ),
            StandardSection::Common => (&b"__DATA"[..], &b"__common"[..], SectionKind::Common),
        }
    }

    fn macho_tlv_bootstrap(&mut self) -> SymbolId {
        match self.tlv_bootstrap {
            Some(id) => id,
            None => {
                let id = self.add_symbol(Symbol {
                    name: b"_tlv_bootstrap".to_vec(),
                    value: 0,
                    size: 0,
                    kind: SymbolKind::Text,
                    scope: SymbolScope::Dynamic,
                    weak: false,
                    section: SymbolSection::Undefined,
                    flags: SymbolFlags::None,
                });
                self.tlv_bootstrap = Some(id);
                id
            }
        }
    }

    /// Create the `__thread_vars` entry for a TLS variable.
    ///
    /// The symbol given by `symbol_id` will be updated to point to this entry.
    ///
    /// A new `SymbolId` will be returned. The caller must update this symbol
    /// to point to the initializer.
    ///
    /// If `symbol_id` is not for a TLS variable, then it is returned unchanged.
    pub(crate) fn macho_add_thread_var(&mut self, symbol_id: SymbolId) -> SymbolId {
        let symbol = self.symbol_mut(symbol_id);
        if symbol.kind != SymbolKind::Tls {
            return symbol_id;
        }

        // Create the initializer symbol.
        let mut name = symbol.name.clone();
        name.extend(b"$tlv$init");
        let init_symbol_id = self.add_raw_symbol(Symbol {
            name,
            value: 0,
            size: 0,
            kind: SymbolKind::Tls,
            scope: SymbolScope::Compilation,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });

        // Add the tlv entry.
        // Three pointers in size:
        //   - __tlv_bootstrap - used to make sure support exists
        //   - spare pointer - used when mapped by the runtime
        //   - pointer to symbol initializer
        let section = self.section_id(StandardSection::TlsVariables);
        let pointer_width = self.architecture.pointer_width().unwrap().bytes();
        let size = u64::from(pointer_width) * 3;
        let data = vec![0; size as usize];
        let offset = self.append_section_data(section, &data, u64::from(pointer_width));

        let tlv_bootstrap = self.macho_tlv_bootstrap();
        self.add_relocation(
            section,
            Relocation {
                offset: offset,
                size: pointer_width * 8,
                kind: RelocationKind::Absolute,
                encoding: RelocationEncoding::Generic,
                symbol: tlv_bootstrap,
                addend: 0,
            },
        )
        .unwrap();
        self.add_relocation(
            section,
            Relocation {
                offset: offset + u64::from(pointer_width) * 2,
                size: pointer_width * 8,
                kind: RelocationKind::Absolute,
                encoding: RelocationEncoding::Generic,
                symbol: init_symbol_id,
                addend: 0,
            },
        )
        .unwrap();

        // Update the symbol to point to the tlv.
        let symbol = self.symbol_mut(symbol_id);
        symbol.value = offset;
        symbol.size = size;
        symbol.section = SymbolSection::Section(section);

        init_symbol_id
    }

    pub(crate) fn macho_fixup_relocation(&mut self, mut relocation: &mut Relocation) -> i64 {
        let constant = match relocation.kind {
            RelocationKind::Relative
            | RelocationKind::GotRelative
            | RelocationKind::PltRelative => relocation.addend + 4,
            _ => relocation.addend,
        };
        relocation.addend -= constant;
        constant
    }

    pub(crate) fn macho_write(&self) -> Result<Vec<u8>, String> {
        let (is_32, pointer_align) = match self.architecture.pointer_width().unwrap() {
            PointerWidth::U16 | PointerWidth::U32 => (true, 4),
            PointerWidth::U64 => (false, 8),
        };
        let endian = match self.architecture.endianness().unwrap() {
            Endianness::Little => RunTimeEndian::Little,
            Endianness::Big => RunTimeEndian::Big,
        };
        let macho32 = MachO32 { endian };
        let macho64 = MachO64 { endian };
        let macho: &dyn MachO = if is_32 { &macho32 } else { &macho64 };

        // Calculate offsets of everything, and build strtab.
        let mut offset = 0;

        // Calculate size of Mach-O header.
        offset += macho.mach_header_size();

        // Calculate size of commands.
        let mut ncmds = 0;
        let command_offset = offset;

        // Calculate size of segment command and section headers.
        let segment_command_offset = offset;
        let segment_command_len =
            macho.segment_command_size() + self.sections.len() * macho.section_header_size();
        offset += segment_command_len;
        ncmds += 1;

        // Calculate size of symtab command.
        let symtab_command_offset = offset;
        let symtab_command_len = mem::size_of::<macho::SymtabCommand<RunTimeEndian>>();
        offset += symtab_command_len;
        ncmds += 1;

        let sizeofcmds = offset - command_offset;

        // Calculate size of section data.
        let segment_data_offset = offset;
        let mut section_offsets = vec![SectionOffsets::default(); self.sections.len()];
        let mut address = 0;
        for (index, section) in self.sections.iter().enumerate() {
            section_offsets[index].index = 1 + index;
            if !section.is_bss() {
                let len = section.data.len();
                if len != 0 {
                    offset = align(offset, section.align as usize);
                    section_offsets[index].offset = offset;
                    offset += len;
                } else {
                    section_offsets[index].offset = offset;
                }
                address = align_u64(address, section.align);
                section_offsets[index].address = address;
                address += section.size;
            }
        }
        for (index, section) in self.sections.iter().enumerate() {
            if section.kind.is_bss() {
                assert!(section.data.is_empty());
                address = align_u64(address, section.align);
                section_offsets[index].address = address;
                address += section.size;
            }
        }
        let segment_data_size = offset - segment_data_offset;

        // Count symbols and add symbol strings to strtab.
        let mut strtab = StringTable::default();
        let mut symbol_offsets = vec![SymbolOffsets::default(); self.symbols.len()];
        let mut nsyms = 0;
        for (index, symbol) in self.symbols.iter().enumerate() {
            match symbol.kind {
                SymbolKind::Unknown | SymbolKind::Text | SymbolKind::Data | SymbolKind::Tls => {}
                SymbolKind::File | SymbolKind::Section => continue,
                SymbolKind::Null | SymbolKind::Label => {
                    return Err(format!("unimplemented symbol {:?}", symbol))
                }
            }
            symbol_offsets[index].index = nsyms;
            nsyms += 1;
            if !symbol.name.is_empty() {
                symbol_offsets[index].str_id = Some(strtab.add(&symbol.name));
            }
        }

        // Calculate size of symtab.
        offset = align(offset, pointer_align);
        let symtab_offset = offset;
        let symtab_len = nsyms * macho.nlist_size();
        offset += symtab_len;

        // Calculate size of strtab.
        let strtab_offset = offset;
        let mut strtab_data = Vec::new();
        // Null name.
        strtab_data.push(0);
        strtab.write(1, &mut strtab_data);
        offset += strtab_data.len();

        // Calculate size of relocations.
        for (index, section) in self.sections.iter().enumerate() {
            let count = section.relocations.len();
            if count != 0 {
                offset = align(offset, 4);
                section_offsets[index].reloc_offset = offset;
                let len = count * mem::size_of::<macho::Relocation<RunTimeEndian>>();
                offset += len;
            }
        }

        // Start writing.
        let mut buffer = BytesMut(Vec::with_capacity(offset));

        // Write file header.
        let (cputype, cpusubtype) = match self.architecture {
            Architecture::Arm(_) => (macho::CPU_TYPE_ARM, macho::CPU_SUBTYPE_ARM_ALL),
            Architecture::Aarch64(_) => (macho::CPU_TYPE_ARM64, macho::CPU_SUBTYPE_ARM64_ALL),
            Architecture::I386 => (macho::CPU_TYPE_X86, macho::CPU_SUBTYPE_I386_ALL),
            Architecture::X86_64 => (macho::CPU_TYPE_X86_64, macho::CPU_SUBTYPE_X86_64_ALL),
            _ => {
                return Err(format!(
                    "unimplemented architecture {:?}",
                    self.architecture
                ))
            }
        };

        let flags = match self.flags {
            FileFlags::MachO { flags } => flags,
            _ => 0,
        };
        macho.write_mach_header(
            &mut buffer,
            MachHeader {
                cputype,
                cpusubtype,
                filetype: macho::MH_OBJECT,
                ncmds,
                sizeofcmds: sizeofcmds as u32,
                flags,
            },
        );

        // Write segment command.
        debug_assert_eq!(segment_command_offset, buffer.len());
        macho.write_segment_command(
            &mut buffer,
            SegmentCommand {
                cmdsize: segment_command_len as u32,
                segname: [0; 16],
                vmaddr: 0,
                vmsize: address,
                fileoff: segment_data_offset as u64,
                filesize: segment_data_size as u64,
                maxprot: macho::VM_PROT_READ | macho::VM_PROT_WRITE | macho::VM_PROT_EXECUTE,
                initprot: macho::VM_PROT_READ | macho::VM_PROT_WRITE | macho::VM_PROT_EXECUTE,
                nsects: self.sections.len() as u32,
                flags: 0,
            },
        );

        // Write section headers.
        for (index, section) in self.sections.iter().enumerate() {
            let mut sectname = [0; 16];
            sectname[..section.name.len()].copy_from_slice(&section.name);
            let mut segname = [0; 16];
            segname[..section.segment.len()].copy_from_slice(&section.segment);
            let flags = if let SectionFlags::MachO { flags } = section.flags {
                flags
            } else {
                match section.kind {
                    SectionKind::Text => {
                        macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS
                    }
                    SectionKind::Data => 0,
                    SectionKind::ReadOnlyData => 0,
                    SectionKind::ReadOnlyString => macho::S_CSTRING_LITERALS,
                    SectionKind::UninitializedData | SectionKind::Common => macho::S_ZEROFILL,
                    SectionKind::Tls => macho::S_THREAD_LOCAL_REGULAR,
                    SectionKind::UninitializedTls => macho::S_THREAD_LOCAL_ZEROFILL,
                    SectionKind::TlsVariables => macho::S_THREAD_LOCAL_VARIABLES,
                    SectionKind::Debug => macho::S_ATTR_DEBUG,
                    SectionKind::OtherString => macho::S_CSTRING_LITERALS,
                    SectionKind::Other
                    | SectionKind::Unknown
                    | SectionKind::Linker
                    | SectionKind::Metadata => 0,
                }
            };
            macho.write_section(
                &mut buffer,
                SectionHeader {
                    sectname,
                    segname,
                    addr: section_offsets[index].address,
                    size: section.size,
                    offset: section_offsets[index].offset as u32,
                    align: section.align.trailing_zeros(),
                    reloff: section_offsets[index].reloc_offset as u32,
                    nreloc: section.relocations.len() as u32,
                    flags,
                },
            );
        }

        // Write symtab command.
        debug_assert_eq!(symtab_command_offset, buffer.len());
        let symtab_command = macho::SymtabCommand {
            cmd: U32::new(endian, macho::LC_SYMTAB),
            cmdsize: U32::new(endian, symtab_command_len as u32),
            symoff: U32::new(endian, symtab_offset as u32),
            nsyms: U32::new(endian, nsyms as u32),
            stroff: U32::new(endian, strtab_offset as u32),
            strsize: U32::new(endian, strtab_data.len() as u32),
        };
        buffer.write(&symtab_command);

        // Write section data.
        debug_assert_eq!(segment_data_offset, buffer.len());
        for (index, section) in self.sections.iter().enumerate() {
            let len = section.data.len();
            if len != 0 {
                write_align(&mut buffer, section.align as usize);
                debug_assert_eq!(section_offsets[index].offset, buffer.len());
                buffer.write_bytes(&section.data);
            }
        }

        // Write symtab.
        write_align(&mut buffer, pointer_align);
        debug_assert_eq!(symtab_offset, buffer.len());
        for (index, symbol) in self.symbols.iter().enumerate() {
            match symbol.kind {
                SymbolKind::Unknown | SymbolKind::Text | SymbolKind::Data | SymbolKind::Tls => {}
                SymbolKind::File | SymbolKind::Section => continue,
                SymbolKind::Null | SymbolKind::Label => {
                    return Err(format!("unimplemented symbol {:?}", symbol))
                }
            }
            // TODO: N_STAB
            let (mut n_type, n_sect) = match symbol.section {
                SymbolSection::Undefined => (macho::N_UNDF | macho::N_EXT, 0),
                SymbolSection::Absolute => (macho::N_ABS, 0),
                SymbolSection::Section(id) => (macho::N_SECT, id.0 + 1),
                SymbolSection::None | SymbolSection::Common => {
                    return Err(format!("unimplemented symbol.section {:?}", symbol.section))
                }
            };
            match symbol.scope {
                SymbolScope::Unknown | SymbolScope::Compilation => {}
                SymbolScope::Linkage => {
                    n_type |= macho::N_EXT | macho::N_PEXT;
                }
                SymbolScope::Dynamic => {
                    n_type |= macho::N_EXT;
                }
            }

            let n_desc = if let SymbolFlags::MachO { n_desc } = symbol.flags {
                n_desc
            } else {
                let mut n_desc = 0;
                if symbol.weak {
                    if symbol.is_undefined() {
                        n_desc |= macho::N_WEAK_REF;
                    } else {
                        n_desc |= macho::N_WEAK_DEF;
                    }
                }
                n_desc
            };

            let n_value = match symbol.section.id() {
                Some(section) => section_offsets[section.0].address + symbol.value,
                None => symbol.value,
            };

            let n_strx = symbol_offsets[index]
                .str_id
                .map(|id| strtab.get_offset(id))
                .unwrap_or(0);

            macho.write_nlist(
                &mut buffer,
                Nlist {
                    n_strx: n_strx as u32,
                    n_type,
                    n_sect: n_sect as u8,
                    n_desc,
                    n_value,
                },
            );
        }

        // Write strtab.
        debug_assert_eq!(strtab_offset, buffer.len());
        buffer.extend(&strtab_data);

        // Write relocations.
        for (index, section) in self.sections.iter().enumerate() {
            if !section.relocations.is_empty() {
                write_align(&mut buffer, 4);
                debug_assert_eq!(section_offsets[index].reloc_offset, buffer.len());
                for reloc in &section.relocations {
                    let r_extern;
                    let r_symbolnum;
                    let symbol = &self.symbols[reloc.symbol.0];
                    if symbol.kind == SymbolKind::Section {
                        r_symbolnum = section_offsets[symbol.section.id().unwrap().0].index as u32;
                        r_extern = false;
                    } else {
                        r_symbolnum = symbol_offsets[reloc.symbol.0].index as u32;
                        r_extern = true;
                    }
                    let r_length = match reloc.size {
                        8 => 0,
                        16 => 1,
                        32 => 2,
                        64 => 3,
                        _ => return Err(format!("unimplemented reloc size {:?}", reloc)),
                    };
                    let (r_pcrel, r_type) = match self.architecture {
                        Architecture::I386 => match reloc.kind {
                            RelocationKind::Absolute => (false, macho::GENERIC_RELOC_VANILLA),
                            _ => return Err(format!("unimplemented relocation {:?}", reloc)),
                        },
                        Architecture::X86_64 => match (reloc.kind, reloc.encoding, reloc.addend) {
                            (RelocationKind::Absolute, RelocationEncoding::Generic, 0) => {
                                (false, macho::X86_64_RELOC_UNSIGNED)
                            }
                            (RelocationKind::Relative, RelocationEncoding::Generic, -4) => {
                                (true, macho::X86_64_RELOC_SIGNED)
                            }
                            (RelocationKind::Relative, RelocationEncoding::X86RipRelative, -4) => {
                                (true, macho::X86_64_RELOC_SIGNED)
                            }
                            (RelocationKind::Relative, RelocationEncoding::X86Branch, -4) => {
                                (true, macho::X86_64_RELOC_BRANCH)
                            }
                            (RelocationKind::PltRelative, RelocationEncoding::X86Branch, -4) => {
                                (true, macho::X86_64_RELOC_BRANCH)
                            }
                            (RelocationKind::GotRelative, RelocationEncoding::Generic, -4) => {
                                (true, macho::X86_64_RELOC_GOT)
                            }
                            (
                                RelocationKind::GotRelative,
                                RelocationEncoding::X86RipRelativeMovq,
                                -4,
                            ) => (true, macho::X86_64_RELOC_GOT_LOAD),
                            (RelocationKind::MachO { value, relative }, _, _) => (relative, value),
                            _ => return Err(format!("unimplemented relocation {:?}", reloc)),
                        },
                        _ => {
                            return Err(format!(
                                "unimplemented architecture {:?}",
                                self.architecture
                            ))
                        }
                    };
                    let reloc_info = macho::RelocationInfo {
                        r_address: reloc.offset as u32,
                        r_symbolnum,
                        r_pcrel,
                        r_length,
                        r_extern,
                        r_type,
                    };
                    buffer.write(&reloc_info.relocation(endian));
                }
            }
        }

        Ok(buffer.0)
    }
}

struct MachHeader {
    cputype: u32,
    cpusubtype: u32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
}

struct SegmentCommand {
    cmdsize: u32,
    segname: [u8; 16],
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: u32,
    initprot: u32,
    nsects: u32,
    flags: u32,
}

pub struct SectionHeader {
    sectname: [u8; 16],
    segname: [u8; 16],
    addr: u64,
    size: u64,
    offset: u32,
    align: u32,
    reloff: u32,
    nreloc: u32,
    flags: u32,
}

struct Nlist {
    n_strx: u32,
    n_type: u8,
    n_sect: u8,
    n_desc: u16,
    n_value: u64,
}

trait MachO {
    fn mach_header_size(&self) -> usize;
    fn segment_command_size(&self) -> usize;
    fn section_header_size(&self) -> usize;
    fn nlist_size(&self) -> usize;
    fn write_mach_header(&self, buffer: &mut BytesMut, section: MachHeader);
    fn write_segment_command(&self, buffer: &mut BytesMut, segment: SegmentCommand);
    fn write_section(&self, buffer: &mut BytesMut, section: SectionHeader);
    fn write_nlist(&self, buffer: &mut BytesMut, nlist: Nlist);
}

struct MachO32<E> {
    endian: E,
}

impl<E: Endian> MachO for MachO32<E> {
    fn mach_header_size(&self) -> usize {
        mem::size_of::<macho::MachHeader32<E>>()
    }

    fn segment_command_size(&self) -> usize {
        mem::size_of::<macho::SegmentCommand32<E>>()
    }

    fn section_header_size(&self) -> usize {
        mem::size_of::<macho::Section32<E>>()
    }

    fn nlist_size(&self) -> usize {
        mem::size_of::<macho::Nlist32<E>>()
    }

    fn write_mach_header(&self, buffer: &mut BytesMut, header: MachHeader) {
        let endian = self.endian;
        let magic = if endian.is_big_endian() {
            macho::MH_MAGIC
        } else {
            macho::MH_CIGAM
        };
        let header = macho::MachHeader32 {
            magic: U32::new(BigEndian, magic),
            cputype: U32::new(endian, header.cputype),
            cpusubtype: U32::new(endian, header.cpusubtype),
            filetype: U32::new(endian, header.filetype),
            ncmds: U32::new(endian, header.ncmds),
            sizeofcmds: U32::new(endian, header.sizeofcmds),
            flags: U32::new(endian, header.flags),
        };
        buffer.write(&header);
    }

    fn write_segment_command(&self, buffer: &mut BytesMut, segment: SegmentCommand) {
        let endian = self.endian;
        let segment = macho::SegmentCommand32 {
            cmd: U32::new(endian, macho::LC_SEGMENT),
            cmdsize: U32::new(endian, segment.cmdsize),
            segname: segment.segname,
            vmaddr: U32::new(endian, segment.vmaddr as u32),
            vmsize: U32::new(endian, segment.vmsize as u32),
            fileoff: U32::new(endian, segment.fileoff as u32),
            filesize: U32::new(endian, segment.filesize as u32),
            maxprot: U32::new(endian, segment.maxprot),
            initprot: U32::new(endian, segment.initprot),
            nsects: U32::new(endian, segment.nsects),
            flags: U32::new(endian, segment.flags),
        };
        buffer.write(&segment);
    }

    fn write_section(&self, buffer: &mut BytesMut, section: SectionHeader) {
        let endian = self.endian;
        let section = macho::Section32 {
            sectname: section.sectname,
            segname: section.segname,
            addr: U32::new(endian, section.addr as u32),
            size: U32::new(endian, section.size as u32),
            offset: U32::new(endian, section.offset),
            align: U32::new(endian, section.align),
            reloff: U32::new(endian, section.reloff),
            nreloc: U32::new(endian, section.nreloc),
            flags: U32::new(endian, section.flags),
            reserved1: U32::default(),
            reserved2: U32::default(),
        };
        buffer.write(&section);
    }

    fn write_nlist(&self, buffer: &mut BytesMut, nlist: Nlist) {
        let endian = self.endian;
        let nlist = macho::Nlist32 {
            n_strx: U32::new(endian, nlist.n_strx),
            n_type: nlist.n_type,
            n_sect: nlist.n_sect,
            n_desc: U16::new(endian, nlist.n_desc),
            n_value: U32::new(endian, nlist.n_value as u32),
        };
        buffer.write(&nlist);
    }
}

struct MachO64<E> {
    endian: E,
}

impl<E: Endian> MachO for MachO64<E> {
    fn mach_header_size(&self) -> usize {
        mem::size_of::<macho::MachHeader64<E>>()
    }

    fn segment_command_size(&self) -> usize {
        mem::size_of::<macho::SegmentCommand64<E>>()
    }

    fn section_header_size(&self) -> usize {
        mem::size_of::<macho::Section64<E>>()
    }

    fn nlist_size(&self) -> usize {
        mem::size_of::<macho::Nlist64<E>>()
    }

    fn write_mach_header(&self, buffer: &mut BytesMut, header: MachHeader) {
        let endian = self.endian;
        let magic = if endian.is_big_endian() {
            macho::MH_MAGIC_64
        } else {
            macho::MH_CIGAM_64
        };
        let header = macho::MachHeader64 {
            magic: U32::new(BigEndian, magic),
            cputype: U32::new(endian, header.cputype),
            cpusubtype: U32::new(endian, header.cpusubtype),
            filetype: U32::new(endian, header.filetype),
            ncmds: U32::new(endian, header.ncmds),
            sizeofcmds: U32::new(endian, header.sizeofcmds),
            flags: U32::new(endian, header.flags),
            reserved: U32::default(),
        };
        buffer.write(&header);
    }

    fn write_segment_command(&self, buffer: &mut BytesMut, segment: SegmentCommand) {
        let endian = self.endian;
        let segment = macho::SegmentCommand64 {
            cmd: U32::new(endian, macho::LC_SEGMENT_64),
            cmdsize: U32::new(endian, segment.cmdsize),
            segname: segment.segname,
            vmaddr: U64::new(endian, segment.vmaddr),
            vmsize: U64::new(endian, segment.vmsize),
            fileoff: U64::new(endian, segment.fileoff),
            filesize: U64::new(endian, segment.filesize),
            maxprot: U32::new(endian, segment.maxprot),
            initprot: U32::new(endian, segment.initprot),
            nsects: U32::new(endian, segment.nsects),
            flags: U32::new(endian, segment.flags),
        };
        buffer.write(&segment);
    }

    fn write_section(&self, buffer: &mut BytesMut, section: SectionHeader) {
        let endian = self.endian;
        let section = macho::Section64 {
            sectname: section.sectname,
            segname: section.segname,
            addr: U64::new(endian, section.addr),
            size: U64::new(endian, section.size),
            offset: U32::new(endian, section.offset),
            align: U32::new(endian, section.align),
            reloff: U32::new(endian, section.reloff),
            nreloc: U32::new(endian, section.nreloc),
            flags: U32::new(endian, section.flags),
            reserved1: U32::default(),
            reserved2: U32::default(),
            reserved3: U32::default(),
        };
        buffer.write(&section);
    }

    fn write_nlist(&self, buffer: &mut BytesMut, nlist: Nlist) {
        let endian = self.endian;
        let nlist = macho::Nlist64 {
            n_strx: U32::new(endian, nlist.n_strx),
            n_type: nlist.n_type,
            n_sect: nlist.n_sect,
            n_desc: U16::new(endian, nlist.n_desc),
            n_value: U64::new(endian, nlist.n_value),
        };
        buffer.write(&nlist);
    }
}
