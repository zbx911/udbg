
use super::*;

use core::time::Duration;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::mem::{transmute, size_of_val};
use std::ptr::{null, null_mut};
use std::ops::Deref;
use std::cell::UnsafeCell;
use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::io::{Error as IoErr, Result as IoRes};

use winapi::um::processthreadsapi::*;
use winapi::um::winbase::INFINITE;
use winapi::um::minwinbase::*;
use winapi::um::debugapi::*;
use winapi::um::handleapi::*;
use ntapi::FIELD_OFFSET;

use winapi::shared::ntstatus::*;
const EXCEPTION_WX86_BREAKPOINT: u32 = STATUS_WX86_BREAKPOINT as u32;
const EXCEPTION_WX86_SINGLE_STEP: u32 = STATUS_WX86_SINGLE_STEP as u32;

use serde_value::Value as SerdeVal;
use ntapi::ntpebteb::{TEB, PEB};
use ntapi::ntexapi::SYSTEM_THREAD_INFORMATION;
use crossbeam::atomic::AtomicCell;

use super::pdbfile::*;
use crate::ntdll::*;

#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

pub trait DbgReg {}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq)]
pub enum HandleResult {
    Continue = winapi::um::winnt::DBG_CONTINUE,
    Handled = winapi::um::winnt::DBG_EXCEPTION_HANDLED,
    NotHandled = winapi::um::winnt::DBG_EXCEPTION_NOT_HANDLED,
}

// https://docs.microsoft.com/en-us/windows-hardware/drivers/debugger/specific-exceptions
// pub const DBG_PRINTEXCEPTION_C: u32 = 0x40010006;
// pub const DBG_PRINTEXCEPTION_WIDE_C: u32 = 0x4001000A;

macro_rules! set_bit {
    ($n:expr, $x:expr, $set:expr) => {
        if $set { $n |= 1 << $x; } else { $n &= !(1 << $x); }
    }
}

macro_rules! test_bit {
    ($n:expr, $x:expr) => { ($n & (1 << $x) > 0) }
}

macro_rules! set_bit2 {
    ($n:expr, $x:expr, $v:expr) => {
        $n &= !(0b11 << $x);
        $n |= $v << $x;
    };
}

const L0: usize = 0;
const G0: usize = 1;
const L1: usize = 2;
const G1: usize = 3;
const L2: usize = 4;
const G2: usize = 5;
const L3: usize = 5;
const G3: usize = 7;
const L_ENABLE: usize = 8;
const G_ENABLE: usize = 9;
const RW0: usize = 16;
const LEN0: usize = 18;
const RW1: usize = 20;
const LEN1: usize = 22;
const RW2: usize = 24;
const LEN2: usize = 26;
const RW3: usize = 28;
const LEN3: usize = 30;

pub trait HWBPRegs: AbstractRegs {
    fn set_step(&mut self, step: bool);
    fn set_rf(&mut self);
    fn test_eflags(&self, flag: u32) -> bool;

    fn l_enable(&mut self, enable: bool);
    fn set_local(&mut self, idx: usize, set: bool);
    fn set_rw(&mut self, idx: usize, val: u8);
    fn set_len(&mut self, idx: usize, val: u8);

    fn empty(&self) -> bool;

    fn set_bp(&mut self, address: usize, idx: usize, rw: u8, len: u8);
    fn unset_bp(&mut self, idx: usize);

    fn dr6(&self) -> usize;
}

#[cfg(target_arch="x86_64")]
impl HWBPRegs for CONTEXT {
    fn set_step(&mut self, step: bool) {
        let flags = self.EFlags;
        self.EFlags = if step { flags | EFLAGS_TF } else { flags & (!EFLAGS_TF) };
    }

    fn set_rf(&mut self) { self.EFlags |= EFLAGS_RF; }

    fn test_eflags(&self, flag: u32) -> bool {
        self.EFlags & flag > 0
    }

    fn l_enable(&mut self, enable: bool) {
        set_bit!(self.Dr7, L_ENABLE, enable);
    }

    fn set_local(&mut self, idx: usize, set: bool) {
        let x = match idx {
            0 => L0,
            1 => L1,
            2 => L2,
            _ => L3,
        };
        set_bit!(self.Dr7, x, set);
    }

    fn set_rw(&mut self, idx: usize, val: u8) {
        let x = match idx {
            0 => RW0,
            1 => RW1,
            2 => RW2,
            _ => RW3,
        };
        set_bit2!(self.Dr7, x, (val as Self::REG));
    }

    fn set_len(&mut self, idx: usize, val: u8) {
        let x = match idx {
            0 => LEN0,
            1 => LEN1,
            2 => LEN2,
            _ => LEN3,
        };
        set_bit2!(self.Dr7, x, (val as Self::REG));
    }

    fn empty(&self) -> bool {
        let n = self.Dr7;
        !test_bit!(n, L0) && !test_bit!(n, L1) && !test_bit!(n, L2) && !test_bit!(n, L3)
    }

    fn set_bp(&mut self, address: usize, idx: usize, rw: u8, len: u8) {
        self.l_enable(true);
        self.set_local(idx, true);
        self.set_rw(idx, rw);
        self.set_len(idx, len);
        let address = address as Self::REG;
        match idx {
            0 => self.Dr0 = address,
            1 => self.Dr1 = address,
            2 => self.Dr2 = address,
            _ => self.Dr3 = address,
        };
    }

    fn unset_bp(&mut self, idx: usize) {
        self.set_local(idx, false);
        self.set_rw(idx, 0);
        self.set_len(idx, 0);
        match idx {
            0 => self.Dr0 = 0,
            1 => self.Dr1 = 0,
            2 => self.Dr2 = 0,
            _ => self.Dr3 = 0,
        };
        if self.empty() { self.l_enable(false); }
    }

    #[inline(always)]
    fn dr6(&self) -> usize { self.Dr6 as usize }
}

impl HWBPRegs for CONTEXT32 {
    fn set_step(&mut self, step: bool) {
        let flags = self.EFlags;
        self.EFlags = if step { flags | EFLAGS_TF } else { flags & (!EFLAGS_TF) };
    }

    fn set_rf(&mut self) { self.EFlags |= EFLAGS_RF; }

    fn test_eflags(&self, flag: u32) -> bool {
        self.EFlags & flag > 0
    }

    fn l_enable(&mut self, enable: bool) {
        set_bit!(self.Dr7, L_ENABLE, enable);
    }

    fn set_local(&mut self, idx: usize, set: bool) {
        let x = match idx {
            0 => L0,
            1 => L1,
            2 => L2,
            _ => L3,
        };
        set_bit!(self.Dr7, x, set);
    }

    fn set_rw(&mut self, idx: usize, val: u8) {
        let x = match idx {
            0 => RW0,
            1 => RW1,
            2 => RW2,
            _ => RW3,
        };
        set_bit2!(self.Dr7, x, (val as Self::REG));
    }

    fn set_len(&mut self, idx: usize, val: u8) {
        let x = match idx {
            0 => LEN0,
            1 => LEN1,
            2 => LEN2,
            _ => LEN3,
        };
        set_bit2!(self.Dr7, x, (val as Self::REG));
    }

    fn empty(&self) -> bool {
        let n = self.Dr7;
        !test_bit!(n, L0) && !test_bit!(n, L1) && !test_bit!(n, L2) && !test_bit!(n, L3)
    }

    fn set_bp(&mut self, address: usize, idx: usize, rw: u8, len: u8) {
        self.l_enable(true);
        self.set_local(idx, true);
        self.set_rw(idx, rw);
        self.set_len(idx, len);
        let address = address as Self::REG;
        match idx {
            0 => self.Dr0 = address,
            1 => self.Dr1 = address,
            2 => self.Dr2 = address,
            _ => self.Dr3 = address,
        };
    }

    fn unset_bp(&mut self, idx: usize) {
        self.set_local(idx, false);
        self.set_rw(idx, 0);
        self.set_len(idx, 0);
        match idx {
            0 => self.Dr0 = 0,
            1 => self.Dr1 = 0,
            2 => self.Dr2 = 0,
            _ => self.Dr3 = 0,
        };
        if self.empty() { self.l_enable(false); }
    }

    #[inline(always)]
    fn dr6(&self) -> usize { self.Dr6 as usize }
}

// TODO
#[cfg(target_arch = "aarch64")]
impl HWBPRegs for CONTEXT {
    fn set_step(&mut self, step: bool) {}
    fn set_rf(&mut self) {}
    fn test_eflags(&self, flag: u32) -> bool { false }

    fn l_enable(&mut self, enable: bool) {}
    fn set_local(&mut self, idx: usize, set: bool) {}
    fn set_rw(&mut self, idx: usize, val: u8) {}
    fn set_len(&mut self, idx: usize, val: u8) {}

    fn empty(&self) -> bool {false}

    fn set_bp(&mut self, address: usize, idx: usize, rw: u8, len: u8) {}
    fn unset_bp(&mut self, idx: usize) {}

    fn dr6(&self) -> usize {0}
}

pub trait DbgContext: HWBPRegs {
    const IS_32: bool = false;

    fn get_context(&mut self, t: HANDLE) -> bool;
    fn set_context(&self, t: HANDLE) -> bool;
}

impl DbgContext for CONTEXT {
    #[inline(always)]
    fn get_context(&mut self, t: HANDLE) -> bool {
        self.ContextFlags = CONTEXT_ALL;
        unsafe { GetThreadContext(t, self) > 0 }
    }

