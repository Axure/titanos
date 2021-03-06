#![allow(unused)]
use core::mem;
use core;
use core::cmp;
use core::ops::{Fn,FnMut};
use core::option::Option;
use core::option::Option::{Some, None};
use core::intrinsics::transmute;

use titanium::arch::reg::*;
use titanium::arch::mmu::*;
use titanium::arch::*;
use titanium::consts::*;
pub use titanium::drv;
pub use titanium::hw;

use mm::PageArena;
use World;

const ENTRIES : usize = 8192;
const _PER_LEVEL : u64 = 13;
const PAGE_SIZE : u64  = 64 * 1024;

const START_LEVEL : u8 = 2;
const END_LEVEL : u8 = 3;

/// Region size at a given level of translation
const REGION_SIZE : [u64; 4] = [0, ENTRIES as u64 * SZ_512MB as u64, SZ_512MB as u64, SZ_64KB as u64];
const IDX_MASK : [u64; 4] = [0, 0, L2_IDX::MASK, L3_IDX::MASK];
const IDX_SHIFT : [u64; 4] = [0, 0, L2_IDX::SHIFT, L3_IDX::SHIFT];

def_bitfields!(u64,
               L2_IDX(41, 29),
               L3_IDX(28, 16),
               LOW(16, 0),
               );

const TNSZ : u64 = 22;
const IA_WIDTH : u64 = 42; // IA[41:16]

const PTE_ATTRS_MMIO : u64 = 1 << pte::XN::SHIFT;
const PTE_ATTRS_RAM : u64 = pte::AP_RW << pte::AP::SHIFT;

/// Raw PTE, being just u64
#[repr(C)]
struct PteRaw(u64);

/// PTE is a reference to Raw PTE
/// and the level of it
struct Pte<'a> {
    raw : &'a PteRaw,
    level : u8
}

/// PteMut is a mut reference to Raw PTE
/// and the level of it
struct PteMut<'a> {
    raw : &'a mut PteRaw,
    level : u8
}

impl<'a> Pte<'a> {
    fn as_raw<'b>(&'b self) -> &'b u64 {
        let &PteRaw(ref raw) = self.raw;
        raw
    }

    fn can_be_table(&self) -> bool {
        self.level != END_LEVEL
    }

    fn is_valid(&self) -> bool {
        pte::TYPE::from(*self.as_raw()) == pte::TYPE_INVALID
    }

    fn is_table(&mut self) -> bool {
        pte::TYPE::from(*self.as_raw()) == pte::TYPE_TABLE || self.level == END_LEVEL
    }

    fn as_table<'b>(&'b mut self) -> PageTable<'b> {
        debug_assert!(self.is_table());
        let &raw = self.as_raw();
        PageTable{
            raw: unsafe { transmute(pte::ADDR::from(raw)) },
            level: self.level + 1
        }
    }
}

impl<'a> PteMut<'a> {
    fn as_raw<'b>(&'b mut self) -> &'b mut u64 {
        let &mut PteRaw(ref mut raw) = self.raw;
        raw
    }

    fn clear(&mut self) {
        *self.as_raw() = 0;
    }

    fn write(&mut self, mapping : Mapping) {
        debug_assert!(mapping.size == REGION_SIZE[self.level as usize]);
        debug_assert!(mapping.attr & !(pte::HATTRS::MASK | pte::LATTRS::MASK) == 0);
        debug_assert!(mapping.pa & !pte::ADDR::MASK == 0);
        debug_assert!(mapping.va & !pte::ADDR::MASK == 0);
        debug_assert!(mapping.pa & (mapping.size - 1) == 0);
        debug_assert!(mapping.va & (mapping.size - 1) == 0);

        *self.as_raw() = mapping.pa | mapping.attr;
    }

    fn can_be_table(&self) -> bool {
        self.level != END_LEVEL
    }

    /// Create new table in place of invalid PTE
    fn create_table<'b, 'w, H>(&'b mut self, world : &'w mut World<H>, mapping : Mapping) -> PageTableMut<'b>
        where H : hw::HW
    {
        debug_assert!(self.can_be_table());

        let start = world.page_pool.get().unwrap();
        *self.as_raw()= start as u64 | mapping.attr | pte::TYPE_TABLE << pte::TYPE::SHIFT;

        for idx in 0..ENTRIES {
            self.as_table_mut().pte(idx).clear();
        }

        self.as_table_mut()
    }

    /// Rewrite valid PTE as TABLE of finer granularity
    fn expand_to_table<'b, 'w, H>(&'b mut self, world : &'w mut World<H>, mut mapping : Mapping) -> PageTableMut<'b>
        where H : hw::HW
    {
        debug_assert!(self.can_be_table());

        let start = world.page_pool.get().unwrap();

        let old_raw = *self.as_raw();
        mapping.va = pte::ADDR::from(old_raw);
        mapping.attr = pte::HATTRS::from(old_raw) | pte::LATTRS::from(old_raw);

        *self.as_raw() = start as u64 | mapping.attr | pte::TYPE_TABLE << pte::TYPE::SHIFT;

        self.as_table_mut().map(world, mapping);

        self.as_table_mut()
    }

    fn is_valid(&mut self) -> bool {
        pte::TYPE::from(*self.as_raw()) == pte::TYPE_INVALID
    }

    fn is_table(&mut self) -> bool {
        pte::TYPE::from(*self.as_raw()) == pte::TYPE_TABLE || self.level == END_LEVEL
    }

    fn as_table_mut<'b>(&'b mut self) -> PageTableMut<'b> {
        debug_assert!(self.is_table());
        PageTableMut{
            raw: unsafe { transmute(pte::ADDR::from(*self.as_raw())) },
            level: self.level + 1
        }
    }
}

