// Copyright (c) 2021 by Rivos Inc.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

use core::slice;

use riscv_pages::*;

use crate::page_table::Result;
use crate::page_table::*;
use crate::page_tracking::PageState;
use crate::pte::{Pte, PteLeafPerms};

pub enum L1Table {}
pub enum L2Table {}
pub enum L3Table {}
pub enum L4Table {}

// Setup the page table levels for sv48x4.

impl PageTableLevel for L1Table {
    type LeafPageSize = PageSize4k;
    type NextLevel = L4Table; // Invalid
    const ADDR_SHIFT: u64 = 12;
    const ADDR_WIDTH: u64 = 9;
    const TABLE_PAGES: usize = 1;
}

impl PageTableLevel for L2Table {
    type LeafPageSize = PageSize2MB;
    type NextLevel = L1Table;
    const ADDR_SHIFT: u64 = 21;
    const ADDR_WIDTH: u64 = 9;
    const TABLE_PAGES: usize = 1;
}
impl UpperLevel for L2Table {}

impl PageTableLevel for L3Table {
    type LeafPageSize = PageSize1GB;
    type NextLevel = L2Table;
    const ADDR_SHIFT: u64 = 30;
    const ADDR_WIDTH: u64 = 9;
    const TABLE_PAGES: usize = 1;
}
impl UpperLevel for L3Table {}

impl PageTableLevel for L4Table {
    type LeafPageSize = PageSize512GB;
    type NextLevel = L3Table;
    const ADDR_SHIFT: u64 = 39;
    const ADDR_WIDTH: u64 = 11;
    const TABLE_PAGES: usize = 4;
}
impl UpperLevel for L4Table {}

/// An Sv48x4 set of mappings for second stage translation.
pub struct Sv48x4 {
    root: SequentialPages<PageSize4k>,
    owner: PageOwnerId,
    phys_pages: PageState,
}

impl Sv48x4 {
    // Returns the top level (L4) page table.
    fn top_level_directory(&mut self) -> PageTable<L4Table> {
        unsafe {
            // Safe to create an array of mutable ptes from the owned pages because the mut
            // reference guarantees the mut slice will be the only owner for the lifetime of the
            // `PageTable` that is returned.
            PageTable::from_slice(slice::from_raw_parts_mut(
                self.root.base() as *mut Pte,
                L4Table::TABLE_PAGES * ENTRIES_PER_PAGE,
            ))
        }
    }

    // returns true if the given gpa is mapped in the page table.
    fn addr_mapped(&mut self, guest_phys_addr: u64) -> bool {
        let addr = match self.host_4k_addr(guest_phys_addr) {
            None => return false,
            Some(a) => a,
        };
        self.phys_pages.owner(addr) == self.owner
    }

    fn host_4k_addr(&mut self, guest_phys_addr: u64) -> Option<PageAddr4k> {
        use TableEntryMut::*;
        use ValidTableEntryMut::*;

        let mut l4 = self.top_level_directory();
        let mut l3 = match l4.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(Leaf(pte)) => return Some(PageAddr4k::try_from(pte.pfn()).unwrap()),
        };
        let mut l2 = match l3.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(Leaf(pte)) => return Some(PageAddr4k::try_from(pte.pfn()).unwrap()),
        };
        let mut l1 = match l2.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(Leaf(pte)) => return Some(PageAddr4k::try_from(pte.pfn()).unwrap()),
        };
        match l1.entry_for_addr_mut(guest_phys_addr) {
            Valid(Leaf(pte)) => Some(PageAddr4k::try_from(pte.pfn()).unwrap()),
            _ => None,
        }
    }

    fn handle_fault_at<S: PageSize>(
        pte: &mut Pte,
        phys_pages: &mut PageState,
        owner: &PageOwnerId,
    ) -> bool {
        if pte.valid() {
            // TODO     check permissions and type
            return false;
        } else if pte.leaf() {
            let addr = PageAddr4k::try_from(pte.pfn()).unwrap();
            if phys_pages.owner(addr) == *owner {
                // Zero the page before mapping it back to this VM.
                unsafe {
                    // Safe because this table uniquely owns the page and it isn't mapped to a
                    // guest.
                    core::ptr::write_bytes(addr.bits() as *mut u8, 0, S::SIZE_BYTES as usize);
                }
                pte.mark_valid();
                return true;
            }
        }
        false
    }
}

impl PlatformPageTable for Sv48x4 {
    type TLD = L4Table;
    const HGATP_VALUE: u64 = 9;
    const TOP_LEVEL_ALIGN: u64 = 16 * 1024;

    fn page_owner_id(&self) -> PageOwnerId {
        self.owner
    }

    fn max_pte_pages(num_pages: u64) -> u64 {
        // Determine how much ram is needed for host sv48x4 mappings; 512 8-byte ptes per page
        let num_l1_pages = num_pages / 512 + 1;
        let num_l2_pages = num_l1_pages / 512 + 1;
        let num_l3_pages = num_l2_pages / 512 + 1;
        let num_l4_pages = 4;
        num_l1_pages + num_l2_pages + num_l3_pages + num_l4_pages
    }

    fn new(
        root: SequentialPages<PageSize4k>,
        owner: PageOwnerId,
        phys_pages: PageState,
    ) -> Result<Self> {
        Ok(Self {
            root,
            owner,
            phys_pages,
        })
    }

    fn phys_pages(&self) -> PageState {
        self.phys_pages.clone()
    }