    #[inline(always)]
    fn set_context(&self, t: HANDLE) -> bool {
        unsafe { SetThreadContext(t, self) > 0 }
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl DbgContext for CONTEXT32 {
    const IS_32: bool = true;

    #[inline(always)]
    fn get_context(&mut self, t: HANDLE) -> bool {
        self.ContextFlags = CONTEXT_ALL;
        unsafe { Wow64GetThreadContext(t, self) > 0 }
    }

    #[inline(always)]
    fn set_context(&self, t: HANDLE) -> bool {
        unsafe { Wow64SetThreadContext(t, self) > 0 }
    }
}

pub fn get_thread_handle_context<C: DbgContext>(handle: &Handle, c: &mut C, flags: u32) -> bool {
    unsafe {
        SuspendThread(*handle.deref());
        let r = c.get_context(*handle.deref());
        ResumeThread(*handle.deref());
        return r;
    }
}

#[inline(always)]
pub fn get_thread_context<C: DbgContext>(tid: u32, c: &mut C, flags: u32) -> bool {
    let handle = open_thread(tid, THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, false);
    get_thread_handle_context(&handle, c, flags)
}

pub fn set_thread_context<C: DbgContext>(tid: u32, c: &C) -> bool {
    let handle = open_thread(tid, THREAD_SUSPEND_RESUME | THREAD_SET_CONTEXT, false);
    unsafe {
        SuspendThread(*handle);
        let r = c.set_context(*handle);
        ResumeThread(*handle);
        return r;
    }
}

pub struct DbgThread {
    pub tid: u32,
    pub handle: HANDLE,
    pub local_base: usize,
    pub start_address: usize,
}

impl DbgThread {
    pub fn new(thread: HANDLE, local_base: usize, start_address: usize) -> Self {
        unsafe {
            DbgThread { handle: thread, tid: GetThreadId(thread), local_base, start_address }
        }
    }
}

impl From<&CREATE_THREAD_DEBUG_INFO> for DbgThread {
    fn from(info: &CREATE_THREAD_DEBUG_INFO) -> DbgThread {
        DbgThread::new(info.hThread,
            info.lpThreadLocalBase as usize,
            unsafe { transmute(info.lpStartAddress) })
    }
}

impl From<&CREATE_PROCESS_DEBUG_INFO> for DbgThread {
    fn from(info: &CREATE_PROCESS_DEBUG_INFO) -> DbgThread {
        DbgThread::new(info.hThread,
            info.lpThreadLocalBase as usize,
            unsafe { transmute(info.lpStartAddress) })
    }
}

#[derive(Deref)]
pub struct WinThread {
    #[deref] base: ThreadData,
    pub teb: AtomicCell<usize>,
    pub process: *const Process,
    pub infos: Arc<HashMap<u32, SYSTEM_THREAD_INFORMATION>>,
}

impl WinThread {
    pub fn new(tid: tid_t) -> Option<Self> {
        Some(WinThread {
            base: ThreadData {
                wow64: false,
                tid,
                handle: open_thread(tid, THREAD_ALL_ACCESS, false),
            },
            process: null(),
            teb: AtomicCell::new(0),
            infos: Arc::new(HashMap::new())
        })
    }

    fn get_reg(&self, r: &str) -> UDbgResult<CpuReg> {
        if self.wow64 {
            let mut cx = Align16::<CONTEXT32>::new();
            let context = cx.as_mut();
            context.get_context(*self.handle);
            context.get(r).ok_or(UDbgError::InvalidRegister)
        } else {
            let mut cx = Align16::<CONTEXT>::new();
            let context = cx.as_mut();
            context.get_context(*self.handle);
            context.get(r).ok_or(UDbgError::InvalidRegister)
        }
    }
}

static mut GetThreadDescription: Option<extern "system" fn(HANDLE, *mut PWSTR)->HRESULT> = None;

#[ctor]
unsafe fn foo() {
    use winapi::um::libloaderapi::*;

    GetThreadDescription = transmute(GetProcAddress(
        GetModuleHandleA(cstr!("kernelbase").as_ptr().cast()),
        cstr!("GetThreadDescription").as_ptr().cast()
    ));
}

impl GetProp for WinThread {
    fn get_prop(&self, key: &str) -> UDbgResult<serde_value::Value> {
        if let Some(reg) = key.strip_prefix("@") {
            Ok(serde_value::to_value(self.get_reg(reg)?).unwrap())
        } else {
            Err(UDbgError::NotSupport)
        }
    }
}

impl UDbgThread for WinThread {
    fn name(&self) -> Arc<str> {
        unsafe {
            GetThreadDescription.map(|get| {
                let mut s = null_mut();
                get(*self.handle, &mut s);
                let result = String::from_wide_ptr(s);
                LocalFree(s as _);
                result
            }).unwrap_or_default().into()
        }
    }

    fn status(&self) -> Arc<str> {
        self.infos.get(&self.tid).map(|t| {
            t.status()
        }).unwrap_or(String::new()).into()
    }

    fn priority(&self) -> Option<i32> {
        use winapi::um::winbase::*;
        unsafe {
            let mut p = if self.handle.is_null() {
                GetThreadPriority(*open_thread(self.tid, THREAD_QUERY_INFORMATION, false))
            } else {
                GetThreadPriority(*self.handle)
            };
            if p == THREAD_PRIORITY_ERROR_RETURN as i32 {
                self.infos.get(&self.tid).map(|t| p = t.Priority);
            }
            Some(p)
        }
    }

    fn teb(&self) -> Option<usize> {
        let mut teb = self.teb.load();
        if teb == 0 {
            teb = if self.handle.is_null() {
                let h = open_thread(self.tid, THREAD_QUERY_INFORMATION, false);
                query_thread::<THREAD_BASIC_INFORMATION>(*h, ThreadInfoClass::BasicInformation, None)
            } else {
                query_thread::<THREAD_BASIC_INFORMATION>(*self.handle, ThreadInfoClass::BasicInformation, None)
            }.map(|t| t.TebBaseAddress as usize).unwrap_or(0);
            self.teb.store(teb);
        }
        if teb > 0 { teb.into() } else { None }
    }

    fn entry(&self) -> usize {
        if self.handle.is_null() {
            let h = open_thread(self.tid, THREAD_QUERY_INFORMATION, false);
            query_thread::<usize>(*h, ThreadInfoClass::QuerySetWin32StartAddress, None)
        } else {
            query_thread::<usize>(*self.handle, ThreadInfoClass::QuerySetWin32StartAddress, None)
        }.or_else(|| self.infos.get(&self.tid).map(|t| t.StartAddress as usize)).unwrap_or(0)
    }

    fn suspend(&self) -> IoRes<i32> {
        unsafe {
            Ok(if self.wow64 {
                Wow64SuspendThread(*self.handle)
            } else {
                SuspendThread(*self.handle)
            } as i32)
        }
    }
    fn resume(&self) -> IoRes<u32> {
        unsafe {
            Ok(ResumeThread(*self.handle))
        }
    }
    fn get_context(&self, cx: &mut ThreadContext) -> IoRes<()> {
        if cx.get_context(*self.handle) {
            Ok(())
        } else {
            Err(IoErr::last_os_error())
        }
    }
    fn set_context(&self, cx: &ThreadContext) -> IoRes<()> {
        if cx.set_context(*self.handle) {
            Ok(())
        } else {
            Err(IoErr::last_os_error())
        }
    }
    fn get_context32(&self, cx: &mut ThreadContext32) -> IoRes<()> {
        if cx.get_context(*self.handle) {
            Ok(())
        } else {
            Err(IoErr::last_os_error())
        }
    }
    fn set_context32(&self, cx: &ThreadContext32) -> IoRes<()> {
        if cx.set_context(*self.handle) {
            Ok(())
        } else {
            Err(IoErr::last_os_error())
        }
    }

    fn last_error(&self) -> Option<u32> {
        self.teb().and_then(|teb| unsafe {
            use ntapi::ntpebteb::TEB;
            self.process.as_ref()?.read_value::<u32>(teb + ntapi::FIELD_OFFSET!(TEB, LastErrorValue))
        })
    }
}

pub fn get_selector_entry(th: HANDLE, s: u32) -> usize {
    unsafe {
        let mut ldt: LDT_ENTRY = core::mem::zeroed();
        let r = GetThreadSelectorEntry(th, s, transmute(&mut ldt));
        ldt.BaseLow as usize | ((ldt.HighWord.Bits_mut().BaseMid() as usize) << 16) | ((ldt.HighWord.Bits_mut().BaseHi() as usize) << 24)
    }
}

pub fn get_selector_entry_wow64(th: HANDLE, s: u32) -> u32 {
    unsafe {
        let mut ldt: WOW64_LDT_ENTRY = core::mem::zeroed();
        let r = Wow64GetThreadSelectorEntry(th, s, &mut ldt);
        ldt.BaseLow as u32 | ((ldt.HighWord.Bits_mut().BaseMid() as u32) << 16) | ((ldt.HighWord.Bits_mut().BaseHi() as u32) << 24)
    }
}

#[inline]
fn map_or_open(file: HANDLE, path: &str) -> Option<memmap::Mmap> {
    if file.is_null() {
        crate::util::mapfile(path)
    } else {
        unsafe {
            let f = File::from_raw_handle(file);
            memmap::Mmap::map(&f).ok()
        }
    }
}

// type WinModule = ModuleSymbol<WinModLoader>;
pub struct WinModule {
    /// 模块基本信息
    pub data: ModuleData,
    /// 模块符号信息
    pub syms: SymbolsData,
    /// x64用于栈回溯的函数表，x86下为空
    pub funcs: Vec<RUNTIME_FUNCTION>,
    /// 模块文件句柄，可能是NULL
    file: HANDLE,
}

impl WinModule {
}

impl UDbgModule for WinModule {
    fn data(&self) -> &sym::ModuleData { &self.data }
    // fn is_32(&self) -> bool { IS_ARCH_X64 || IS_ARCH_ARM64 }
    fn symbol_status(&self) -> SymbolStatus {
        if self.syms.pdb.read().is_some() {
            SymbolStatus::Loaded
        } else {
            SymbolStatus::Unload
        }
    }
    fn add_symbol(&self, offset: usize, name: &str) -> UDbgResult<()> {
        self.syms.add_symbol(offset, name)
    }
    fn find_symbol(&self, offset: usize, max_offset: usize) -> Option<sym::Symbol> {
        self.syms.find_symbol(offset, max_offset)
    }
    fn runtime_function(&self) -> Option<&[RUNTIME_FUNCTION]> {
        self.funcs.as_slice().into()
    }
    fn get_symbol(&self, name: &str) -> Option<sym::Symbol> {
        self.syms.get_symbol(name)
    }
    fn symbol_file(&self) -> Option<Arc<dyn sym::SymbolFile>> {
        self.syms.pdb.read().clone()
    }
    fn load_symbol_file(&self, path: Option<&str>) -> UDbgResult<()> {
        *self.syms.pdb.write() = Some(match path {
            Some(path) => Arc::new(PDBData::load(path, None)?),
            None => {
                let mmap = map_or_open(self.file, &self.data.path).ok_or("map failed")?;
                let pe = pe::parse(&mmap).ok_or("parse pe")?;
                find_pdb(&self.data.path, &pe)? as Arc<dyn SymbolFile>
            }
        });
        Ok(())
    }
    fn enum_symbol(&self, pat: Option<&str>) -> UDbgResult<Box<dyn Iterator<Item=sym::Symbol>>> {
        Ok(Box::new(self.syms.enum_symbol(pat)?.into_iter()))
    }
    fn get_exports(&self) -> Option<Vec<sym::Symbol>> {
        Some(self.syms.exports.iter().map(|i| i.1.clone()).collect())
    }
}

fn system_root() -> &'static str {
    static mut ROOT: Option<Box<str>> = None;
    unsafe {
        ROOT.get_or_insert_with(||
            std::env::var("SystemRoot").map(Into::into).unwrap_or_else(|_| r"C:\Windows".into())
        )
    }
}

impl UDbgSymMgr for SymbolManager<WinModule> {
    fn find_module(&self, address: usize) -> Option<Arc<dyn UDbgModule>> {
        Some(Self::find_module(self, address)?)
    }

