use rmm::{
    KILOBYTE,
    MEGABYTE,
    GIGABYTE,
    TERABYTE,
    Arch,
    EmulateArch,
    MemoryArea,
    PageEntry,
    PageTable,
    PhysicalAddress,
    VirtualAddress,
};

use core::marker::PhantomData;

pub fn format_size(size: usize) -> String {
    if size >= 2 * TERABYTE {
        format!("{} TB", size / TERABYTE)
    } else if size >= 2 * GIGABYTE {
        format!("{} GB", size / GIGABYTE)
    } else if size >= 2 * MEGABYTE {
        format!("{} MB", size / MEGABYTE)
    } else if size >= 2 * KILOBYTE {
        format!("{} KB", size / KILOBYTE)
    } else {
        format!("{} B", size)
    }
}

unsafe fn dump_tables<A: Arch>(table: PageTable<A>) {
    let level = table.level();
    for i in 0..A::PAGE_ENTRIES {
        if level == 0 {
            if let Some(entry) = table.entry(i) {
                if entry.present() {
                    let base = table.entry_base(i).unwrap();
                    println!("0x{:X}: 0x{:X}", base.data(), entry.address().data());
                }
            }
        } else {
            if let Some(next) = table.next(i) {
                dump_tables(next);
            }
        }
    }
}

pub struct BumpAllocator<A> {
    areas: &'static [MemoryArea],
    offset: usize,
    phantom: PhantomData<A>,
}

impl<A: Arch> BumpAllocator<A> {
    pub fn new(areas: &'static [MemoryArea], offset: usize) -> Self {
        Self {
            areas,
            offset,
            phantom: PhantomData,
        }
    }

    pub fn allocate(&mut self) -> Option<PhysicalAddress> {
        let mut offset = self.offset;
        for area in self.areas.iter() {
            if offset < area.size {
                self.offset += A::PAGE_SIZE;
                return Some(area.base.add(offset));
            }
            offset -= area.size;
        }
        None
    }
}

pub struct SlabNode<A> {
    next: PhysicalAddress,
    count: usize,
    phantom: PhantomData<A>,
}

impl<A: Arch> SlabNode<A> {
    pub fn new(next: PhysicalAddress, count: usize) -> Self {
        Self {
            next,
            count,
            phantom: PhantomData,
        }
    }

    pub fn empty() -> Self {
        Self::new(PhysicalAddress::new(0), 0)
    }

    pub unsafe fn insert(&mut self, phys: PhysicalAddress) {
        let virt = A::phys_to_virt(phys);
        A::write(virt, self.next);
        self.next = phys;
        self.count += 1;
    }

    pub unsafe fn remove(&mut self) -> Option<PhysicalAddress> {
        if self.count > 0 {
            let phys = self.next;
            let virt = A::phys_to_virt(phys);
            self.next = A::read(virt);
            self.count -= 1;
            Some(phys)
        } else {
            None
        }
    }
}

pub struct SlabAllocator<A> {
    //TODO: Allow allocations up to maximum pageable size
    nodes: [SlabNode<A>; 4],
    phantom: PhantomData<A>,
}

impl<A: Arch> SlabAllocator<A> {
    pub unsafe fn new(areas: &'static [MemoryArea], offset: usize) -> Self {
        let mut allocator = Self {
            nodes: [
                SlabNode::empty(),
                SlabNode::empty(),
                SlabNode::empty(),
                SlabNode::empty(),
            ],
            phantom: PhantomData,
        };

        // Add unused areas to free lists
        let mut area_offset = offset;
        for area in areas.iter() {
            if area_offset < area.size {
                area_offset = 0;
                let area_base = area.base.add(offset);
                let area_size = area.size - offset;
                allocator.free(area_base, area_size);
            } else {
                area_offset -= area.size;
            }
        }

        allocator
    }

    pub unsafe fn allocate(&mut self, size: usize) -> Option<PhysicalAddress> {
        for level in 0..A::PAGE_LEVELS - 1 {
            let level_shift = level * A::PAGE_ENTRY_SHIFT + A::PAGE_SHIFT;
            let level_size = 1 << level_shift;
            if size <= level_size {
                if let Some(base) = self.nodes[level].remove() {
                    self.free(base.add(size), level_size - size);
                    return Some(base);
                }
            }
        }
        None
    }

    //TODO: This causes fragmentation, since neighbors are not identified
    //TODO: remainders less than PAGE_SIZE will be lost
    pub unsafe fn free(&mut self, mut base: PhysicalAddress, mut size: usize) {
        for level in (0..A::PAGE_LEVELS - 1).rev() {
            let level_shift = level * A::PAGE_ENTRY_SHIFT + A::PAGE_SHIFT;
            let level_size = 1 << level_shift;
            while size >= level_size {
                println!("Add {:X} {}", base.data(), format_size(level_size));
                self.nodes[level].insert(base);
                base = base.add(level_size);
                size -= level_size;
            }
        }
    }

