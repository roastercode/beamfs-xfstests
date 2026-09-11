// SPDX-License-Identifier: GPL-2.0-only
//! Read a frozen volume and say what its tree actually looks like.
//!
//! A campaign reports a number of lost blocks. The number says how many;
//! it never says why, and the volume is reformatted before anyone can
//! ask. Freezing the volume made the question possible and this answers
//! it, on the image rather than on the live device, as many times as
//! needed.
//!
//! The defect it was written for: an indirect block named by an inode
//! on disk whose contents were never written. Inode 36 of one run held
//! i_indirect = 16469, and block 16469 held 0xcd across 464 of its 512
//! slots -- the fresh buffer had been released and read back, and
//! evicted in between, so the read came from a medium nothing had
//! written to. fsck walked it, found no pointer it could follow, and
//! called 273 blocks used and referenced by nothing: the largest leak
//! of the run, all of it from one block.
//!
//! That shape is worth naming rather than rediscovering. An indirect
//! block that holds no plausible pointers is not a corrupted tree, it
//! is a block that never got written, and the two want different fixes.

use std::collections::BTreeMap;
use std::path::Path;

const BLOCK: u64 = 4096;
const PTRS_PER_BLOCK: usize = 512;

/// Where each superblock field sits, asked of the compiler.
///
/// Offsets written by hand were wrong three times in two days, each
/// time in a way that read as a fact rather than as an error: a set
/// flag looked unset, and an RS volume looked like a CRC one. The
/// header is the only thing that knows, so the header is asked.
///
/// The probe is compiled once per process into a temporary file. If it
/// cannot be built -- no compiler, no header -- the fallback offsets
/// are the ones measured on format v5, and the caller is told they are
/// assumed rather than known.
struct Offsets {
    block_count: usize,
    inode_count: usize,
    inode_table: usize,
    data_start: usize,
    feat_incompat: usize,
    ind_parity_blk: usize,
    ind_parity_len: usize,
    ind_parity_mode: usize,
    inode_size: u64,
    /// False when the probe could not run and the values are assumed.
    measured: bool,
}

impl Default for Offsets {
    fn default() -> Self {
        // Format v5, measured 2026-09-11. Kept only as a fallback: a
        // format change moves these and nothing here would notice.
        Offsets {
            block_count: 8,
            inode_count: 24,
            inode_table: 40,
            data_start: 48,
            feat_incompat: 2693,
            ind_parity_blk: 2713,
            ind_parity_len: 2721,
            ind_parity_mode: 2725,
            inode_size: 256,
            measured: false,
        }
    }
}