    fn get_module(&self, name: &str) -> Option<Arc<dyn UDbgModule>> {
        Some(Self::get_module(self, name)?)
    }

    fn enum_module<'a>(&'a self) -> Box<dyn Iterator<Item=Arc<dyn UDbgModule + 'a>> + 'a> {
        Self::enum_module(self)
    }

    fn remove(&self, address: usize) {
        self.base.write().remove(address)
    }

    fn check_load_module(&self, read: &dyn ReadMemory, base: usize, size: usize, path: &str, file: HANDLE) -> bool {
        use goblin::pe::header::*;

        let mut symgr = self.base.write();
        if symgr.exists(base) { return false; }
        // println!("check_load_module: {:x} {} {}", base, path, symgr.list.len());

        let ui = udbg_ui();
        let mut buf = vec![0u8; 0x1000];
        let mmap = map_or_open(file, path);
        let m = match &mmap {
            Some(m) => m, None => {
                ui.warn(format!("map {} failed", path));
                if read.read_memory(base, &mut buf).is_none() {
                    ui.error(format!("read pe falied: {:x} {}", base, path));
                    return false;
                }
                buf.as_slice()
            }
        };
        let h = match Header::parse(&m) {
            Ok(h) => h, Err(err) => {
                ui.error(format!("parse {} failed: {:?}", path, err));
                return false;
            }
        };
        let o = match &h.optional_header {
            Some(o) => o, None => {
                ui.error(format!("no optional_header: {}", path));
                return false;
            }
        };
        let name = match path.rfind(|c| c == '\\' || c == '/') {
            Some(p) => &path[p+1..], None => &path,
        };

        let entry = o.standard_fields.address_of_entry_point as usize;
        let size = if size > 0 { size } else { o.windows_fields.size_of_image as usize };
        let arch = pe::machine_to_arch(h.coff_header.machine);
        // info!("load {:x} {} {}", base, arch, name);

        let mut name: Arc<str> = name.into();
        if self.is_wow64.get() && h.coff_header.machine == COFF_MACHINE_X86_64 {
            name = match name.as_ref() {
                "ntdll.dll" => "ntdll64.dll".into(),
                _ => name,
            };
        }
        let root = system_root();
        let data = ModuleData {
            user_module: (unicase::Ascii::new(root) != path.chars().take(root.len()).collect::<String>()).into(),
            base, size, entry, arch, name, path: path.into(),
        };

        let mut funcs: Vec<RUNTIME_FUNCTION> = Default::default();
        let mut pdb_sig = String::new();
        let mut pdb_name = String::new();
        let syms = if let Some(pe) = pe::parse(&m) {
            pdb_sig = pe.get_pdb_signature().unwrap_or_default().to_ascii_uppercase();
            pdb_name = pe.debug_data.and_then(|d| d.codeview_pdb70_debug_info)
                                    .and_then(|d| std::str::from_utf8(&d.filename).ok())
                                    .unwrap_or_default().trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string();
            let pdb = match find_pdb(&path, &pe) {
                Ok(p) => {
                    // info!("load pdb: {}", p.path);
                    Some(p as Arc<dyn SymbolFile>)
                }
                Err(e) => {
                    if !e.is_empty() {
                        ui.warn(format!("load pdb for {}: {}", data.name, e));
                    }
                    None
                }
            };

            pub fn get_exports_from_pe(pe: &pe::PeHelper) -> Syms {
                let mut result = Syms::new();
                for e in pe.exports.iter() {
                    let len = pe.exception_data.as_ref().and_then(|x| x.find_function(e.rva as u32).ok())
                        .and_then(|f| f.map(|f| f.end_address - f.begin_address)).unwrap_or(SYM_NOLEN);
                    result.insert(e.rva, Symbol {
                        name: e.name.unwrap_or("<>").into(),
                        type_id: 0, len,
                        offset: e.rva as u32,
                        flags: (SymbolFlags::FUNCTION | SymbolFlags::EXPORT).bits(),
                    });
                }
                result
            }

            // TODO: aarch64
            #[cfg(any(target_arch="x86_64", target_arch="x86"))] {
                funcs = pe.exception_data.iter().map(|e| e.functions()).flatten().filter_map(|x| x.ok())
                                                .map(|x| unsafe { transmute::<_, RUNTIME_FUNCTION>(x) }).collect::<Vec<_>>();
            }
            SymbolsData {
                pdb: pdb.into(),
                user_syms: Default::default(),
                exports: get_exports_from_pe(&pe),
                pdb_name: pdb_name.into(),
                pdb_sig: pdb_sig.into(),
            }
        } else {
            SymbolsData {
                exports: Default::default(),
                user_syms: Default::default(),
                pdb: Default::default(),
                pdb_name: pdb_name.into(),
                pdb_sig: pdb_sig.into(),
            }
        };
        symgr.add(WinModule {
            data, funcs, syms: syms.into(), file
        });
        true
    }
}

pub struct CommonAdaptor {
    pub base: UDbgBase,
    tid_to_run: UnsafeCell<HashSet<u32>>,       // 标记为继续运行的线程ID
    dbg_reg: [Cell<usize>; 4],
    pub symgr: SymbolManager<WinModule>,
    pub protected_thread: RwLock<Vec<u32>>,
    pub process: Process,
    pub bp_map: RwLock<HashMap<BpID, Arc<Breakpoint>>>,
    pub context: Cell<*mut CONTEXT>,
    pub cx32: Cell<*mut CONTEXT32>,
    pub step_tid: Cell<u32>,
    pub show_debug_string: Cell<bool>,
    pub uspy_tid: Cell<u32>,
}

impl CommonAdaptor {
    pub fn new(mut base: UDbgBase, p: Process) -> CommonAdaptor {
        let ui = udbg_ui();
        let symbol_cache = ui.get_config::<String>("symbol_cache").or_else(|| {
            let var = std::env::var("_NT_SYMBOL_PATH").ok();
            var.and_then(|s| s.split('*').nth(1).map(ToString::to_string))
        }).unwrap_or_default();
        let sds = ui.get_config::<bool>("show_debug_string").unwrap_or(true);
        let symgr = SymbolManager::new(symbol_cache.into());
        symgr.is_wow64.set(p.is_wow64());
        let image_base = p.peb().and_then(|peb| p.read_value::<PEB>(peb))
            .map(|peb| peb.ImageBaseAddress).unwrap_or(null_mut()) as usize;
        base.pid.set(p.pid());
        base.image_base = image_base;

        let result = Self {
            process: p, dbg_reg: Default::default(),
            bp_map: RwLock::new(HashMap::new()),
            show_debug_string: sds.into(),
            protected_thread: vec![].into(),
            tid_to_run: UnsafeCell::new(HashSet::new()),
            base, symgr,
            step_tid: Cell::new(0),
            cx32: Cell::new(null_mut()),
            context: Cell::new(null_mut()),
            uspy_tid: Cell::new(0),
        };
        result.check_all_module(&result.process); result
    }

    pub fn get_mapped_file_name(&self, module: usize) -> Option<String> {
        self.process.get_mapped_file_name(module)
    }

    #[inline(always)]
    pub fn tid_to_run(&self) -> &mut HashSet<u32> {
        unsafe { &mut *self.tid_to_run.get() }
    }

    pub fn get_hwbp_index(&self) -> Option<usize> {
        for (i, p) in self.dbg_reg.iter().enumerate() {
            if p.get() == 0 { return Some(i); }
        }
        None
    }

    pub fn occupy_hwbp_index(&self, index: usize, p: usize) {
        self.dbg_reg[index].set(p);
    }

    pub fn get_registers(&self, tid: u32) -> Option<Registers> {
        let handle = open_thread(tid, THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, false);
        unsafe {
            let mut context: CONTEXT = core::mem::zeroed();
            context.ContextFlags = CONTEXT_ALL;
            SuspendThread(*handle);
            let r = GetThreadContext(*handle, &mut context);
            ResumeThread(*handle);
            if r > 0 { Some(context_to_regs(&context)) } else { None }
        }
    }

    pub fn set_thread_context(&self, tid: u32, context: &CONTEXT) -> bool {
        let handle = open_thread(tid, THREAD_SUSPEND_RESUME | THREAD_SET_CONTEXT, false);
        unsafe {
            SuspendThread(*handle);
            let r = SetThreadContext(*handle, context as *const CONTEXT);
            ResumeThread(*handle);
            return r > 0;
        }
    }

    pub fn terminate_process(&self) -> UDbgResult<()> {
        self.process.terminate().check_errno("")?;
        Ok(())
    }

    pub fn is_process_exited(&self, timeout: u32) -> bool {
        use winapi::um::synchapi::WaitForSingleObject;
        use winapi::shared::winerror::WAIT_TIMEOUT;
        unsafe {
            WAIT_TIMEOUT != WaitForSingleObject(*self.process.handle, timeout)
        }
    }

    pub fn wait_process_exit(&self) {
        while !self.is_process_exited(10) {
            if !self.base.is_opened() { break; }
        }
    }

    #[inline(always)]
    pub fn bp_exists(&self, id: BpID) -> bool {
        self.bp_map.read().unwrap().get(&id).is_some()
    }

    fn handle_reply<C: DbgContext>(&self, this: &dyn UDbgAdaptor, reply: UserReply, exception: &ExceptionRecord, context: &mut C) {
        let tid = self.base.event_tid.get();
        match reply {
            UserReply::StepIn => {
                context.set_step(true);
                self.step_tid.set(tid);
                // info!("step_tid: {}", tid);
            }
            UserReply::StepOut => {
                if let Some(address) = this.check_call(exception.address as _) {
                    context.set_step(false);
                    self.check_result(
                        "add bp",
                        self.add_int3_bp(this, &BpOpt::int3(address).temp(true).enable(true).thread(self.base.event_tid.get()))
                    );
                } else {
                    context.set_step(true);
                    self.step_tid.set(tid);
                    // info!("step_tid out: {}", tid)
                }
            }
            UserReply::Goto(address) => {
                self.check_result("add bp", self.add_int3_bp(this, &BpOpt::int3(address).temp(true).enable(true)));
            }
            _ => { }
        };
    }

    fn get_bp(&self, id: BpID) -> Option<Arc<Breakpoint>> {
        Some(self.bp_map.read().ok()?.get(&id)?.clone())
    }

    fn check_result<T>(&self, msg: &str, r: UDbgResult<T>) {
        match r {
            Ok(_) => {}
            Err(e) => udbg_ui().error(&format!("{}: {:?}", msg, e)),
        }
    }

