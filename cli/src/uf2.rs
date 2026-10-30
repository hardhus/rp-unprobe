//! ELF -> UF2. Takes the loadable segments in flash (by physical address) and splits
//! them into 256-byte UF2 blocks.

use anyhow::{Context, Result, bail};
use object::elf::{EM_RISCV, PT_LOAD};
use object::read::elf::{ElfFile32, FileHeader, ProgramHeader};
use object::{Object, ObjectSection};
use std::collections::BTreeMap;

pub const RP2040: u32 = 0xe48b_ff56;
pub const RP2350_ARM_S: u32 = 0xe48b_ff59;
pub const RP2350_RISCV: u32 = 0xe48b_ff5a;

const FLASH: std::ops::Range<u32> = 0x1000_0000..0x1400_0000;
const PAGE: u32 = 256;
const FLAG_FAMILY_ID: u32 = 0x0000_2000;

/// The chip an ELF was built for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chip {
    Rp2040,
    Rp2350,
}

struct Elf {
    is_riscv: bool,
    /// From the Arm core in `.ARM.attributes`; `None` if the ELF doesn't say.
    arm_chip: Option<Chip>,
    /// Flash page address -> 256 bytes of content
    pages: BTreeMap<u32, [u8; PAGE as usize]>,
}

fn parse(data: &[u8]) -> Result<Elf> {
    let elf = ElfFile32::<object::Endianness>::parse(data).context("not a 32-bit ELF file")?;
    let endian = elf.endian();
    let header = elf.elf_header();
    let is_riscv = header.e_machine(endian) == EM_RISCV;
    let arm_chip = elf
        .section_by_name(".ARM.attributes")
        .and_then(|s| s.data().ok())
        .and_then(cpu_arch)
        .and_then(|arch| match arch {
            // v6-M, v6S-M: Cortex-M0+
            11 | 12 => Some(Chip::Rp2040),
            // v8-M baseline / mainline: Cortex-M33
            16 | 17 => Some(Chip::Rp2350),
            _ => None,
        });

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
    Ok(Elf {
        is_riscv,
        arm_chip,
        pages,
    })
}

fn uleb128(data: &mut &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let (&b, rest) = data.split_first()?;
        *data = rest;
        value |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn skip_string(data: &mut &[u8]) -> Option<()> {
    let end = data.iter().position(|&b| b == 0)?;
    *data = &data[end + 1..];
    Some(())
}

/// `Tag_CPU_arch` from the "aeabi" build attributes (ARM ELF ABI, "Build Attributes").
fn cpu_arch(section: &[u8]) -> Option<u32> {
    const TAG_FILE: u8 = 1;
    const TAG_CPU_ARCH: u32 = 6;
    let mut data = section.strip_prefix(b"A")?;
    while data.len() >= 4 {
        let len = u32::from_le_bytes(data[..4].try_into().ok()?) as usize;
        let sub = data.get(4..len)?;
        data = &data[len..];
        let Some(mut attrs) = sub.strip_prefix(b"aeabi\0") else {
            continue;
        };
        // Only the file-wide attributes: tag, size (including both), attributes.
        if attrs.first() != Some(&TAG_FILE) {
            continue;
        }
        let size = u32::from_le_bytes(attrs.get(1..5)?.try_into().ok()?) as usize;
        attrs = attrs.get(5..size)?;
        while !attrs.is_empty() {
            let tag = uleb128(&mut attrs)?;
            match tag {
                TAG_CPU_ARCH => return uleb128(&mut attrs),
                // Tag_CPU_raw_name, Tag_CPU_name, and odd tags above 32: strings.
                4 | 5 => skip_string(&mut attrs)?,
                t if t > 32 && t % 2 == 1 => skip_string(&mut attrs)?,
                // Tag_compatibility: a number and a string.
                32 => {
                    uleb128(&mut attrs)?;
                    skip_string(&mut attrs)?;
                }
                _ => {
                    uleb128(&mut attrs)?;
                }
            }
        }
    }
    None
}

/// Picks the family ID from the ELF and, for `unprobe run`, the chip on the BOOTSEL drive
/// (`Some(true)`: RP2040). Refuses an ELF built for the other chip: it would be written
/// but couldn't start.
pub fn family_for(data: &[u8], board_rp2040: Option<bool>) -> Result<u32> {
    let elf = parse(data)?;
    let hint = "The board stays in BOOTSEL. (`--family` skips this check.)";
    match (board_rp2040, elf.arm_chip) {
        (Some(true), Some(Chip::Rp2350)) => bail!(
            "this ELF is built for the RP2350 (Cortex-M33), but the board is an RP2040 \
             (e.g. a Pico W). Plug in the RP2350 board, or build for thumbv6m-none-eabi \
             with embassy-rp's `rp2040` feature. {hint}"
        ),
        (Some(false), Some(Chip::Rp2040)) => bail!(
            "this ELF is built for the RP2040 (Cortex-M0+), but the board is an RP2350 \
             (e.g. a Pico 2). Plug in the RP2040 board, or build for thumbv8m.main-none-eabihf \
             with embassy-rp's `rp235xa` feature. {hint}"
        ),
        (Some(true), _) if elf.is_riscv => {
            bail!("this is a RISC-V ELF, but the board is an RP2040 (Arm only). {hint}")
        }
        _ => {}
    }
    let rp2040 = board_rp2040.unwrap_or(elf.arm_chip == Some(Chip::Rp2040));
    Ok(if rp2040 {
        RP2040
    } else if elf.is_riscv {
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
