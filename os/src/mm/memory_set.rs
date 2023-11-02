//! Implementation of [`MapArea`] and [`MemorySet`].

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{
    KERNEL_STACK_SIZE, MEMORY_END, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_STACK_SIZE,
};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::asm;
use lazy_static::*;
use riscv::register::satp;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn ekernel();
    fn strampoline();
}

lazy_static! {
    /// The kernel's initial memory mapping(kernel address space)
    /// 实例化一个内核的地址空间：
    pub static ref KERNEL_SPACE: Arc<UPSafeCell<MemorySet>> =
        Arc::new(unsafe { UPSafeCell::new(MemorySet::new_kernel()) });
}
/// address space
/// 地址空间与进程是一一对应的
pub struct MemorySet {
    page_table: PageTable, //一个页表
    areas: Vec<MapArea>, //一系列逻辑段
    mmap_frames: BTreeMap<VirtPageNum, FrameTracker>, //vpn和ppn的映射
}

impl MemorySet {
    /// Create a new empty `MemorySet`.
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
            mmap_frames: BTreeMap::new(),
        }
    }
    /// Get the page table token
    pub fn token(&self) -> usize {   //获取内置页表的satp
        self.page_table.token()
    }
    /// Assume that no conflicts.
    /// MemorySet中根据参数插入一个没有存放数据的Frame类型的MapArea。
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }
    //push方法将data写入map_area中，再将map_area插入MemorySet中
    fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) {
        map_area.map(&mut self.page_table);  //将map_area与page_table绑定并分配物理页帧
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data);
        }
        self.areas.push(map_area);
    }
    /// Mention that trampoline is not collected by areas.
    /// strampoline对应了__alltraps所对应的物理页的起始地址
    /// 在内置页表中将虚拟地址TRAMPOLINE所对应的虚拟页映射到__alltraps所对应的页。
    fn map_trampoline(&mut self) {
        self.page_table.map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        );
    }
    /// Without kernel stacks.
    /// 内核载入时映射地址空间的函数：
    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map kernel sections
        info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        info!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );
        info!("mapping .text section");
        memory_set.push(
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
            ),
            None,
        );
        info!("mapping .rodata section");
        memory_set.push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
            ),
            None,
        );
        info!("mapping .data section");
        memory_set.push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping .bss section");
        memory_set.push(
            MapArea::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping physical memory");
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                MEMORY_END.into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        memory_set
    }
    /// Include sections in elf and trampoline and TrapContext and user stack,
    /// also returns user_sp_base and entry point.
    /// 载入elf文件的用户态程序的地址空间建立过程：
    /// 返回了一个三元组(进程的MemorySet，用户栈地址，入口地址)
    pub fn from_elf(elf_data: &[u8]) -> (Self, usize, usize) {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map program headers of elf, with U flag
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        // 应用程序在链接的时候就已经确定了每个数据的虚拟地址，在载入系统的时候，数据在程序中的虚
        // 拟地址和在虚拟内存中的虚拟地址是一致的，这样才能够保证程序在进入虚拟内存系
        // 统后依然可以正常的运行。注意flags的设置，一定是MapPermission::U的，这样才能在用户态下执行。
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                max_end_vpn = map_area.vpn_range.get_end();
                memory_set.push(
                    map_area,
                    Some(&elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
                );
            }
        }
        // map user stack with U flags
        // max_end_vpn是用户程序部分所占用的最后一个虚拟页，我们从它后面的一个页开始用作用户栈，
        // 需要设置MapPermission::R | MapPermission::W ||MapPermission::U
        let max_end_va: VirtAddr = max_end_vpn.into();
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        user_stack_bottom += PAGE_SIZE;
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // used in sbrk
        memory_set.push(
            MapArea::new(
                user_stack_top.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // map TrapContext
        // 这一步是预留了TRAMPOLINE前面的一个虚拟页来放置TRAP_CONTEXT
        memory_set.push(
            MapArea::new(
                TRAP_CONTEXT_BASE.into(),
                TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        (
            memory_set,
            user_stack_top,
            elf.header.pt2.entry_point() as usize,
        )
    }
    /// Change page table by writing satp CSR Register.
    pub fn activate(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(satp); //切换页表
            asm!("sfence.vma"); //刷新TLB
        }
    }
    /// Translate a virtual page number to a page table entry
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }
    /// shrink the area to new_end
    #[allow(unused)]
    pub fn shrink_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            area.shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// append the area to new_end
    #[allow(unused)]
    pub fn append_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            area.append_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// mmap
    pub fn mmap(&mut self, start_vpn: VirtPageNum, end_vpn: VirtPageNum, port: usize) -> isize {
        let mut flags = PTEFlags::empty();
        let mut vpn = start_vpn;

        if port & 0b0000_0001 != 0 {
            flags |= PTEFlags::R;
        }

        if port & 0b0000_0010 != 0 {
            flags |= PTEFlags::W;
        }

        if port & 0b0000_0100 != 0 {
            flags |= PTEFlags::X;
        }

        flags |= PTEFlags::U;
        flags |= PTEFlags::V;

        while vpn != end_vpn {
            if let Some(pte) = self.page_table.translate(vpn) {
                debug!("find vpn {:?} pte flag = {:?}", vpn, pte.flags());
                if pte.is_valid() {
                    debug!("map on already mapped vpn {:?}", vpn);
                    return -1;
                }
            }
            if let Some(frame) = frame_alloc() {
                let ppn = frame.ppn;
                debug!(" map vpn {:?} and ppn {:?} flag {:?}", vpn, ppn, flags);
                self.page_table.map(vpn, ppn, flags);
                self.mmap_frames.insert(vpn, frame);
            } else {
                return -1;
            }
            vpn.step();
        }

        0
    }

    /// mmunmap
    pub fn munmap(&mut self, start_vpn: VirtPageNum, end_vpn: VirtPageNum) -> isize {
        let mut vpn = start_vpn;
        while vpn != end_vpn {
            if let Some(pte) = self.page_table.translate(vpn) {
                if !pte.is_valid() {
                    debug!("unmap on no map vpn");
                    return -1;
                }
            } else {
                return -1;
            }
            self.page_table.unmap(vpn);
            self.mmap_frames.remove(&vpn);
            vpn.step();
        }
        0
    }
}
/// map area structure, controls a contiguous piece of virtual memory
/// 逻辑段的结构与方法
pub struct MapArea {
    vpn_range: VPNRange, // 迭代器，元素为所有的虚拟页
    data_frames: BTreeMap<VirtPageNum, FrameTracker>, //记录映射关系
    map_type: MapType,
    map_perm: MapPermission, // 逻辑段的访问权限
}