    pub fn user_handle_exception(&self, first: bool, tb: &mut TraceBuf) -> HandleResult {
        let reply = (tb.callback)(UEvent::Exception {first, code: tb.record.code});
        if reply == UserReply::Run(true) {
            HandleResult::Continue
        } else {
            HandleResult::NotHandled
        }
    }

    pub fn handle_int3_breakpoint<C: DbgContext>(&self, this: &dyn UDbgAdaptor, eh: &mut dyn EventHandler, first: bool, tb: &mut TraceBuf, context: &mut C) -> HandleResult {
        let address = tb.record.address;
        // info!("Breakpoint On 0x{:x}", address);
        let id = address as BpID;
        if let Some(bp) = self.get_bp(id) {
            self.handle_bp_has_data(this, eh, bp, tb, context)
        } else {
            self.user_handle_exception(first, tb)
        }
    }

    pub fn handle_step<C: DbgContext>(&self, this: &dyn UDbgAdaptor, eh: &mut dyn EventHandler, tb: &mut TraceBuf, context: &mut C) -> HandleResult {
        let address = tb.record.address;
        let dr6 = context.dr6();
        let id: BpID = if dr6 & 0x01 > 0 {
            -1
        } else if dr6 & 0x02 > 0 {
            -2
        } else if dr6 & 0x04 > 0 {
            -3
        } else if dr6 & 0x08 > 0 {
            -4
        } else { address as BpID };

        if let Some(bp) = self.get_bp(id) {
            // 可执行硬件断点检查预期地址
            if let InnerBpType::Hard(info) = bp.bp_type {
                if info.rw == HwbpType::Execute as u8 && bp.address as u64 != address {
                    return HandleResult::NotHandled;
                }
            }

            // 当前位置存在断点
            if let Some(i) = bp.hard_index() {
                if !bp.temp.get() {
                    context.set_rf();   // 临时禁用硬件断点
                }
                self.handle_bp_has_data(this, eh, bp, tb, context)
            } else {
                // 如果是软件断点，作为软件断点处理
                self.handle_bp_has_data(this, eh, bp, tb, context)
            }
        } else {
            // 不存在断点，可能是用户选择了单步
            let tid = self.base.event_tid.get();
            if self.step_tid.get() == tid {
                self.step_tid.set(0);
                self.handle_reply(this, (tb.callback)(UEvent::Step), tb.record, context);
                return HandleResult::Continue;
            }
            HandleResult::NotHandled
        }
    }

    pub fn handle_bp_has_data<C: DbgContext>(&self, this: &dyn UDbgAdaptor, eh: &mut dyn EventHandler, bp: Arc<Breakpoint>, tb: &mut TraceBuf, context: &mut C) -> HandleResult {
        bp.hit_count.set(bp.hit_count.get() + 1);
        if bp.temp.get() {
            self.remove_breakpoint(this, &bp);
        }

        // correct the pc register
        let pc = match bp.bp_type {
            InnerBpType::Table {origin, ..} => {
                C::REG::from_usize(origin)
            }
            InnerBpType::Soft(_) | InnerBpType::Hard {..} => {
                C::REG::from_usize(tb.record.address as usize)
            }
        };
        // info!("correct the pc: {:x}", pc.to_usize());
        *context.ip() = pc;

        // handle by user
        let tid = self.base.event_tid.get();
        let hitted = bp.hit_tid.map(|t| t == tid).unwrap_or(true);
        if hitted {
            self.handle_reply(this, (tb.callback)(UEvent::Breakpoint(bp.clone())), tb.record, context);
        }

        let id = bp.get_id();
        // int3断点需要特殊处理 revert
        if bp.is_soft() && self.get_bp(id).is_some() {
            // 中断过程中，断点没有被用户删除
            if bp.enabled.get() {
                // 暂时禁用，以便能继续运行
                self.check_result("disable bp", self.enable_breadpoint(this, &bp, false));

                // step once and revert
                let user_step = context.test_eflags(EFLAGS_TF);
                if !user_step {
                    context.set_step(true);
                }
                eh.cont(HandleResult::Handled);
                loop {
                    match eh.fetch(tb) {
                        Some(_) => if self.base.event_tid.get() == tid {
                            // TODO: maybe other exception?
                            assert!(matches!(tb.record.code, EXCEPTION_SINGLE_STEP | EXCEPTION_WX86_SINGLE_STEP));
                            break;
                        } else if let Some(s) = eh.handle(tb) {
                            eh.cont(s);
                        } else {
                            return HandleResult::Handled;
                        }
                        None => return HandleResult::Handled,
                    }
                }
                self.check_result("enable bp", self.enable_breadpoint(this, &bp, true));
                return if user_step {
                    eh.handle(tb).unwrap_or(HandleResult::Handled)
                } else {
                    // avoid to set context in the subsequent
                    self.cx32.set(null_mut());
                    self.context.set(null_mut());

                    HandleResult::Continue
                };
            }
        }
        HandleResult::Continue
    }

    pub fn handle_possible_table_bp<C: DbgContext>(&self, this: &dyn UDbgAdaptor, eh: &mut dyn EventHandler, tb: &mut TraceBuf, context: &mut C) -> HandleResult {
        let pc = if C::IS_32 {
            context.ip().to_usize() as i32 as isize
        } else { context.ip().to_usize() as isize };
        // info!("exception address: {:x}", pc);
        if pc > 0 { return HandleResult::NotHandled; }

        // self.get_bp(pc as BpID).map(|bp| self.handle_bp_has_data(tid, &bp, record, context)).unwrap_or(C::NOT_HANDLED)
        if let Some(bp) = self.get_bp(pc as BpID) {
            self.handle_bp_has_data(this, eh, bp, tb, context)
        } else { HandleResult::NotHandled }
    }

    pub fn open_thread(&self, tid: tid_t) -> UDbgResult<Box<WinThread>> {
        open_win_thread(&self.process, tid)
    }