/// Ask the compiler where the fields are.
fn probe_offsets(header_dir: &Path) -> Offsets {
    let src = r#"
#include <stdio.h>
#include <stddef.h>
#include "beamfs_format.h"
int main(void){
#define P(f) printf("%s %zu\n", #f, offsetof(struct beamfs_super_block, f))
  P(s_block_count); P(s_inode_count); P(s_inode_table_blk);
  P(s_data_start_blk); P(s_feat_incompat); P(s_ind_parity_blk);
  P(s_ind_parity_len); P(s_ind_parity_mode);
  printf("inode_size %zu
", sizeof(struct beamfs_inode));
  return 0;
}
"#;
    let dir = std::env::temp_dir().join(format!("beamfs-off-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let c = dir.join("off.c");
    let bin = dir.join("off");
    if std::fs::write(&c, src).is_err() {
        return Offsets::default();
    }
    let built = std::process::Command::new("cc")
        .args(["-I", &header_dir.to_string_lossy(), "-o"])
        .arg(&bin)
        .arg(&c)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !built {
        let _ = std::fs::remove_dir_all(&dir);
        return Offsets::default();
    }
    let out = std::process::Command::new(&bin)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    let mut o = Offsets { measured: true, ..Default::default() };
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
        let Ok(v) = v.parse::<usize>() else { continue };
        match k {
            "s_block_count" => o.block_count = v,
            "s_inode_count" => o.inode_count = v,
            "s_inode_table_blk" => o.inode_table = v,
            "s_data_start_blk" => o.data_start = v,
            "s_feat_incompat" => o.feat_incompat = v,
            "s_ind_parity_blk" => o.ind_parity_blk = v,
            "s_ind_parity_len" => o.ind_parity_len = v,
            "s_ind_parity_mode" => o.ind_parity_mode = v,
            "inode_size" => o.inode_size = v as u64,
            _ => {}
        }
    }
    o
}

/// How the indirect blocks are protected on this volume.
///
/// The difference decides what "beyond correction" means: under CRC it
/// is the expected outcome of any single flipped bit, because a CRC
/// detects and cannot repair; under RS it means the damage exceeded
/// eight symbols in a subblock, which is a real defect worth chasing.
#[derive(Clone, Copy, PartialEq)]
pub enum ParityMode {
    None,
    Crc,
    Rs,
    Unknown(u32),
}

impl ParityMode {
    fn from(v: u32) -> Self {
        match v {
            0 => ParityMode::None,
            1 => ParityMode::Crc,
            2 => ParityMode::Rs,
            other => ParityMode::Unknown(other),
        }
    }

    pub fn name(&self) -> String {
        match self {
            ParityMode::None => "none".into(),
            ParityMode::Crc => "crc (detects, cannot correct)".into(),
            ParityMode::Rs => "rs (corrects up to 8 symbols a subblock)".into(),
            ParityMode::Unknown(v) => format!("unrecognised value {v}"),
        }
    }
}

/// The fields this needs from the superblock, read once.
struct Geometry {
    /// Where the allocation region begins. Not a bound on what is
    /// readable -- the root and the canary sit just below it -- but it
    /// is what distinguishes a reserved block from an allocated one.
    #[allow(dead_code)]
    data_start: u64,
    block_count: u64,
    inode_table: u64,
    inode_count: u64,
    inode_size: u64,
    parity_mode: ParityMode,
    feat_incompat: u64,
    /// False when the field offsets could not be measured.
    offsets_measured: bool,
}

/// What one inode's tree looks like on the medium.
pub struct TreeReport {
    pub ino: u64,
    /// Blocks the inode names directly or through its tree.
    pub reachable: u64,
    /// Indirect blocks whose contents are not pointers at all.
    pub unwritten: Vec<u64>,
    /// Indirect blocks pointing outside the device.
    pub out_of_range: Vec<u64>,
}

pub struct VolumeReport {
    /// How the indirect blocks on this volume are protected. Without
    /// it, "beyond correction" cannot be read: it is routine under CRC
    /// and a defect under RS.
    pub parity_mode: ParityMode,
    pub feat_incompat: u64,
    pub offsets_measured: bool,
    pub inodes_walked: u64,
    pub trees: Vec<TreeReport>,
    /// The byte that fills an unwritten block, and how many blocks it
    /// fills: naming it turns "corruption" into "never written".
    pub fill_bytes: BTreeMap<u8, u64>,
}

fn le32(buf: &[u8], off: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[off..off + 4]);
    u32::from_le_bytes(b)
}

fn le64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

fn read_at(f: &mut std::fs::File, off: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut v = vec![0u8; len];
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(&mut v)?;
    Ok(v)
}

/// Does this block hold pointers, or was it never written?
///
/// A written indirect block holds block numbers: zero, or somewhere
/// inside the device. An unwritten one holds whatever the medium had --
/// in practice one byte repeated, because that is what a fresh image or
/// a previous test leaves behind. Judging by "most slots are one
/// repeated value" is what distinguishes the two without needing to
/// know which byte a given medium happens to use.
fn looks_unwritten(raw: &[u8], geo: &Geometry) -> Option<u8> {
    let mut plausible = 0usize;
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();

    for i in 0..PTRS_PER_BLOCK {
        let v = le64(raw, i * 8);
        *counts.entry(v).or_insert(0) += 1;
        // Plausible means "could be a block on this device": zero for
        // an empty slot, anything inside the device otherwise. The
        // reserved blocks below data_start count -- the root lives
        // there.
        if v == 0 || v < geo.block_count {
            plausible += 1;
        }
    }

    // A written block is almost entirely plausible: real pointers and
    // zeroed slack. Anything under half says the contents are not a
    // pointer array at all.
    if plausible * 2 >= PTRS_PER_BLOCK {
        return None;
    }

    // The dominant value, expressed as its repeated byte when it is
    // one -- 0xcdcdcdcdcdcdcdcd is 0xcd, and saying "0xcd" is clearer
    // than saying the whole word.
    let (&dominant, &n) = counts.iter().max_by_key(|(_, n)| **n)?;
    if n * 4 < PTRS_PER_BLOCK {
        return Some(0);
    }
    let bytes = dominant.to_le_bytes();
    if bytes.iter().all(|b| *b == bytes[0]) {
        Some(bytes[0])
    } else {
        Some(0)
    }
}

/// Walk one subtree, counting what it reaches and noting what it cannot.
fn walk(
    f: &mut std::fs::File,
    geo: &Geometry,
    blk: u64,
    level: u32,
    rep: &mut TreeReport,
    fill: &mut BTreeMap<u8, u64>,
) {
    if blk == 0 {
        return;
    }
    /*
     * Below data_start is still inside the device.
     *
     * mkfs puts the root directory and the canary at data_start - 2 and
     * - 1, outside the allocation bitmap: never allocated, never freed,
     * and perfectly valid. Taking data_start as the lower bound flagged
     * both of them on every healthy volume -- the same mistake made in
     * the checker's reader the day before, and worth not making twice.
     *
     * What is outside the device is what is past its end, or zero.
     */
    if blk == 0 || blk >= geo.block_count {
        rep.out_of_range.push(blk);
        return;
    }
    rep.reachable += 1;

    let Ok(raw) = read_at(f, blk * BLOCK, BLOCK as usize) else { return };

    if let Some(byte) = looks_unwritten(&raw, geo) {
        // Not walked. Following pointers out of a block that holds no
        // pointers is how one unwritten block becomes a report of
        // hundreds of lost ones.
        rep.unwritten.push(blk);
        *fill.entry(byte).or_insert(0) += 1;
        return;
    }

    for i in 0..PTRS_PER_BLOCK {
        let child = le64(&raw, i * 8);
        if child == 0 {
            continue;
        }
        if level > 1 {
            walk(f, geo, child, level - 1, rep, fill);
        } else if child < geo.block_count {
            rep.reachable += 1;
        } else {
            rep.out_of_range.push(child);
        }
    }
}

/// Read a frozen volume and report on every inode's tree.
pub fn inspect(path: &Path) -> std::io::Result<VolumeReport> {
    let mut f = std::fs::File::open(path)?;
    let sb = read_at(&mut f, 0, BLOCK as usize)?;

    let off = probe_offsets(
        &std::env::var("BEAMFS_HEADER_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(
                    std::env::var("HOME").unwrap_or_default(),
                )
                .join("git/beamfs/tools/fsck.beamfs")
            }),
    );

    let geo = Geometry {
        block_count: le64(&sb, off.block_count),
        inode_count: le64(&sb, off.inode_count),
        inode_table: le64(&sb, off.inode_table),
        data_start: le64(&sb, off.data_start),
        inode_size: off.inode_size,
        feat_incompat: le64(&sb, off.feat_incompat),
        parity_mode: ParityMode::from(le32(&sb, off.ind_parity_mode)),
        offsets_measured: off.measured,
    };

    let mut out = VolumeReport {
        parity_mode: geo.parity_mode,
        feat_incompat: geo.feat_incompat,
        offsets_measured: geo.offsets_measured,
        inodes_walked: 0,
        trees: Vec::new(),
        fill_bytes: BTreeMap::new(),
    };

    let per_block = BLOCK / geo.inode_size;
    for ino in 1..=geo.inode_count {
        let blk = geo.inode_table + (ino - 1) / per_block;
        let idx = (ino - 1) % per_block;
        let Ok(raw) = read_at(&mut f, blk * BLOCK + idx * geo.inode_size,
                              geo.inode_size as usize) else { continue };

        let mode = u16::from_le_bytes([raw[0], raw[1]]);
        if mode == 0 {
            continue;
        }
        out.inodes_walked += 1;

        let mut rep = TreeReport {
            ino,
            reachable: 0,
            unwritten: Vec::new(),
            out_of_range: Vec::new(),
        };

        // i_direct at 52, then the three indirect pointers.
        for k in 0..12 {
            let v = le64(&raw, 52 + k * 8);
            if v != 0 {
                if v < geo.block_count {
                    rep.reachable += 1;
                } else {
                    rep.out_of_range.push(v);
                }
            }
        }
        walk(&mut f, &geo, le64(&raw, 148), 1, &mut rep, &mut out.fill_bytes);
        walk(&mut f, &geo, le64(&raw, 156), 2, &mut rep, &mut out.fill_bytes);
        walk(&mut f, &geo, le64(&raw, 164), 3, &mut rep, &mut out.fill_bytes);

        if !rep.unwritten.is_empty() || !rep.out_of_range.is_empty() {
            out.trees.push(rep);
        }
    }
    Ok(out)
}