#[repr(C)]
struct PageTableRaw {
    entries : [PteRaw; ENTRIES],
}

struct PageTable<'a> {
    raw: &'a mut PageTableRaw,
    level : u8,
}

struct PageTableMut<'a> {
    raw: &'a mut PageTableRaw,
    level : u8,
}

#[derive(Copy, Clone)]
struct Mapping {
    va : u64,
    pa : u64,
    size : u64,
    attr : u64,
}

impl<'a> PageTable<'a> {
    pub fn pte<'b>(&'b self, i : usize) -> Pte<'b> {
        debug_assert!(i < ENTRIES);
        Pte {
            raw: &self.raw.entries[i],
            level: self.level
        }
    }
}

impl<'a> PageTableMut<'a> {

    pub fn pte<'b>(&'b mut self, i : usize) -> PteMut<'b> {
        debug_assert!(i < ENTRIES);
        PteMut {
            raw: &mut self.raw.entries[i],
            level: self.level
        }
    }

    pub fn with_pte<'b, F>(&'b mut self, i : usize, mut f : F)
        where F : FnMut(PteMut<'b>) {
        debug_assert!(i < ENTRIES);
        f(PteMut {
            raw: &mut self.raw.entries[i],
            level: self.level
        });
    }

    pub fn map<'w, H>(&mut self, world : &'w mut World<H>, mapping : Mapping)
        where H : hw::HW
    {
        let region_size = REGION_SIZE[self.level as usize];
        let region_mask = region_size - 1;
        let idx_mask = IDX_MASK[self.level as usize];
        let idx_shift = IDX_SHIFT[self.level as usize];

        let mut pa = mapping.pa;
        let mut va = mapping.va;
        let mut left = mapping.size;

        loop {
            if left == 0 {
                break;
            }

            let va_start_aligned = va & !region_mask;
            let va_end_aligned = va_start_aligned + region_size;
            let va_end = cmp::min(va + left, va_end_aligned);
            let size = va_end - va;

            let idx = ((va & idx_mask) >> idx_shift) as usize;

            let mapping = Mapping{pa: pa, va: va, size: size, attr: mapping.attr};

            self.with_pte(idx, |mut pte| {
                if pte.is_table() {
                    let mut table = pte.as_table_mut();
                    table.map(world, mapping);
                } else if region_size == size {
                    pte.write(mapping);
                } else if pte.is_valid() {
                    let mut table = pte.expand_to_table(world, mapping);
                    table.map(world, mapping);
                } else {
                    let mut table = pte.create_table(world, mapping);
                    table.map(world, mapping);
                }
            });

            left -= size;
            va += size;
            pa += size;

            debug_assert!(left == 0 && ((va & !region_mask) == 0))
        }

    }
}



selftest!(fn page_table_size(_uart) {
    mem::size_of::<PageTableRaw>() == PAGE_SIZE as usize
});

/*
pub struct PageTableRoot {
    root : u64,
    level : u8,
}

impl PageTableRoot {
    pub fn new(world : &mut World<hw::Real>) -> PageTableRoot {
        let start = world.page_pool.get();

        PageTableRoot {
            root: start.unwrap() as u64,
            level: START_LEVEL as u8,
        }
    }
}

impl PageTableRoot {

    pub fn root(&self) -> PageTable {
        PageTable {
            raw: unsafe { mem::transmute(self.root) },
            level: START_LEVEL as u8,
        }
    }

    pub fn map(&self, va : u64, pa : u64, size : u64) {
        self.root().map_recv(va, pa, size, 0);
    }

    pub fn start(&self) {
        let asid = 0;
        let addr = self.root; // TODO: check alignment

        ttbr0_el1::write(
            asid << ttbr0_el1::ASID::SHIFT |
            addr << ttbr0_el1::BADDR::SHIFT
            );


        ttbr1_el1::write(
            asid << ttbr0_el1::ASID::SHIFT |
            addr << ttbr0_el1::BADDR::SHIFT
            );

        // invalidate all to PoU
        unsafe { asm!("ic ialluis" :::: "volatile"); }
        dsb_sy();
        isb();

        // TODO: invalidate i- and c- cache by set-way
        // TODO: move to head?

        // TODO: fails ATM
        // unsafe { asm!("tlbi alle1is" :::: "volatile"); }
        dsb_sy();
        isb();
    }
}
*/