    pub fn enum_thread<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item=tid_t>+'a>> {
        Ok(Box::new(self.process.enum_thread().map(|e| e.tid())))
    }

    pub fn enum_memory<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item = MemoryPage> + 'a>> {
        Ok(Box::new(MemoryIter { dbg: self, address: 0 }))
    }

    pub fn find_table_bp_index(&self) -> Option<isize> {
        for i in -10000..-10 {
            if self.bp_map.read().unwrap().get(&i).is_none() {
                return Some(i);
            }
        }
        None
    }

    fn add_int3_bp(&self, this: &dyn UDbgAdaptor, opt: &BpOpt) -> UDbgResult<Arc<Breakpoint>> {
        // int3 breakpoint
        if let Some(raw_byte) = this.read_value::<BpInsn>(opt.address) {
            let bp = Arc::new(Breakpoint {
                address: opt.address,
                enabled: Cell::new(false),
                temp: Cell::new(opt.temp),
                hit_tid: opt.tid,
                hit_count: Cell::new(0),
                bp_type: InnerBpType::Soft(raw_byte),

                target: to_weak(this), common: self,
            });
            if opt.enable { self.enable_breadpoint(this, &bp, true)?; }
            self.bp_map.write().unwrap().insert(bp.get_id(), bp.clone());
            Ok(bp)
        } else { Err(UDbgError::InvalidAddress) }
    }

    pub fn add_bp(&self, this: &dyn UDbgAdaptor, opt: &BpOpt) -> UDbgResult<Arc<Breakpoint>> {
        self.base.check_opened()?;
        if self.bp_exists(opt.address as BpID) { return Err(UDbgError::BpExists); }

        let bp = if let Some(rw) = opt.rw {
            // hardware breakpoint
            if let Some(index) = self.get_hwbp_index() {
                let bp = Arc::new(Breakpoint {
                    address: opt.address,
                    enabled: Cell::new(false),
                    temp: Cell::new(opt.temp),
                    hit_count: Cell::new(0),
                    hit_tid: opt.tid,
                    bp_type: InnerBpType::Hard(HwbpInfo {
                        rw: rw.into(), index: index as u8,
                        len: opt.len.unwrap_or(HwbpLen::L1).into(),
                    }),

                    target: to_weak(this), common: self,
                });
                self.occupy_hwbp_index(index, opt.address);
                // 硬件断点额外记录
                self.bp_map.write().unwrap().insert(-(index as BpID + 1), bp.clone());
                Ok(bp)
            } else { Err(UDbgError::HWBPSlotMiss) }
        } else if opt.table {
            // table breakpoint
            let origin = this.read_ptr(opt.address).ok_or("read origin failed")?;
            let index = self.find_table_bp_index().ok_or("no more table index")?;
            let bp = Arc::new(Breakpoint {
                address: opt.address,
                enabled: Cell::new(false),
                temp: Cell::new(opt.temp),
                hit_count: Cell::new(0),
                hit_tid: opt.tid,
                bp_type: InnerBpType::Table {index, origin},

                target: to_weak(this), common: self,
            });
            self.bp_map.write().unwrap().insert(index, bp.clone());
            Ok(bp)
        } else { self.add_int3_bp(this, opt) }?;
        let bpid = bp.get_id();
        self.bp_map.write().unwrap().insert(bpid, bp.clone());

        if opt.enable {
            self.check_result("enable bp falied", self.enable_breadpoint(this, &bp, true));
        }
        Ok(bp)
    }

    pub fn remove_breakpoint(&self, this: &dyn UDbgAdaptor, bp: &Breakpoint) {
        let mut hard_id = 0;
        let mut table_index = None;
        self.check_result("disable bp falied", self.enable_breadpoint(this, &bp, false));
        if self.bp_map.write().unwrap().remove(&bp.get_id()).is_some() {
            match bp.bp_type {
                InnerBpType::Hard(info) => {
                    self.occupy_hwbp_index(info.index as usize, 0);
                    hard_id = -(info.index as BpID + 1);
                }
                InnerBpType::Table{index, ..} => {
                    table_index = Some(index);
                }
                _ => {}
            }
        }
        // delete hardware breakpoint
        if hard_id < 0 { self.bp_map.write().unwrap().remove(&hard_id); }
        // delete table breakpoint
        table_index.map(|i| self.bp_map.write().unwrap().remove(&i));
    }

    #[inline]
    pub fn enable_hwbp_for_context<C: DbgContext>(&self, cx: &mut C, info: HwbpInfo, enable: bool) {
        let i = info.index as usize;
        if enable {
            cx.set_bp(self.dbg_reg[i].get(), i, info.rw, info.len);
        } else {
            cx.unset_bp(i);
        }
    }

    pub fn enable_hwbp_for_thread(&self, handle: HANDLE, info: HwbpInfo, enable: bool) -> UDbgResult<bool> {
        let mut result = Ok(enable);
        let mut cx = Align16::<CONTEXT>::new();
        let mut wow64cx = Align16::<WOW64_CONTEXT>::new();
        let context = cx.as_mut();
        let cx32 = wow64cx.as_mut();
        cx32.ContextFlags = CONTEXT_DEBUG_REGISTERS;
        context.ContextFlags = CONTEXT_DEBUG_REGISTERS;
        unsafe {
            let tid = GetThreadId(handle);
            let count = SuspendThread(handle) as i32;
            if count < 0 {
                udbg_ui().error(format!("SuspendThread: {}:{:p} {}", tid, handle, get_last_error()));
            }
            for _ in 0..1 {
                let r = GetThreadContext(handle, context);
                if r == 0 { result = Err(UDbgError::GetContext(get_last_error())); break; }
                self.enable_hwbp_for_context(context, info, enable);
                let r = SetThreadContext(handle, context);
                if r == 0 { result = Err(UDbgError::SetContext(get_last_error())); break; }
                #[cfg(target_arch="x86_64")]
                if Wow64GetThreadContext(handle, cx32) > 0 {
                    self.enable_hwbp_for_context(cx32, info, enable);
                    Wow64SetThreadContext(handle, cx32);
                }
            }
            ResumeThread(handle);
        }
        result
    }

    pub fn enable_all_hwbp_for_thread(&self, handle: HANDLE, enable: bool) {
        for i in 0..4 {
            self.get_bp(-i).map(|bp| if let InnerBpType::Hard(info) = bp.bp_type {
                 self.enable_hwbp_for_thread(handle, info, enable);
            });
        }
    }

    pub fn enable_breadpoint(&self, dbg: &dyn UDbgAdaptor, bp: &Breakpoint, enable: bool) -> UDbgResult<bool> {
        match bp.bp_type {
            InnerBpType::Soft(raw_byte) => {
                let written = if enable {
                    self.process.write_code(bp.address, &[0xCC])
                } else {
                    self.process.write_code(bp.address, &raw_byte)
                };
                // println!("enable softbp @{:x} {} {:?}", bp.address, enable, written);
                if written.unwrap_or_default() > 0 {
                    bp.enabled.set(enable);
                    Ok(enable)
                } else { Err(UDbgError::MemoryError) }
            }
            InnerBpType::Table {index, origin} => {
                let r = if enable {
                    dbg.write_ptr(bp.address, index as usize)
                } else {
                    dbg.write_ptr(bp.address, origin)
                };
                if r.is_some() {
                    bp.enabled.set(enable);
                    Ok(enable)
                } else { Err(UDbgError::MemoryError) }
            }
            InnerBpType::Hard(info) => {
                let mut result = Ok(enable);
                // Set Context for each thread
                for tid in self.enum_thread()? {
                    // Ignore threads
                    if self.protected_thread.read().unwrap().contains(&tid) { continue; }
                    if bp.hit_tid.is_some() && bp.hit_tid != Some(tid) { continue; }
                    // Set Debug Register
                    let th = match dbg.open_thread(tid) {
                        Ok(r) => r,
                        Err(e) => {
                            udbg_ui().error(format!("open thread {} failed {:?}", tid, e));
                            continue;
                        }
                    };
                    result = self.enable_hwbp_for_thread(*th.handle, info, enable);
                    if let Err(e) = &result {
                        udbg_ui().error(format!("enable_hwbp_for_thread for {} failed {:?}", tid, e));
                        // break;
                    }
                }
                // Set Context for current thread
                for _ in 0..1 { 
                    if bp.hit_tid.is_some() && bp.hit_tid != Some(self.base.event_tid.get()) { break; }
                    if !self.context.get().is_null() {
                        self.enable_hwbp_for_context(unsafe { self.context.get().as_mut().unwrap() }, info, enable);
                    }
                    let cx32 = self.cx32.get();
                    if !cx32.is_null() {
                        self.enable_hwbp_for_context(unsafe { self.cx32.get().as_mut().unwrap() }, info, enable);
                    }
                }
                // TODO: wow64
                if result.is_ok() { bp.enabled.set(enable); }
                result
            }
        }
    }

    pub fn enable_bp(&self, dbg: &dyn UDbgAdaptor, id: BpID, enable: bool) -> Result<bool, UDbgError> {
        if let Some(bp) = self.bp_map.read().unwrap().get(&id) {
            self.enable_breadpoint(dbg, bp, enable)
        } else { Err(UDbgError::NotFound) }
    }

    #[inline]
    pub fn get_bp_list(&self) -> Vec<BpID> {
        self.bp_map.read().unwrap().keys().filter(|&id| *id > 0).cloned().collect()
    }

    pub fn virtual_query(&self, address: usize) -> Option<MemoryPage> {
        self.process.virtual_query(address)
    }

    pub fn to_syminfo(mut sym: SymbolInfo, addr: usize, max_offset: usize) -> SymbolInfo {
        if sym.symbol.len() > 0 && sym.offset > max_offset {
            sym.symbol = "".into();
            sym.offset = addr - sym.mod_base;
        } sym
    }

    pub fn get_symbol(&self, addr: usize, max_offset: usize) -> Option<SymbolInfo> {
        self.symgr.base.read().get_symbol_info(addr, max_offset)
    }

    pub fn check_all_module(&self, dbg: &dyn ReadMemory) {
        let mut rest_loaded = HashSet::new();
        for m in self.symgr.enum_module() {
            rest_loaded.insert(m.data().base);
        }
        // for m in self.process.enum_module() {
        //     let base = m.base();
        //     rest_loaded.remove(&base);
        //     self.symgr.check_load_module(dbg, base, m.size(), m.path().as_ref(), null_mut());
        // }
        use winapi::um::psapi::LIST_MODULES_ALL;
        for m in self.process.get_module_list(LIST_MODULES_ALL).unwrap_or_default() {
            rest_loaded.remove(&m);
            if self.symgr.base.read().exists(m) { continue; }
            // get_module_path() 在 wow64 进程里拿到的是64位dll的路径，不准确
            // let path = self.process.get_module_path(m).unwrap_or_default();
            let path = self.process.get_mapped_file_name(m)
                    .unwrap_or_else(|| self.process.get_module_path(m).unwrap_or_default());
            self.process.get_module_info(m).map(|m| {
                self.symgr.check_load_module(dbg, m.lpBaseOfDll as usize, m.SizeOfImage as usize, path.as_ref(), null_mut());
            });
        }
        // 移除已经卸载了的模块
        for m in rest_loaded {
            // println!("remove: {:x}", m);
            self.symgr.remove(m);
        }

    }

    pub fn output_debug_string(&self, dbg: &dyn UDbgAdaptor, address: usize, count: usize) {
        if self.base.flags.get().contains(UFlags::SHOW_OUTPUT) {
            if let Some(s) = dbg.read_utf8_or_ansi(address, count) {
                udbg_ui().debug(&s);
            }
        }
    }

    pub fn output_debug_string_wide(&self, dbg: &dyn UDbgAdaptor, address: usize, count: usize) {
        if self.base.flags.get().contains(UFlags::SHOW_OUTPUT) {
            if let Some(s) = dbg.read_wstring(address, count) {
                udbg_ui().debug(&s);
            }
        }
    }

    pub fn get_prop(&self, key: &str) -> UDbgResult<SerdeVal> {
        Ok(match key {
            "peb" => SerdeVal::U64(self.process.peb().ok_or(UDbgError::NotFound)? as _),
            "wow64" => SerdeVal::Bool(self.symgr.is_wow64.get()),
            "handle" => SerdeVal::U64(*self.process.handle as usize as _),
            _ => return Err(UDbgError::NotFound),
        })
    }
}

impl<T: Deref<Target=CommonAdaptor>> GetProp for T {
    fn get_prop(&self, key: &str) -> UDbgResult<SerdeVal> {
        self.deref().get_prop(key)
    }
}

pub fn check_dont_set_hwbp() -> bool {
    udbg_ui().get_config("DONT_SET_HWBP").unwrap_or(false)
}

pub trait NtHeader {
    fn as_32(&self) -> &IMAGE_NT_HEADERS32;
    fn as_64(&self) -> &IMAGE_NT_HEADERS64;
    fn is_32(&self) -> bool;
}

impl NtHeader for IMAGE_NT_HEADERS {
    #[inline]
    fn as_32(&self) -> &IMAGE_NT_HEADERS32 { unsafe { transmute(self) } }
    #[inline]
    fn as_64(&self) -> &IMAGE_NT_HEADERS64  { unsafe { transmute(self) } }
    #[inline]
    fn is_32(&self) -> bool { self.FileHeader.Machine == IMAGE_FILE_MACHINE_I386 }
}

pub fn get_memory_map(p: &Process, this: &dyn UDbgAdaptor) -> Vec<UiMemory> {
    const PAGE_SIZE: usize = 0x1000;
    const MAX_HEAPS: usize = 1000;

    let peb = p.peb().unwrap_or_default();
    let mut result = this.enum_memory().unwrap().map(|m| {
        let mut usage = String::new();
        let mut flags = match m.type_ {
            MEM_PRIVATE => MF_PRIVATE,
            MEM_IMAGE => MF_IMAGE,
            MEM_MAPPED => MF_MAP, _ => 0,
        };
        if m.base == 0x7FFE0000 {
            usage.push_str("KUSER_SHARED_DATA");
        } else if m.base == peb {
            usage.push_str("PEB");
            flags |= MF_PEB;
        }

        UiMemory {
            alloc_base: m.alloc_base,
            base: m.base, size: m.size,
            flags, usage: usage.into(),
            type_: m.type_().into(),
            protect: m.protect().into(),
        }
    }).collect::<Vec<_>>();

    // Mark the thread's stack
    for tid in this.enum_thread().unwrap() {
        if let Ok(t) = this.open_thread(tid) {
            let stack = t.teb().and_then(|teb| {
                this.read_value::<NT_TIB>(teb + FIELD_OFFSET!(TEB, NtTib))
                    .map(|tib| tib.StackLimit as usize)
            });
            stack.map(|stack| RangeValue::binary_search_mut(&mut result, stack).map(|m| {
                m.usage = format!("Stack ~{}", tid).into();
                m.flags |= MF_STACK;
            }));
        }
    }

    // Mark the process heaps
    let heaps_num = if peb > 0 {
        this.read_value::<ULONG>(peb + FIELD_OFFSET!(PEB, NumberOfHeaps)).unwrap_or(0)
    } else { 0 } as usize;

    if heaps_num > 0 && heaps_num < MAX_HEAPS {
        let mut buf = vec![0usize; heaps_num];
        this.read_value::<usize>(peb + FIELD_OFFSET!(PEB, ProcessHeaps)).map(|p_heaps| {
            let len = this.read_to_array(p_heaps, &mut buf);
            buf.resize(len, 0);
        });
        for i in 0..buf.len() {
            RangeValue::binary_search_mut(&mut result, buf[i]).map(|m| {
                m.usage = format!("Heap #{}", i).into();
                m.flags |= MF_HEAP;
            });
        }
    }

    // Mark the Executable modules
    // use std::ffi::CStr;
    let mut i = 0;
    while i < result.len() {
        let mut module = 0usize;
        let mut module_size = 0usize;
        let sections: Option<Vec<IMAGE_SECTION_HEADER>> = {
            let m = &mut result[i]; i += 1;
            p.get_mapped_file_name(m.base).and_then(|p| {
                module = m.base;
                m.usage = p.into();
                if m.flags & MF_IMAGE == 0 { return None; }

                this.read_nt_header(m.base).map(|(nt, nt_offset)| {
                    module_size = if nt.is_32() { nt.as_32().OptionalHeader.SizeOfImage } else { nt.as_64().OptionalHeader.SizeOfImage } as usize;
                    let p_section_header = module + nt_offset +
                                           FIELD_OFFSET!(IMAGE_NT_HEADERS, OptionalHeader) +
                                           nt.FileHeader.SizeOfOptionalHeader as usize;
                    let mut buf = vec![
                        unsafe { core::mem::zeroed::<IMAGE_SECTION_HEADER>() };
                        nt.FileHeader.NumberOfSections as usize
                    ];
                    this.read_to_array(p_section_header, &mut buf);
                    buf
                })
            })
        };
        if let Some(sections) = sections {
            while i < result.len() && result[i].base - module < module_size {
                let m = &mut result[i]; i += 1;
                sections.iter().find(|sec| m.base == sec.VirtualAddress as usize + module)
                               .map(|sec| {
                    let name = &sec.Name;
                    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                    let name = &name[..len];
                    let sec_name = unsafe { std::str::from_utf8_unchecked(name) };
                    m.usage = sec_name.into();
                    m.flags |= MF_SECTION;
                });
            }
        }
    }
    result
}

