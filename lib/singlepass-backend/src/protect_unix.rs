//! Installing signal handlers allows us to handle traps and out-of-bounds memory
//! accesses that occur when runniing WebAssembly.
//!
//! This code is inspired by: https://github.com/pepyakin/wasmtime/commit/625a2b6c0815b21996e111da51b9664feb174622
//!
//! When a WebAssembly module triggers any traps, we perform recovery here.
//!
//! This module uses TLS (thread-local storage) to track recovery information. Since the four signals we're handling
//! are very special, the async signal unsafety of Rust's TLS implementation generally does not affect the correctness here
//! unless you have memory unsafety elsewhere in your code.
//!
use libc::{c_int, c_void, siginfo_t};
use nix::sys::signal::{
    sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal, SIGBUS, SIGFPE, SIGILL, SIGSEGV,
    SIGTRAP,
};
use std::any::Any;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ptr;
use std::sync::Arc;
use std::sync::Once;
use wasmer_runtime_core::codegen::BkptInfo;
use wasmer_runtime_core::typed_func::WasmTrapInfo;
use wasmer_runtime_core::state::x64::{X64Register, GPR, read_stack};
use wasmer_runtime_core::vm;

fn join_strings(x: impl Iterator<Item = String>, sep: &str) -> String {
    let mut ret = String::new();
    let mut first = true;

    for s in x {
        if first {
            first = false;
        } else {
            ret += sep;
        }
        ret += &s;
    }

    ret
}

fn format_optional_u64_sequence(x: &[Option<u64>]) -> String {
    use colored::*;

    if x.len() == 0 {
        "(empty)".into()
    } else {
        join_strings(x.iter()
            .enumerate()
            .map(|(i, x)| {
                format!("[{}] = {}", i, x.map(|x| format!("{}", x)).unwrap_or_else(|| "?".to_string()).bold().cyan())
            })
        , ", ")
    }
}

extern "C" fn signal_trap_handler(
    signum: ::nix::libc::c_int,
    siginfo: *mut siginfo_t,
    ucontext: *mut c_void,
) {
    unsafe {
        let fault = get_fault_info(siginfo as _, ucontext);

        match Signal::from_c_int(signum) {
            Ok(SIGTRAP) => {
                let bkpt_map = BKPT_MAP.with(|x| x.borrow().last().map(|x| x.clone()));
                if let Some(bkpt_map) = bkpt_map {
                    if let Some(ref x) = bkpt_map.get(&(fault.ip as usize)) {
                        (x)(BkptInfo { throw: throw });
                        return;
                    }
                }
            }
            _ => {}
        }

        // TODO: make this safer
        let ctx = &*(fault.known_registers[X64Register::GPR(GPR::R15).to_index().0].unwrap() as *mut vm::Ctx);
        let rsp = fault.known_registers[X64Register::GPR(GPR::RSP).to_index().0].unwrap();

        let msm = (*ctx.module)
            .runnable_module
            .get_module_state_map()
            .unwrap();
        let code_base = (*ctx.module).runnable_module.get_code().unwrap().as_ptr() as usize;
        let frames = self::read_stack(&msm, code_base, rsp as usize as *const u64, fault.known_registers, Some(fault.ip as usize as u64));

        use colored::*;
        eprintln!("\n{}\n", "Wasmer encountered an error while running your WebAssembly program.".bold().red());
        if frames.len() == 0 {
            eprintln!("{}", "Unknown fault address, cannot read stack.".yellow());
        } else {
            use colored::*;
            eprintln!("{}\n", "Backtrace:".bold());
            for (i, f) in frames.iter().enumerate() {
                eprintln!("{}", format!("* Frame {} @ Local function {}", i, f.local_function_id).bold());
                eprintln!("  {} {}", "Locals:".bold().yellow(), format_optional_u64_sequence(&f.locals));
                eprintln!("  {} {}", "Stack:".bold().yellow(), format_optional_u64_sequence(&f.stack));
                eprintln!("");
            }
        }

        do_unwind(signum, siginfo as _, ucontext);
    }
}

extern "C" {
    pub fn setjmp(env: *mut c_void) -> c_int;
    fn longjmp(env: *mut c_void, val: c_int) -> !;
}