pub fn report(r: &VolumeReport) {
    println!("  === the tree as it sits on the medium ===");
    println!(
        "  indirect parity: {} | features 0x{:x}",
        r.parity_mode.name(),
        r.feat_incompat
    );
    if !r.offsets_measured {
        println!("  -- field offsets assumed, not measured: no compiler or header found,");
        println!("     so everything below could be reading the wrong fields");
    }
    println!("  {} allocated inode(s) walked", r.inodes_walked);

    if r.trees.is_empty() {
        println!("  every indirect block holds pointers; nothing was left unwritten");
        println!();
        return;
    }

    for t in &r.trees {
        if !t.unwritten.is_empty() {
            println!(
                "  inode {}: {} indirect block(s) hold no pointers at all -- {}",
                t.ino,
                t.unwritten.len(),
                t.unwritten
                    .iter()
                    .take(4)
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("    the inode names them on disk and nothing ever wrote them");
        }
        if !t.out_of_range.is_empty() {
            println!(
                "  inode {}: {} pointer(s) outside the device",
                t.ino,
                t.out_of_range.len()
            );
        }
    }

    if !r.fill_bytes.is_empty() {
        let parts: Vec<String> = r
            .fill_bytes
            .iter()
            .map(|(b, n)| format!("0x{b:02x} in {n}"))
            .collect();
        println!();
        println!("  what fills them: {}", parts.join(", "));
        println!("  -- a repeated byte is a block that was allocated, named,");
        println!("     and never written; not a tree that got corrupted");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo() -> Geometry {
        Geometry {
            data_start: 1000,
            block_count: 100_000,
            inode_table: 1,
            inode_count: 16,
            inode_size: 256,
        }
    }

    #[test]
    fn a_block_of_pointers_is_not_unwritten() {
        let mut raw = vec![0u8; BLOCK as usize];
        for i in 0..40 {
            raw[i * 8..i * 8 + 8].copy_from_slice(&(2000u64 + i as u64).to_le_bytes());
        }
        assert!(looks_unwritten(&raw, &geo()).is_none());
    }

    #[test]
    fn a_block_of_one_repeated_byte_names_that_byte() {
        let raw = vec![0xcdu8; BLOCK as usize];
        assert_eq!(looks_unwritten(&raw, &geo()), Some(0xcd));
    }

    #[test]
    fn an_all_zero_block_is_a_written_block_with_nothing_in_it() {
        // Zero is a plausible slot: a fresh indirect block is all
        // zeroes and is perfectly written.
        let raw = vec![0u8; BLOCK as usize];
        assert!(looks_unwritten(&raw, &geo()).is_none());
    }
}

/// Inspect a zstd-compressed image without keeping it uncompressed.
///
/// The frozen volumes are stored compressed -- a gigabyte of mostly
/// zeroes is a couple of megabytes -- and decompressing every one to
/// look at it would fill the disk a campaign is running on.
pub fn inspect_compressed(path: &Path) -> std::io::Result<VolumeReport> {
    let tmp = std::env::temp_dir().join(format!(
        "beamfs-inspect-{}.img",
        std::process::id()
    ));
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "zstd -dcf {} > {}",
            path.display(),
            tmp.display()
        ))
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("could not decompress the image"));
    }
    let r = inspect(&tmp);
    let _ = std::fs::remove_file(&tmp);
    r
}