pub fn query_object_name_timeout(handle: HANDLE) -> String {
    call_with_timeout(
        Duration::from_millis(10),
        || query_object_name(handle).ok().map(|r| {
            // r.as_mut_slice().and_then(to_dos_path).map(|p| p.to_utf8()).unwrap_or_else(|| r.to_string())
            r.to_string()
        }).unwrap_or_default()
    ).unwrap_or_default()
}

pub fn enum_process_handle<'a>(pid: pid_t, p: HANDLE) -> Result<Box<dyn Iterator<Item = UiHandle> + 'a>, UDbgError> {
    let mut type_cache = HashMap::<u32, String>::new();
    Ok(Box::new(system_handle_information().filter_map(move |h| {
        if h.pid() != pid { return None; }
        let mut handle = 0 as HANDLE;
        unsafe {
            let r = DuplicateHandle(p, h.HandleValue as HANDLE, GetCurrentProcess(), &mut handle, 0, FALSE, DUPLICATE_SAME_ACCESS);
            if 0 == r || handle.is_null() { return None; }

            let handle = Handle::from_raw_handle(handle);
            let et = type_cache.entry(h.ObjectTypeIndex as u32).or_insert_with(||
                query_object_type(*handle).map(|t| t.TypeName.to_string()).unwrap_or_default()
            );
            let type_name = et.clone();
            let name = if type_name == "Process" {
                Process { handle }.image_path().unwrap_or_default()
            } else {
                query_object_name_timeout(*handle)
            };
            Some(UiHandle {
                name, type_name,
                ty: h.ObjectTypeIndex as u32,
                handle: h.HandleValue as usize,
            })
        }
    })))
}

pub struct MemoryIter<'a> {
    dbg: &'a CommonAdaptor,
    address: usize,
}

impl<'a> Iterator for MemoryIter<'a> {
    type Item = MemoryPage;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(p) = self.dbg.virtual_query(self.address) {
            self.address += p.size;
            if p.is_commit() { return Some(p); }
        }
        return None;
    }
}

#[derive(Deref)]
pub struct StandardAdaptor {
    #[deref]
    pub debug: CommonAdaptor,
    pub record: UnsafeCell<ExceptionRecord>,
    pub threads: UnsafeCell<HashMap<u32, DbgThread>>,
    pub attached: Cell<bool>,     // create by attach
    pub detaching: Cell<bool>,
}

unsafe impl Send for StandardAdaptor {}
unsafe impl Sync for StandardAdaptor {}

const PROC_THREAD_ATTRIBUTE_NUMBER: usize =    0x0000FFFF;
const PROC_THREAD_ATTRIBUTE_THREAD: usize =    0x00010000;
const PROC_THREAD_ATTRIBUTE_INPUT: usize =     0x00020000;
const PROC_THREAD_ATTRIBUTE_ADDITIVE: usize =  0x00040000;

const fn ProcThreadAttributeValue(Number: usize, Thread: usize, Input: usize, Additive: usize) -> usize {
    ((Number) & PROC_THREAD_ATTRIBUTE_NUMBER) |
     (if Thread != 0 { PROC_THREAD_ATTRIBUTE_THREAD } else { 0 }) |
     (if Input != 0 { PROC_THREAD_ATTRIBUTE_INPUT } else { 0 }) |
     (if Additive != 0 { PROC_THREAD_ATTRIBUTE_ADDITIVE } else { 0 })
}

pub fn create_debug_process(path: &str, cwd: Option<&str>, args: &[&str], pi: &mut PROCESS_INFORMATION, ppid: Option<pid_t>) -> UDbgResult<Process> {
    unsafe {
        let mut cmdline = path.trim().to_string();
        if cmdline.find(char::is_whitespace).is_some() {
            cmdline = format!("\"{}\"", cmdline);
        }
        if !args.is_empty() {
            cmdline += " ";
            cmdline += &args.join(" ");
        }
        let cwd = cwd.map(|v| v.to_wide());
        let cwd = cwd.as_ref().map(|r| r.as_ptr()).unwrap_or(null());

        const DEFAULT_OPTION: u32 = DEBUG_ONLY_THIS_PROCESS | CREATE_NEW_CONSOLE;
        let mut create_process = |opt: u32, si: LPSTARTUPINFOW| CreateProcessW(
            null_mut(),
            cmdline.to_wide().as_mut_ptr(),
            null_mut(), null_mut(), FALSE,
            DEFAULT_OPTION | opt,
            null_mut(), cwd, si, pi
        );
        let r = if let Some(ppid) = ppid {
            let mut si: STARTUPINFOEXW = core::mem::zeroed();
            si.StartupInfo.cb = size_of_val(&si) as u32;

            let mut psize = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut psize);
            let mut pa = BufferType::<PROC_THREAD_ATTRIBUTE_LIST>::with_size(psize);
            let mut handle = OpenProcess(PROCESS_CREATE_PROCESS, 0, ppid);
            handle.as_ref().ok_or("ppid open failed")?;

            InitializeProcThreadAttributeList(pa.as_mut_ptr(), 1, 0, &mut psize);
            let ProcThreadAttributeParentProcess = 0;
            let PROC_THREAD_ATTRIBUTE_PARENT_PROCESS = ProcThreadAttributeValue(ProcThreadAttributeParentProcess, 0, 1, 0);
            if UpdateProcThreadAttribute(
                pa.as_mut_ptr(), 0,
                PROC_THREAD_ATTRIBUTE_PARENT_PROCESS,
                transmute(&mut handle), size_of_val(&handle),
                null_mut(), null_mut()
            ) == 0 { return Err("set ppid falied".into()); }
            si.lpAttributeList = pa.as_mut_ptr();

            let r = create_process(EXTENDED_STARTUPINFO_PRESENT, transmute(&mut si));
            DeleteProcThreadAttributeList(pa.as_mut_ptr()); r
        } else {
            let mut si: STARTUPINFOW = core::mem::zeroed();
            si.cb = size_of_val(&si) as u32;
            create_process(0, &mut si)
        };
        if r == 0 { return Err(UDbgError::Text(get_last_error_string())); }
        Ok(Process::from_handle(Handle::from_raw_handle(pi.hProcess)).check_errstr("get pid")?)
    }
}

impl StandardAdaptor {
    pub fn open(base: UDbgBase, pid: pid_t) -> Result<Arc<StandardAdaptor>, UDbgError> {
        let p = Process::open(pid, None).check_errstr("open process")?;
        Ok(Self::new(base, p))
    }

    #[inline]
    pub fn threads(&self) -> &mut HashMap<u32, DbgThread> {
        unsafe { transmute(self.threads.get()) }
    }

    fn new(mut base: UDbgBase, p: Process) -> Arc<Self> {
        base.pid.set(p.pid());
        base.image_path = p.image_path().unwrap_or_default();

        Arc::new(Self {
            debug: CommonAdaptor::new(base, p),
            record: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            threads: HashMap::new().into(),
            attached: false.into(),
            detaching: false.into(),
        })
    }

    pub fn try_load_module(&self, pname: usize, base: usize, file: HANDLE, unicode: bool) {
        use std::path::Path;
        // https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfinalpathnamebyhandlew
        use winapi::um::fileapi::GetFinalPathNameByHandleW;

        let this: &dyn UDbgAdaptor = self;
        let mut path = this.read_ptr(pname).and_then(|a| {
            if unicode {
                self.process.read_wstring(a, MAX_PATH)
            } else {
                self.process.read_utf8_or_ansi(a, MAX_PATH)
            }
        });
        // Win7上ntdll.dll得到的是相对路径
        let exists = path.as_ref().map(|p| Path::new(p).exists()).unwrap_or_default();
        if !exists {
            path = self.process.get_mapped_file_name(base).or_else(|| unsafe {
                let mut buf = [0u16; 500];
                if GetFinalPathNameByHandleW(file, buf.as_mut_ptr(), buf.len() as u32, 2) > 0 {
                    to_dos_path(&mut buf).map(String::from_wide).or_else(|| Some(String::from_wide(&buf)))
                } else { error!("GetFinalPathNameByHandleW"); None }
            });
        }
        if let Some(path) = path {
            self.symgr.check_load_module(self, base, 0, &path, file);
            // self.base().module_load(&path, base);
        } else { error!("get path of module: 0x{:x} failed", base); }
    }

    #[inline]
    pub fn record(&self) -> &mut ExceptionRecord {
        unsafe { &mut *self.record.get() }
    }

    pub fn context(&self) -> Option<&mut CONTEXT> {
        unsafe { transmute(self.context.get()) }
    }