pub unsafe fn install_sighandler() {
    let sa = SigAction::new(
        SigHandler::SigAction(signal_trap_handler),
        SaFlags::SA_ONSTACK,
        SigSet::empty(),
    );
    sigaction(SIGFPE, &sa).unwrap();
    sigaction(SIGILL, &sa).unwrap();
    sigaction(SIGSEGV, &sa).unwrap();
    sigaction(SIGBUS, &sa).unwrap();
    sigaction(SIGTRAP, &sa).unwrap();
}

const SETJMP_BUFFER_LEN: usize = 27;
pub static SIGHANDLER_INIT: Once = Once::new();

thread_local! {
    pub static SETJMP_BUFFER: UnsafeCell<[c_int; SETJMP_BUFFER_LEN]> = UnsafeCell::new([0; SETJMP_BUFFER_LEN]);
    pub static CAUGHT_FAULTS: Cell<Option<FaultInfo>> = Cell::new(None);
    pub static CURRENT_EXECUTABLE_BUFFER: Cell<*const c_void> = Cell::new(ptr::null());
    pub static TRAP_EARLY_DATA: Cell<Option<Box<dyn Any>>> = Cell::new(None);
    pub static BKPT_MAP: RefCell<Vec<Arc<HashMap<usize, Box<Fn(BkptInfo) + Send + Sync + 'static>>>>> = RefCell::new(Vec::new());
}

pub unsafe fn trigger_trap() -> ! {
    let jmp_buf = SETJMP_BUFFER.with(|buf| buf.get());

    longjmp(jmp_buf as *mut c_void, 0)
}

pub enum CallProtError {
    Trap(WasmTrapInfo),
    Error(Box<dyn Any>),
}

pub fn call_protected<T>(f: impl FnOnce() -> T) -> Result<T, CallProtError> {
    unsafe {
        let jmp_buf = SETJMP_BUFFER.with(|buf| buf.get());
        let prev_jmp_buf = *jmp_buf;

        SIGHANDLER_INIT.call_once(|| {
            install_sighandler();
        });

        let signum = setjmp(jmp_buf as *mut _);
        if signum != 0 {
            *jmp_buf = prev_jmp_buf;

            if let Some(data) = TRAP_EARLY_DATA.with(|cell| cell.replace(None)) {
                Err(CallProtError::Error(data))
            } else {
                // let (faulting_addr, _inst_ptr) = CAUGHT_ADDRESSES.with(|cell| cell.get());

                // let signal = match Signal::from_c_int(signum) {
                //     Ok(SIGFPE) => "floating-point exception",
                //     Ok(SIGILL) => "illegal instruction",
                //     Ok(SIGSEGV) => "segmentation violation",
                //     Ok(SIGBUS) => "bus error",
                //     Err(_) => "error while getting the Signal",
                //     _ => "unknown trapped signal",
                // };
                // // When the trap-handler is fully implemented, this will return more information.
                // Err(RuntimeError::Trap {
                //     msg: format!("unknown trap at {:p} - {}", faulting_addr, signal).into(),
                // }
                // .into())
                Err(CallProtError::Trap(WasmTrapInfo::Unknown))
            }
        } else {
            let ret = f(); // TODO: Switch stack?
            *jmp_buf = prev_jmp_buf;
            Ok(ret)
        }
    }
}

pub unsafe fn throw(payload: Box<dyn Any>) -> ! {
    let jmp_buf = SETJMP_BUFFER.with(|buf| buf.get());
    if *jmp_buf == [0; SETJMP_BUFFER_LEN] {
        ::std::process::abort();
    }
    TRAP_EARLY_DATA.with(|cell| cell.replace(Some(payload)));
    longjmp(jmp_buf as *mut ::nix::libc::c_void, 0xffff);
}

/// Unwinds to last protected_call.
pub unsafe fn do_unwind(signum: i32, siginfo: *const c_void, ucontext: *const c_void) -> ! {
    // Since do_unwind is only expected to get called from WebAssembly code which doesn't hold any host resources (locks etc.)
    // itself, accessing TLS here is safe. In case any other code calls this, it often indicates a memory safety bug and you should
    // temporarily disable the signal handlers to debug it.

    let jmp_buf = SETJMP_BUFFER.with(|buf| buf.get());
    if *jmp_buf == [0; SETJMP_BUFFER_LEN] {
        ::std::process::abort();
    }

    CAUGHT_FAULTS.with(|cell| cell.set(Some(get_fault_info(siginfo, ucontext))));

    longjmp(jmp_buf as *mut ::nix::libc::c_void, signum)
}