impl MapArea {
    pub fn new( //创建一个新的MapArea
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
        }
    }
    ////为MapArea中的一个虚拟页映射物理页，若为恒等映射，则直接映射到vpn同号的ppn，否则
    /// 重新分配一个物理页帧使之映射。映射完成后需要在页表中也完成映射操作。
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        match self.map_type {
            MapType::Identical => { //恒等映射
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Framed => { 
                let frame = frame_alloc().unwrap();
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();
        page_table.map(vpn, ppn, pte_flags);
    }
    //解除映射
    #[allow(unused)]
    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            self.data_frames.remove(&vpn);
        }
        page_table.unmap(vpn);
    }
    //完成对MapArea中所有虚拟页的映射
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }
    //解除对MapArea中所有虚拟页的映射
    #[allow(unused)]
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }
    #[allow(unused)]
    pub fn shrink_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(new_end, self.vpn_range.get_end()) {
            self.unmap_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }
    #[allow(unused)]
    pub fn append_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(self.vpn_range.get_end(), new_end) {
            self.map_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }
    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    /// 实现在MapArea中存入数组数据的方法：
    /// 函数将数据放入范围内的每个页中，一个页存满了则进入下一个页存放，
    /// 直到全部数据存放完毕或者MapArea中的虚拟页全部被用完。
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8]) {
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();
        loop {
            let src = &data[start..len.min(start + PAGE_SIZE)];
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[..src.len()];
            dst.copy_from_slice(src);
            start += PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn.step();
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
/// map type for memory set: identical or framed
pub enum MapType {  
    Identical,  //表示对等映射
    Framed,  //表示对于每个虚拟页面都需要映射到一个新分配的物理页帧
}

bitflags! {
    /// map permission corresponding to that in pte: `R W X U`
    pub struct MapPermission: u8 { //与PTEflags是同样的结构
        ///Readable
        const R = 1 << 1;
        ///Writable
        const W = 1 << 2;
        ///Excutable
        const X = 1 << 3;
        ///Accessible in U mode
        const U = 1 << 4;
    }
}

/// Return (bottom, top) of a kernel stack in kernel space.
pub fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    let top = TRAMPOLINE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

/// remap test in kernel space
#[allow(unused)]
pub fn remap_test() {
    let mut kernel_space = KERNEL_SPACE.exclusive_access();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable(),);
    println!("remap_test passed!");
}