    fn open_thread(&self, tid: tid_t) -> UDbgResult<Box<WinThread>> {
        if let Some(t) = self.threads().get(&tid) {
            let wow64 = self.symgr.is_wow64.get();
            let handle = unsafe { Handle::clone_from_raw(t.handle) }?;
            Ok(Box::new(WinThread {
                base: ThreadData {tid: t.tid, wow64, handle},
                process: &self.process,
                teb: AtomicCell::new(t.local_base),
                infos: Arc::new(HashMap::new()),
            }))
        } else {
            self.debug.open_thread(tid)
        }
    }

    pub fn get_context<C: DbgContext>(&self, tid: tid_t, context: &mut C) -> bool {
        if let Some(t) = self.threads().get(&tid) {
            context.get_context(t.handle)
        } else {
            warn!("thread {} not found in debugger", tid);
            get_thread_context(tid, context, CONTEXT_ALL)
        }
    }

    pub fn set_context<C: DbgContext>(&self, tid: tid_t, c: &C) {
        let suc = if let Some(t) = self.threads().get(&tid) {
            c.set_context(t.handle)
        } else {
            warn!("thread {} not found in debugger", tid);
            set_thread_context(tid, c)
        };
        if !suc {
            error!("fatal: SetThreadContext {} {}", get_last_error(), get_last_error_string());
        }
    }
}

// #[intertrait::cast_to]
impl ReadMemory for StandardAdaptor {
    fn read_memory<'a>(&self, addr: usize, data: &'a mut [u8]) -> Option<&'a mut [u8]> {
        self.process.read_memory(addr, data)
    }
}

// #[intertrait::cast_to]
impl WriteMemory for StandardAdaptor {
    fn write_memory(&self, addr: usize, data: &[u8]) -> Option<usize> {
        match self.process.write_memory(addr, data) {
            0 => None, s => Some(s),
        }
    }
}

#[inline(always)]
pub fn wait_for_debug_event(timeout: u32) -> Option<DEBUG_EVENT> {
    unsafe {
        let mut dv: DEBUG_EVENT = core::mem::zeroed();
        if WaitForDebugEvent(&mut dv, timeout) == 0 { None }
        else { Some(dv) }
    }
}

#[inline(always)]
pub fn continue_debug_event(pid: u32, tid: u32, status: u32) -> bool {
    unsafe {
        ContinueDebugEvent(pid, tid, status) != 0
    }
}

impl AdaptorSpec for StandardAdaptor {
    fn handle(&self) -> HANDLE {
        *self.debug.process.handle
    }
    
    fn exception_context(&self) -> UDbgResult<PCONTEXT> {
        Ok(self.context.get())
    }

    fn exception_record(&self) -> UDbgResult<PEXCEPTION_RECORD> {
        unsafe {
            Ok(core::mem::transmute(self.record.get()))
        }
    }
}

impl UDbgAdaptor for StandardAdaptor {
    fn base(&self) -> &UDbgBase { &self.base }

    fn virtual_query(&self, address: usize) -> Option<MemoryPage> {
        self.debug.virtual_query(address)
    }

    fn get_thread_context(&self, tid: u32) -> Option<Registers> {
        self.debug.get_registers(tid)
    }

    fn detach(&self) -> Result<(), UDbgError> {
        if self.base.is_opened() {
            self.base.status.set(UDbgStatus::Ended);
            return Ok(());
        }
        self.detaching.set(true);
        if !self.base.is_paused() {
            self.breakk()?
        }
        Ok(())
    }

    fn breakk(&self) -> Result<(), UDbgError> {
        self.base.check_opened()?;
        if self.base.is_paused() {
            return Err("already paused".into());
        }
        unsafe {
            if DebugBreakProcess(*self.process.handle) > 0 {
                Ok(())
            } else { Err(UDbgError::system()) }
        }
    }

    fn kill(&self) -> Result<(), UDbgError> {
        self.debug.terminate_process()
    }

    fn except_param(&self, i: usize) -> Option<usize> {
        self.record().params.get(i).map(|v| *v as usize)
    }

    fn symbol_manager(&self) -> Option<&dyn UDbgSymMgr> {
        Some(&self.symgr)
    }

    fn enum_module<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item=Arc<dyn UDbgModule+'a>>+'a>> {
        if self.base.is_opened() {
            self.check_all_module(self);
        }
        Ok(self.symgr.enum_module())
    }

    fn enum_thread<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item=tid_t>+'a>> {
        if self.base.is_opened() {
            return self.debug.enum_thread();
        }
        Ok(Box::new(self.threads().iter().map(|(_, t)| t.tid)))
    }

    fn enum_memory<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item = MemoryPage>+'a>> {
        self.debug.enum_memory()
    }

    fn add_bp(&self, opt: BpOpt) -> UDbgResult<Arc<dyn UDbgBreakpoint>> {
        Ok(self.debug.add_bp(self, &opt)?)
    }

    fn get_bp<'a>(&'a self, id: BpID) -> Option<Arc<dyn UDbgBreakpoint + 'a>> {
        Some(self.bp_map.read().ok()?.get(&id)?.clone())
    }

    fn get_bp_list(&self) -> Vec<BpID> { self.debug.get_bp_list() }

    fn get_registers<'a>(&'a self) -> UDbgResult<&'a mut dyn UDbgRegs> {
        let p = self.cx32.get();
        if p.is_null() {
            Ok(self.context().ok_or(UDbgError::TargetIsBusy)?)
        } else {
            Ok(unsafe { &mut *p })
        }
    }

    fn get_memory_map(&self) -> Vec<UiMemory> {
        get_memory_map(&self.process, self as &dyn UDbgAdaptor)
    }

    fn open_thread(&self, tid: tid_t) -> Result<Box<dyn UDbgThread>, UDbgError> {
        StandardAdaptor::open_thread(self, tid).map(|r| r as Box<dyn UDbgThread>)
    }

    fn open_all_thread(&self) -> Vec<(tid_t, Box<dyn UDbgThread>)> {
        open_all_thread(&self.process, self.base.pid.get(), Some(self))
    }

    fn enum_handle<'a>(&'a self) -> Result<Box<dyn Iterator<Item = UiHandle> + 'a>, UDbgError> {
        enum_process_handle(self.base.pid.get(), *self.process.handle)
    }

    fn event_loop(&self, callback: &mut UDbgCallback) -> UDbgResult<()> {unsafe {
        let wow64 = self.symgr.is_wow64.get();
        udbg_ui().info(format!("wow64: {}", wow64));

        if self.base.is_opened() {
            if wow64 {
                self.base.update_arch(ARCH_X86);
            }
            self.wait_process_exit();
            return Ok(());
        }

        let mut cx = Align16::<CONTEXT>::new();
        let mut cx32 = core::mem::zeroed();
        let mut eh = Win32Handler {
            event: core::mem::zeroed(),
            wow64, this: self,
            first_bp_hitted: false,
            first_bp32_hitted: self.attached.get(),
        };
        let mut buf = TraceBuf {
            callback,
            cx: cx.as_mut(),
            cx32: &mut cx32,
            record: self.record(),
        };

        while let Some(s) = eh.fetch(&mut buf).and_then(|_| eh.handle(&mut buf)) {
            eh.cont(s);
        }

        Ok(())
    }}
}

pub struct TraceBuf<'a> {
    pub callback: &'a mut UDbgCallback<'a>,
    pub record: &'a ExceptionRecord,
    pub cx: *mut CONTEXT,
    pub cx32: *mut CONTEXT32,
}

pub trait EventHandler /*: AsMut<TraceBuf<'a>>*/ {
    fn fetch(&mut self, buf: &mut TraceBuf) -> Option<()>;
    fn handle(&mut self, buf: &mut TraceBuf) -> Option<HandleResult>;
    fn cont(&mut self, _: HandleResult); // continue debug event
}

#[derive(Deref)]
pub struct Win32Handler<'a> {
    #[deref]
    this: &'a StandardAdaptor,
    event: DEBUG_EVENT,
    wow64: bool,
    first_bp_hitted: bool,
    first_bp32_hitted: bool,
}

impl Win32Handler<'_> {
    fn update_context(&mut self, buf: &mut TraceBuf) {
        let cx = unsafe { buf.cx.as_mut().unwrap() };
        if self.get_context(self.event.dwThreadId, cx) {
            self.base.event_pc.set(*AbstractRegs::ip(cx) as usize);
            self.debug.context.set(cx);
        } else {
            warn!("get_thread_context {} failed {}", self.event.dwThreadId, get_last_error());
        }
    }
}

impl<'a> EventHandler for Win32Handler<'a> {
    fn fetch(&mut self, _: &mut TraceBuf) -> Option<()> {
        let base = &self.this.base;
        base.status.set(UDbgStatus::Running);
        self.event = wait_for_debug_event(INFINITE)?;
        // let pid = self.event.dwProcessId;
        base.status.set(UDbgStatus::Paused);
        base.event_tid.set(self.event.dwThreadId);
        if self.event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT {
            unsafe {
                let record = self.this.record();
                record.copy(&self.event.u.Exception().ExceptionRecord);
                base.event_pc.set(record.address as usize);
            }
        }
        Some(())
    }

