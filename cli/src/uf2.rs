//! ELF -> UF2. Takes the loadable segments in flash (by physical address) and splits
//! them into 256-byte UF2 blocks.

use anyhow::{Context, Result, bail};
use object::elf::{EM_RISCV, PT_LOAD};
use object::read::elf::{ElfFile32, FileHeader, ProgramHeader};
use std::collections::BTreeMap;

pub const RP2040: u32 = 0xe48b_ff56;
pub const RP2350_ARM_S: u32 = 0xe48b_ff59;
pub const RP2350_RISCV: u32 = 0xe48b_ff5a;

const FLASH: std::ops::Range<u32> = 0x1000_0000..0x1400_0000;
const PAGE: u32 = 256;
const FLAG_FAMILY_ID: u32 = 0x0000_2000;

struct Elf {
    is_riscv: bool,
    /// Flash page address -> 256 bytes of content
    pages: BTreeMap<u32, [u8; PAGE as usize]>,
}

fn parse(data: &[u8]) -> Result<Elf> {
    let elf = ElfFile32::<object::Endianness>::parse(data).context("not a 32-bit ELF file")?;
    let endian = elf.endian();
    let header = elf.elf_header();
    let is_riscv = header.e_machine(endian) == EM_RISCV;

    let mut pages = BTreeMap::new();
    for ph in header.program_headers(endian, data)? {
        if ph.p_type(endian) != PT_LOAD || ph.p_filesz(endian) == 0 {
            continue;
        }
        let start = ph.p_paddr(endian);
        let bytes = ph
            .data(endian, data)
            .map_err(|_| anyhow::anyhow!("invalid segment data"))?;
        let end = start + bytes.len() as u32;
        if !FLASH.contains(&start) {
            continue;
        }
        if end > FLASH.end {
            bail!("segment 0x{start:08x}..0x{end:08x} is outside flash");
        }
        for (i, &b) in bytes.iter().enumerate() {
            let addr = start + i as u32;
            let page = pages
                .entry(addr & !(PAGE - 1))
                .or_insert([0u8; PAGE as usize]);
            page[(addr % PAGE) as usize] = b;
        }
    }
    if pages.is_empty() {
        bail!("ELF has no data in flash (0x10000000..)");
    }
    Ok(Elf { is_riscv, pages })
}

/// Picks the family ID from the `Model:` line on the BOOTSEL drive and the ELF architecture.
pub fn family_for(data: &[u8], model: &str) -> Result<u32> {
    if model.contains("RP2040") {
        return Ok(RP2040);
    }
    Ok(if parse(data)?.is_riscv {
        RP2350_RISCV
    } else {
        RP2350_ARM_S
    })
}

pub fn convert(data: &[u8], family: u32) -> Result<Vec<u8>> {
    let elf = parse(data)?;
    let total = elf.pages.len() as u32;
    let mut out = Vec::with_capacity(elf.pages.len() * 512);
    for (block_no, (addr, page)) in elf.pages.iter().enumerate() {
        for word in [
            0x0A32_4655, // magic start 0
            0x9E5D_5157, // magic start 1
            FLAG_FAMILY_ID,
            *addr,
            PAGE,
            block_no as u32,
            total,
            family,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(page);
        out.resize(out.len() + 476 - PAGE as usize, 0);
        out.extend_from_slice(&0x0AB1_6F30u32.to_le_bytes()); // magic end
    }
    Ok(out)
}