    fn map_page_4k<F>(
        &mut self,
        guest_phys_addr: u64,
        page_to_map: Page4k,
        get_pte_page: &mut F,
    ) -> Result<()>
    where
        F: FnMut() -> Option<Page4k>,
    {
        let page_addr = page_to_map.addr();
        let mut l4 = self.top_level_directory();
        let mut l3 = l4.next_level_or_fill_fn(guest_phys_addr, get_pte_page)?;
        let mut l2 = l3.next_level_or_fill_fn(guest_phys_addr, get_pte_page)?;
        let mut l1 = l2.next_level_or_fill_fn(guest_phys_addr, get_pte_page)?;
        let _entry = l1.map_leaf(guest_phys_addr, page_to_map, PteLeafPerms::RWX);
        self.phys_pages
            .set_page_owner(page_addr, self.owner)
            .map_err(Error::SettingOwner)?;
        Ok(())
    }

    fn map_page_2mb<F>(
        &mut self,
        guest_phys_addr: u64,
        page_to_map: Page<PageSize2MB>,
        get_pte_page: &mut F,
    ) -> Result<()>
    where
        F: FnMut() -> Option<Page4k>,
    {
        let mut l4 = self.top_level_directory();
        let mut l3 = l4.next_level_or_fill_fn(guest_phys_addr, get_pte_page)?;
        let mut l2 = l3.next_level_or_fill_fn(guest_phys_addr, get_pte_page)?;
        let _entry = l2.map_leaf(guest_phys_addr, page_to_map, PteLeafPerms::RWX);
        Ok(())
    }

    fn unmap_page(&mut self, guest_phys_addr: u64) -> Option<UnmappedPage> {
        use TableEntryMut::*;
        use ValidTableEntryMut::*;
        let mut l4 = self.top_level_directory();
        let mut l3 = match l4.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(valid_leaf) => return valid_leaf.take_page().map(UnmappedPage::Tera),
        };
        let mut l2 = match l3.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(valid_leaf) => return valid_leaf.take_page().map(UnmappedPage::Giga),
        };
        let mut l1 = match l2.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(valid_leaf) => return valid_leaf.take_page().map(UnmappedPage::Mega),
        };
        match l1.entry_for_addr_mut(guest_phys_addr) {
            Valid(valid_leaf) => valid_leaf.take_page().map(UnmappedPage::Page),
            _ => None,
        }
    }

    fn invalidate_page(&mut self, guest_phys_addr: u64) -> Option<UnmappedPage> {
        use TableEntryMut::*;
        use ValidTableEntryMut::*;
        let mut l4 = self.top_level_directory();
        let mut l3 = match l4.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(valid_leaf) => return valid_leaf.invalidate_page().map(UnmappedPage::Tera),
        };
        let mut l2 = match l3.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(valid_leaf) => return valid_leaf.invalidate_page().map(UnmappedPage::Giga),
        };
        let mut l1 = match l2.entry_for_addr_mut(guest_phys_addr) {
            Invalid(_) => return None,
            Valid(Table(t)) => t,
            Valid(valid_leaf) => return valid_leaf.invalidate_page().map(UnmappedPage::Mega),
        };
        match l1.entry_for_addr_mut(guest_phys_addr) {
            Valid(valid_leaf) => valid_leaf.invalidate_page().map(UnmappedPage::Page),
            _ => None,
        }
    }

    fn unmap_range<S: PageSize>(
        &mut self,
        addr: PageAddr<S>,
        num_pages: u64,
    ) -> Option<UnmapIter<Self>> {
        if addr
            .iter_from()
            .take(num_pages as usize)
            .all(|a| self.addr_mapped(a.bits()))
        {
            Some(UnmapIter::new(self, addr.bits(), num_pages, S::SIZE_BYTES))
        } else {
            None
        }
    }

    fn invalidate_range<S: PageSize>(
        &mut self,
        addr: PageAddr<S>,
        num_pages: u64,
    ) -> Option<InvalidateIter<Self>> {
        if addr
            .iter_from()
            .take(num_pages as usize)
            .all(|a| self.addr_mapped(a.bits()))
        {
            Some(InvalidateIter::new(
                self,
                addr.bits(),
                num_pages,
                S::SIZE_BYTES,
            ))
        } else {
            None
        }
    }

    fn get_root_address(&self) -> PageAddr4k {
        self.root.start_page_addr()
    }

    fn do_guest_fault(&mut self, guest_phys_addr: u64) -> bool {
        use TableEntryMut::*;
        use ValidTableEntryMut::*;

        // avoid double self borrow, by cloning the pages, each layer borrows self, so the borrow
        // checked can't tell that phys_pages is only borrowed once.
        let mut phys_pages = self.phys_pages.clone();
        let owner = self.owner;
        let mut l4 = self.top_level_directory();
        let mut l3 = match l4.entry_for_addr_mut(guest_phys_addr) {
            Invalid(pte) => {
                return Self::handle_fault_at::<PageSize512GB>(pte, &mut phys_pages, &owner);
            }
            Valid(Table(t)) => t,
            Valid(Leaf(_)) => {
                return false;
            }
        };
        let mut l2 = match l3.entry_for_addr_mut(guest_phys_addr) {
            Invalid(pte) => {
                return Self::handle_fault_at::<PageSize1GB>(pte, &mut phys_pages, &owner);
            }
            Valid(Table(t)) => t,
            Valid(Leaf(_)) => {
                return false;
            }
        };
        let mut l1 = match l2.entry_for_addr_mut(guest_phys_addr) {
            Invalid(pte) => {
                return Self::handle_fault_at::<PageSize2MB>(pte, &mut phys_pages, &owner);
            }
            Valid(Table(t)) => t,
            Valid(Leaf(_)) => {
                return false;
            }
        };
        match l1.entry_for_addr_mut(guest_phys_addr) {
            Invalid(pte) => Self::handle_fault_at::<PageSize4k>(pte, &mut phys_pages, &owner),
            _ => false,
        }
    }
}