    fn handle(&mut self, buf: &mut TraceBuf) -> Option<HandleResult> {
        let this = self.this;
        let base = &this.base;

        let mut cotinue_status = HandleResult::Continue;
        unsafe {
            use UEvent::*;

            // let pid = dv.dwProcessId;
            let tid = self.event.dwThreadId;
            // if pid != this.process.pid {
            //     this = match self.get_target(pid) {
            //         Some(t) => t,
            //         None => continue,       // TODO:
            //     };
            //     this.base.pid.set(pid);
            //     UDbgTarget::set_current(Some(this.clone().into()));
            // }

            match self.event.dwDebugEventCode {
                CREATE_PROCESS_DEBUG_EVENT => {
                    let info = self.event.u.CreateProcessInfo();
                    if info.hThread.is_null() {
                        udbg_ui().error(format!("CREATE_PROCESS_DEBUG_EVENT {}", tid));
                    }
                    self.threads().insert(tid, DbgThread::from(info));
                    self.try_load_module(info.lpImageName as usize, info.lpBaseOfImage as usize, info.hFile, info.fUnicode > 0);
                    self.update_context(buf);
                    (buf.callback)(ProcessCreate);
                    (buf.callback)(ThreadCreate(tid));
                    // 获取PEB32的逻辑，实践得知 &PEB32 = (char*)&PEB + 0x1000
                    // let th = info.hThread;
                    // let mut cx32: WOW64_CONTEXT = zeroed();
                    // cx32.ContextFlags = CONTEXT_SEGMENTS;
                    // Wow64GetThreadContext(th, &mut cx32);
                    // let mut ldt: WOW64_LDT_ENTRY = zeroed();
                    // let r = Wow64GetThreadSelectorEntry(th, cx32.SegFs, &mut ldt);
                    // let base = ldt.BaseLow as u32 | ((ldt.HighWord.Bits_mut().BaseMid() as u32) << 16) | ((ldt.HighWord.Bits_mut().BaseHi() as u32) << 24);
                    // // info!("Wow64GetThreadSelectorEntry: {}, fs: 0x{:x} base: 0x{:x}", r, cx32.SegFs, base);
                    // use ntapi::ntwow64::TEB32;
                    // let t32 = self.read_value::<TEB32>(base as usize).map(|t32| {
                    //     let peb32 = t32.ProcessEnvironmentBlock;
                    //     info!("PEB32: 0x{:x}", peb32);
                    // });
                }
                EXIT_PROCESS_DEBUG_EVENT => {
                    self.update_context(buf);
                    let code = self.event.u.ExitProcess().dwExitCode;
                    (buf.callback)(ProcessExit(code));
                    return None;  // TODO: check if all process exited
                }
                CREATE_THREAD_DEBUG_EVENT => {
                    let info = self.event.u.CreateThread();
                    if info.hThread.is_null() {
                        udbg_ui().error(format!("CREATE_THREAD_DEBUG_EVENT {}", tid));
                    }
                    if !check_dont_set_hwbp() {
                        self.enable_all_hwbp_for_thread(info.hThread, true);
                    }
                    self.threads().insert(tid, DbgThread::from(info));
                    self.update_context(buf);
                    (buf.callback)(ThreadCreate(tid));
                }
                EXIT_THREAD_DEBUG_EVENT => {
                    self.update_context(buf);
                    self.threads().remove(&tid);
                    self.debug.context.set(null_mut());
                    (buf.callback)(ThreadExit(self.event.u.ExitThread().dwExitCode));
                }
                LOAD_DLL_DEBUG_EVENT => {
                    self.update_context(buf);
                    // https://docs.microsoft.com/zh-cn/windows/win32/api/minwinbase/ns-minwinbase-load_dll_debug_info
                    let info = self.event.u.LoadDll();
                    self.try_load_module(info.lpImageName as usize, info.lpBaseOfDll as usize, info.hFile, info.fUnicode > 0);
                    if let Some(m) = self.symgr.find_module(info.lpBaseOfDll as usize) {
                        (buf.callback)(ModuleLoad(m));
                    }
                }
                UNLOAD_DLL_DEBUG_EVENT => {
                    self.update_context(buf);
                    let info = &self.event.u.UnloadDll();
                    let base = info.lpBaseOfDll as usize;
                    // let path = self.process.get_module_path(base).unwrap_or("".into());
                    if let Some(m) = self.symgr.find_module(base) {
                        (buf.callback)(ModuleUnload(m));
                    }
                    self.debug.symgr.remove(base);
                }
                OUTPUT_DEBUG_STRING_EVENT => {
                    if self.show_debug_string.get() {
                        let s = self.event.u.DebugString();
                        if s.fUnicode > 0 {
                            self.output_debug_string_wide(this, s.lpDebugStringData as usize, s.nDebugStringLength as usize);
                        } else {
                            self.output_debug_string(this, s.lpDebugStringData as usize, s.nDebugStringLength as usize);
                        }
                    } else {
                        cotinue_status = HandleResult::NotHandled;
                    }
                }
                RIP_EVENT => {
                    // https://docs.microsoft.com/en-us/windows/win32/api/minwinbase/ns-minwinbase-rip_info
                    let info = self.event.u.RipInfo();
                    udbg_ui().error(format!("RIP_EVENT: Error: {:x} Type: {}", info.dwError, info.dwType));
                }
                EXCEPTION_DEBUG_EVENT => {
                    self.update_context(buf);
                    let first = self.event.u.Exception().dwFirstChance > 0;
                    // align the context's address with 16
                    let cx = buf.cx.as_mut().unwrap();
                    let cx32 = buf.cx32.as_mut().unwrap();

                    let record = self.record();
                    let wow64 = self.wow64 && match record.code as i32 {
                        STATUS_WX86_BREAKPOINT | STATUS_WX86_SINGLE_STEP |
                        STATUS_WX86_UNSIMULATE | STATUS_WX86_INTERNAL_ERROR |
                        STATUS_WX86_FLOAT_STACK_CHECK => true,
                        #[cfg(any(target_arch="x86_64", target_arch="x86"))]
                        _ => cx.SegCs == 0x23,
                        #[cfg(any(target_arch="aarch64"))]
                        _ => false,
                    };
                    // println!("record.code: {:x} wow64: {}", record.code, wow64);
                    if wow64 {
                        self.get_context(tid, cx32);
                        *cx32.ip() = u32::from_usize(self.record().address as usize);
                        self.cx32.set(cx32);
                        base.update_arch(ARCH_X86);
                    } else {
                        base.update_arch(ARCH_X64);
                    }
                    cotinue_status = match record.code {
                        EXCEPTION_WX86_BREAKPOINT => {
                            if self.first_bp32_hitted {
                                self.debug.handle_int3_breakpoint(this, self, first, buf, cx32)
                            } else {
                                self.first_bp32_hitted = true;
                                self.debug.handle_reply(this, (buf.callback)(InitBp), buf.record, cx32);
                                HandleResult::NotHandled
                            }
                        }
                        EXCEPTION_BREAKPOINT => {
                            if self.first_bp_hitted {
                                self.debug.handle_int3_breakpoint(this, self, first, buf, cx)
                            } else {
                                self.first_bp_hitted = true;
                                // 创建32位进程时忽略 附加32位进程时不忽略
                                if !self.symgr.is_wow64.get() || self.attached.get() {
                                    self.debug.handle_reply(this, (buf.callback)(InitBp), buf.record, cx);
                                }
                                HandleResult::Continue
                            }
                        }
                        EXCEPTION_WX86_SINGLE_STEP => {
                            let mut result = self.handle_step(this, self, buf, cx32);
                            if result == HandleResult::NotHandled {
                                result = self.user_handle_exception(first, buf);
                            }
                            result
                        }
                        EXCEPTION_SINGLE_STEP => {
                            let mut result = self.handle_step(this, self, buf, cx);
                            if result == HandleResult::NotHandled {
                                result = self.user_handle_exception(first, buf);
                            }
                            result
                        }
                        code => {
                            if code == STATUS_WX86_CREATEWX86TIB as u32 {
                                info!("STATUS_WX86_CREATEWX86TIB");
                            }
                            let mut result = if code == EXCEPTION_ACCESS_VIOLATION {
                                if wow64 {
                                    self.debug.handle_possible_table_bp(this, self, buf, cx32)
                                } else {
                                    self.debug.handle_possible_table_bp(this, self, buf, cx)
                                }
                            } else {
                                HandleResult::NotHandled
                            };
                            if result == HandleResult::NotHandled && !self.detaching.get() {
                                result = self.user_handle_exception(first, buf);
                            }
                            result
                        }
                    };
                }
                _code => panic!("Invalid DebugEventCode {}", _code),
            };
        }

        Some(cotinue_status)
    }

    fn cont(&mut self, status: HandleResult) {
        let cx32 = self.cx32.get();
        if !cx32.is_null() {
            self.set_context(self.event.dwThreadId, unsafe { &*cx32 });
            self.cx32.set(null_mut());
        } else {
            // TODO: think detail
            // if cx.test_eflags(EFLAGS_TF) {
            //     cotinue_status = HandleResult::Continue;
            // }
            let cx = self.debug.context.get();
            if !cx.is_null() {
                self.set_context(self.event.dwThreadId, unsafe { &*cx });
            }
        }
        self.debug.context.set(null_mut());
        continue_debug_event(self.event.dwProcessId, self.event.dwThreadId, status as u32);

        if self.detaching.get() {
            udbg_ui().info(
                &format!("detach: {}", unsafe { DebugActiveProcessStop(self.event.dwProcessId) })
            );
            // return None;
        }
    }
}

pub fn open_win_thread(process: *const Process, tid: tid_t) -> UDbgResult<Box<WinThread>> {
    WinThread::new(tid).map(|mut t| unsafe {
        t.process = process;
        t.base.wow64 = process.as_ref().map(|p| p.is_wow64()).unwrap_or_default();
        Box::new(t)
    }).ok_or(UDbgError::system())
}

pub fn open_all_thread(p: *const Process, pid: pid_t, map: Option<&StandardAdaptor>) -> Vec<(tid_t, Box<dyn UDbgThread>)> {
    let mut info: HashMap<u32, SYSTEM_THREAD_INFORMATION> = HashMap::new();
    let threads = enum_thread().filter_map(|t|
        if t.pid() == pid { Some(t.tid()) } else { None }
    ).collect::<HashSet<_>>();

    match system_process_information() {
        Ok(infos) => {
            for p in infos {
                for t in p.threads().iter() {
                    let tid = t.ClientId.UniqueThread as u32;
                    if threads.contains(&tid) {
                        info.insert(tid, *t);
                    }
                }
            }
        }
        Err(e) => {
            error!("system_process_information: {:x}", e);
        }
    }

    let infos = Arc::new(info);
    let mut result = Vec::with_capacity(threads.len());
    for tid in threads {
        if let Some(mut t) = map.and_then(|a| StandardAdaptor::open_thread(a, tid).ok()) {
            t.infos = infos.clone();
            result.push((tid, t as Box<dyn UDbgThread>));
            continue;
        }
        if let Ok(mut t) = open_win_thread(p, tid) {
            t.infos = infos.clone();
            result.push((tid, t as Box<dyn UDbgThread>));
        } else {
            let mut t = Box::new(WinThread::new(tid).unwrap());
            t.infos = infos.clone();
            result.push((tid, t as Box<dyn UDbgThread>));
        }
    }
    result
}

pub struct DefaultEngine;

impl UDbgEngine for DefaultEngine {
    fn open(&self, base: UDbgBase, pid: pid_t) -> UDbgResult<Arc<dyn UDbgAdaptor>> {
        Ok(StandardAdaptor::open(base, pid)?)
    }

    fn attach(&self, base: UDbgBase, pid: pid_t) -> UDbgResult<Arc<dyn UDbgAdaptor>> {
        unsafe {
            DebugActiveProcess(pid).check_err("DebugActiveProcess")?;
            let result = StandardAdaptor::open(base, pid)?;
            result.attached.set(true);
            Ok(result)
        }
    }

    fn create(&self, base: UDbgBase, path: &str, cwd: Option<&str>, args: &[&str]) -> UDbgResult<Arc<dyn UDbgAdaptor>> {
        let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
        let ppid = udbg_ui().get_config("ppid");
        Ok(StandardAdaptor::new(base, create_debug_process(path, cwd, args, &mut pi, ppid)?))
    }
}