pub struct FaultInfo {
    faulting_addr: *const c_void,
    ip: *const c_void,
    known_registers: [Option<u64>; 24],
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn get_faulting_addr_and_ip(
    siginfo: *const c_void,
    ucontext: *const c_void,
) -> (*const c_void, *const c_void) {
    use libc::{ucontext_t, RIP};

    #[allow(dead_code)]
    #[repr(C)]
    struct siginfo_t {
        si_signo: i32,
        si_errno: i32,
        si_code: i32,
        si_addr: u64,
        // ...
    }

    let siginfo = siginfo as *const siginfo_t;
    let si_addr = (*siginfo).si_addr;

    let ucontext = ucontext as *const ucontext_t;
    let rip = (*ucontext).uc_mcontext.gregs[RIP as usize];

    (si_addr as _, rip as _)
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
unsafe fn get_fault_info(
    siginfo: *const c_void,
    ucontext: *const c_void,
) -> FaultInfo {
    #[allow(dead_code)]
    #[repr(C)]
    struct ucontext_t {
        uc_onstack: u32,
        uc_sigmask: u32,
        uc_stack: libc::stack_t,
        uc_link: *const ucontext_t,
        uc_mcsize: u64,
        uc_mcontext: *const mcontext_t,
    }
    #[repr(C)]
    struct exception_state {
        trapno: u16,
        cpu: u16,
        err: u32,
        faultvaddr: u64,
    }
    #[repr(C)]
    struct regs {
        rax: u64,
        rbx: u64,
        rcx: u64,
        rdx: u64,
        rdi: u64,
        rsi: u64,
        rbp: u64,
        rsp: u64,
        r8: u64,
        r9: u64,
        r10: u64,
        r11: u64,
        r12: u64,
        r13: u64,
        r14: u64,
        r15: u64,
        rip: u64,
        rflags: u64,
        cs: u64,
        fs: u64,
        gs: u64,
    }
    #[allow(dead_code)]
    #[repr(C)]
    struct mcontext_t {
        es: exception_state,
        ss: regs,
        // ...
    }

    let siginfo = siginfo as *const siginfo_t;
    let si_addr = (*siginfo).si_addr;

    let ucontext = ucontext as *const ucontext_t;
    let ss = &(*(*ucontext).uc_mcontext).ss;

    let mut known_registers: [Option<u64>; 24] = [None; 24];

    known_registers[X64Register::GPR(GPR::R15).to_index().0] = Some(ss.r15);
    known_registers[X64Register::GPR(GPR::R14).to_index().0] = Some(ss.r14);
    known_registers[X64Register::GPR(GPR::R13).to_index().0] = Some(ss.r13);
    known_registers[X64Register::GPR(GPR::R12).to_index().0] = Some(ss.r12);
    known_registers[X64Register::GPR(GPR::R11).to_index().0] = Some(ss.r11);
    known_registers[X64Register::GPR(GPR::R10).to_index().0] = Some(ss.r10);
    known_registers[X64Register::GPR(GPR::R9).to_index().0] = Some(ss.r9);
    known_registers[X64Register::GPR(GPR::R8).to_index().0] = Some(ss.r8);
    known_registers[X64Register::GPR(GPR::RSI).to_index().0] = Some(ss.rsi);
    known_registers[X64Register::GPR(GPR::RDI).to_index().0] = Some(ss.rdi);
    known_registers[X64Register::GPR(GPR::RDX).to_index().0] = Some(ss.rdx);
    known_registers[X64Register::GPR(GPR::RCX).to_index().0] = Some(ss.rcx);
    known_registers[X64Register::GPR(GPR::RBX).to_index().0] = Some(ss.rbx);
    known_registers[X64Register::GPR(GPR::RAX).to_index().0] = Some(ss.rax);

    known_registers[X64Register::GPR(GPR::RBP).to_index().0] = Some(ss.rbp);
    known_registers[X64Register::GPR(GPR::RSP).to_index().0] = Some(ss.rsp);

    FaultInfo {
        faulting_addr: si_addr,
        ip: ss.rip as _,
        known_registers,
    }
}