    pub unsafe fn remaining(&mut self) -> usize {
        let mut remaining = 0;
        for level in (0..A::PAGE_LEVELS - 1).rev() {
            let level_shift = level * A::PAGE_ENTRY_SHIFT + A::PAGE_SHIFT;
            let level_size = 1 << level_shift;
            remaining += self.nodes[level].count * level_size;
        }
        remaining
    }
}

pub struct Mapper<A> {
    table_addr: PhysicalAddress,
    allocator: BumpAllocator<A>,
}

impl<A: Arch> Mapper<A> {
    pub unsafe fn new(mut allocator: BumpAllocator<A>) -> Option<Self> {
        let table_addr = allocator.allocate()?;
        Some(Self {
            table_addr,
            allocator,
        })
    }

    pub unsafe fn map(&mut self, virt: VirtualAddress, entry: PageEntry<A>) -> Option<()> {
        let mut table = PageTable::new(
            VirtualAddress::new(0),
            self.table_addr,
            A::PAGE_LEVELS - 1
        );
        loop {
            let i = table.index_of(virt)?;
            if table.level() == 0 {
                //TODO: check for overwriting entry
                table.set_entry(i, entry);
                return Some(());
            } else {
                let next_opt = table.next(i);
                let next = match next_opt {
                    Some(some) => some,
                    None => {
                        let phys = self.allocator.allocate()?;
                        //TODO: correct flags?
                        table.set_entry(i, PageEntry::new(phys.data() | A::ENTRY_FLAG_WRITABLE | A::ENTRY_FLAG_PRESENT));
                        table.next(i)?
                    }
                };
                table = next;
            }
        }
    }
}

unsafe fn new_tables<A: Arch>(areas: &'static [MemoryArea]) {
    // First, calculate how much memory we have
    let mut size = 0;
    for area in areas.iter() {
        size += area.size;
    }

    println!("Memory: {}", format_size(size));

    // Create a basic allocator for the first pages
    let allocator = BumpAllocator::<A>::new(areas, 0);

    // Map all physical areas at PHYS_OFFSET
    let mut mapper = Mapper::new(allocator).expect("failed to create Mapper");
    for area in areas.iter() {
        for i in 0..area.size / A::PAGE_SIZE {
            let phys = area.base.add(i * A::PAGE_SIZE);
            let virt = A::phys_to_virt(phys);
            mapper.map(
                virt,
                PageEntry::new(phys.data() | A::ENTRY_FLAG_WRITABLE | A::ENTRY_FLAG_PRESENT)
            ).expect("failed to map frame");
        }
    }

    // Use the new table
    A::set_table(mapper.table_addr);

    // Create the physical memory map
    let offset = mapper.allocator.offset;
    println!("Permanently used: {}", format_size(offset));

    let mut allocator = SlabAllocator::<A>::new(areas, offset);
    for i in 0..16 {
        let phys_opt = allocator.allocate(4 * KILOBYTE);
        println!("4 KB page {}: {:X?}", i, phys_opt);
        if i % 2 == 0 {
            if let Some(phys) = phys_opt {
                allocator.free(phys, 4 * KILOBYTE);
            }
        }
    }
    for i in 0..16 {
        let phys_opt = allocator.allocate(2 * MEGABYTE);
        println!("2 MB page {}: {:X?}", i, phys_opt);
        if i % 2 == 0 {
            if let Some(phys) = phys_opt {
                allocator.free(phys, 2 * MEGABYTE);
            }
        }
    }

    println!("Remaining: {}", format_size(allocator.remaining()));
}

unsafe fn inner<A: Arch>() {
    let areas = A::init();

    // Debug table
    //dump_tables(PageTable::<A>::top());

    new_tables::<A>(areas);

    //dump_tables(PageTable::<A>::top());


    for i in &[1, 2, 4, 8, 16, 32] {
        let phys = PhysicalAddress::new(i * MEGABYTE);
        let virt = A::phys_to_virt(phys);

        // Test read
        println!("0x{:X} (0x{:X}) = 0x{:X}", virt.data(), phys.data(), A::read::<u8>(virt));

        // Test write
        A::write::<u8>(virt, 0x5A);

        // Test read
        println!("0x{:X} (0x{:X}) = 0x{:X}", virt.data(), phys.data(), A::read::<u8>(virt));
    }
}

fn main() {
    unsafe {
        inner::<EmulateArch>();
    }
}
