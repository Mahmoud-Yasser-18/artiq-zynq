// From Current artiq firmware ksupport implementation.
// Modified to suit the case of artiq-zynq port, for ARM EHABI.
// Portions of the code in this file are derived from code by:
//
// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
#![allow(non_camel_case_types)]

use core::mem;

use core_io::Error as ReadError;
use cslice::{AsCSlice, CSlice};
use dwarf::eh::{self, EHAction, EHContext};
use io::{Cursor, ProtoRead};
use libc::{c_int, c_void, uintptr_t};
use log::{error, trace};
use unwind as uw;

use crate::kernel::KERNEL_IMAGE;

const EXCEPTION_CLASS: uw::_Unwind_Exception_Class = 0x4d_4c_42_53_41_52_54_51; /* 'MLBSARTQ' */

#[cfg(target_arch = "arm")]
const UNWIND_DATA_REG: (i32, i32) = (0, 1); // R0, R1

// Note: CSlice within an exception may not be actual cslice, they may be strings that exist only
// in the host. If the length == usize:MAX, the pointer is actually a string key in the host.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Exception<'a> {
    pub id: u32,
    pub file: CSlice<'a, u8>,
    pub line: u32,
    pub column: u32,
    pub function: CSlice<'a, u8>,
    pub message: CSlice<'a, u8>,
    pub param: [i64; 3],
}

fn str_err(_: core::str::Utf8Error) -> core::fmt::Error {
    core::fmt::Error
}

fn exception_str<'a>(s: &'a CSlice<'a, u8>) -> Result<&'a str, core::str::Utf8Error> {
    if s.len() == usize::MAX {
        Ok("<host string>")
    } else {
        core::str::from_utf8(s.as_ref())
    }
}

impl<'a> core::fmt::Debug for Exception<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Exception {} from {} in {}:{}:{}, message: {}",
            self.id,
            exception_str(&self.function).map_err(str_err)?,
            exception_str(&self.file).map_err(str_err)?,
            self.line,
            self.column,
            exception_str(&self.message).map_err(str_err)?
        )
    }
}

const MAX_INFLIGHT_EXCEPTIONS: usize = 10;
const MAX_BACKTRACE_SIZE: usize = 128;

#[derive(Debug, Default)]
pub struct StackPointerBacktrace {
    pub stack_pointer: usize,
    pub initial_backtrace_size: usize,
    pub current_backtrace_size: usize,
}

struct ExceptionBuffer {
    // we need n _Unwind_Exception, because each will have their own private data
    uw_exceptions: [uw::_Unwind_Exception; MAX_INFLIGHT_EXCEPTIONS],
    exceptions: [Option<Exception<'static>>; MAX_INFLIGHT_EXCEPTIONS + 1],
    exception_stack: [isize; MAX_INFLIGHT_EXCEPTIONS + 1],
    // nested exceptions will share the backtrace buffer, treated as a tree
    // backtrace contains a tuple of IP and SP
    backtrace: [(usize, usize); MAX_BACKTRACE_SIZE],
    backtrace_size: usize,
    // stack pointers are stored to reconstruct backtrace for each exception
    stack_pointers: [StackPointerBacktrace; MAX_INFLIGHT_EXCEPTIONS + 1],
    // current allocated nested exceptions
    exception_count: usize,
}

static mut EXCEPTION_BUFFER: ExceptionBuffer = ExceptionBuffer {
    uw_exceptions: [uw::_Unwind_Exception {
        exception_class: EXCEPTION_CLASS,
        exception_cleanup: cleanup,
        private: [0; uw::unwinder_private_data_size],
    }; MAX_INFLIGHT_EXCEPTIONS],
    exceptions: [None; MAX_INFLIGHT_EXCEPTIONS + 1],
    exception_stack: [-1; MAX_INFLIGHT_EXCEPTIONS + 1],
    backtrace: [(0, 0); MAX_BACKTRACE_SIZE],
    backtrace_size: 0,
    stack_pointers: [StackPointerBacktrace {
        stack_pointer: 0,
        initial_backtrace_size: 0,
        current_backtrace_size: 0,
    }; MAX_INFLIGHT_EXCEPTIONS + 1],
    exception_count: 0,
};

pub unsafe extern "C" fn reset_exception_buffer() {
    trace!("reset exception buffer");
    EXCEPTION_BUFFER.uw_exceptions = [uw::_Unwind_Exception {
        exception_class: EXCEPTION_CLASS,
        exception_cleanup: cleanup,
        private: [0; uw::unwinder_private_data_size],
    }; MAX_INFLIGHT_EXCEPTIONS];
    EXCEPTION_BUFFER.exceptions = [None; MAX_INFLIGHT_EXCEPTIONS + 1];
    EXCEPTION_BUFFER.exception_stack = [-1; MAX_INFLIGHT_EXCEPTIONS + 1];
    EXCEPTION_BUFFER.backtrace_size = 0;
    EXCEPTION_BUFFER.exception_count = 0;
}

type _Unwind_Stop_Fn = extern "C" fn(
    version: c_int,
    actions: i32,
    exception_class: uw::_Unwind_Exception_Class,
    exception_object: *mut uw::_Unwind_Exception,
    context: *mut uw::_Unwind_Context,
    stop_parameter: *mut c_void,
) -> uw::_Unwind_Reason_Code;

extern "C" {
    // not defined in EHABI, but LLVM added it and is useful to us
    fn _Unwind_ForcedUnwind(
        exception: *mut uw::_Unwind_Exception,
        stop_fn: _Unwind_Stop_Fn,
        stop_parameter: *mut c_void,
    ) -> uw::_Unwind_Reason_Code;
}

unsafe fn find_eh_action(context: *mut uw::_Unwind_Context, foreign_exception: bool, id: u32) -> Result<EHAction, ()> {
    let lsda = uw::_Unwind_GetLanguageSpecificData(context) as *const u8;
    let mut ip_before_instr: c_int = 0;
    let ip = uw::_Unwind_GetIPInfo(context, &mut ip_before_instr);
    let eh_context = EHContext {
        // The return address points 1 byte past the call instruction,
        // which could be in the next IP range in LSDA range table.
        ip: if ip_before_instr != 0 { ip } else { ip - 1 },
        func_start: uw::_Unwind_GetRegionStart(context),
        get_text_start: &|| uw::_Unwind_GetTextRelBase(context),
        get_data_start: &|| uw::_Unwind_GetDataRelBase(context),
    };
    eh::find_eh_action(lsda, &eh_context, foreign_exception, id)
}

pub unsafe fn artiq_personality(
    _state: uw::_Unwind_State,
    exception_object: *mut uw::_Unwind_Exception,
    context: *mut uw::_Unwind_Context,
) -> uw::_Unwind_Reason_Code {
    // we will only do phase 2 forced unwinding now
    // The DWARF unwinder assumes that _Unwind_Context holds things like the function
    // and LSDA pointers, however ARM EHABI places them into the exception object.
    // To preserve signatures of functions like _Unwind_GetLanguageSpecificData(), which
    // take only the context pointer, GCC personality routines stash a pointer to
    // exception_object in the context, using location reserved for ARM's
    // "scratch register" (r12).
    uw::_Unwind_SetGR(context, uw::UNWIND_POINTER_REG, exception_object as uw::_Unwind_Ptr);
    // ...A more principled approach would be to provide the full definition of ARM's
    // _Unwind_Context in our libunwind bindings and fetch the required data from there
    // directly, bypassing DWARF compatibility functions.

    let exception_class = (*exception_object).exception_class;
    let foreign_exception = exception_class != EXCEPTION_CLASS;
    assert!(!foreign_exception, "we do not expect foreign exceptions");
    let index = EXCEPTION_BUFFER.exception_stack[EXCEPTION_BUFFER.exception_count - 1];
    assert!(index != -1);
    let exception = EXCEPTION_BUFFER.exceptions[index as usize].as_ref().unwrap();

    let id = exception.id;
    let eh_action = match find_eh_action(context, foreign_exception, id) {
        Ok(action) => action,
        Err(_) => return uw::_URC_FAILURE,
    };
    match eh_action {
        EHAction::None => return continue_unwind(exception_object, context),
        EHAction::Cleanup(lpad) | EHAction::Catch(lpad) => {
            uw::_Unwind_SetGR(context, UNWIND_DATA_REG.0, exception_object as uintptr_t);
            uw::_Unwind_SetGR(context, UNWIND_DATA_REG.1, exception as *const _ as uw::_Unwind_Word);
            uw::_Unwind_SetIP(context, lpad);
            return uw::_URC_INSTALL_CONTEXT;
        }
        EHAction::Terminate => return uw::_URC_FAILURE,
    }

    // On ARM EHABI the personality routine is responsible for actually
    // unwinding a single stack frame before returning (ARM EHABI Sec. 6.1).
    unsafe fn continue_unwind(
        exception_object: *mut uw::_Unwind_Exception,
        context: *mut uw::_Unwind_Context,
    ) -> uw::_Unwind_Reason_Code {
        let reason = __gnu_unwind_frame(exception_object, context);
        if reason == uw::_URC_NO_REASON {
            uw::_URC_CONTINUE_UNWIND
        } else {
            reason
        }
    }
    // defined in libgcc
    extern "C" {
        fn __gnu_unwind_frame(
            exception_object: *mut uw::_Unwind_Exception,
            context: *mut uw::_Unwind_Context,
        ) -> uw::_Unwind_Reason_Code;
    }
}

pub unsafe extern "C" fn raise(exception: *const Exception) -> ! {
    let count = EXCEPTION_BUFFER.exception_count;
    let stack = &mut EXCEPTION_BUFFER.exception_stack;
    let diff = exception as isize - EXCEPTION_BUFFER.exceptions.as_ptr() as isize;
    if 0 <= diff && diff <= (mem::size_of::<Option<Exception>>() * MAX_INFLIGHT_EXCEPTIONS) as isize {
        let index = diff / (mem::size_of::<Option<Exception>>() as isize);
        trace!("reraise at {}", index);

        let mut found = false;
        for i in 0..=MAX_INFLIGHT_EXCEPTIONS + 1 {
            if found {
                if stack[i] == -1 {
                    stack[i - 1] = index;
                    assert!(i == count);
                    break;
                } else {
                    stack[i - 1] = stack[i];
                }
            } else {
                if stack[i] == index {
                    found = true;
                }
            }
        }
        assert!(found);
        let _result = _Unwind_ForcedUnwind(
            &mut EXCEPTION_BUFFER.uw_exceptions[stack[count - 1] as usize],
            stop_fn,
            core::ptr::null_mut(),
        );
    } else {
        if count < MAX_INFLIGHT_EXCEPTIONS {
            trace!("raising exception at level {}", count);
            let exception = &*exception;
            for (i, slot) in EXCEPTION_BUFFER.exceptions.iter_mut().enumerate() {
                // we should always be able to find a slot
                if slot.is_none() {
                    *slot = Some(*mem::transmute::<*const Exception, *const Exception<'static>>(
                        exception,
                    ));
                    EXCEPTION_BUFFER.exception_stack[count] = i as isize;
                    EXCEPTION_BUFFER.uw_exceptions[i].private = [0; uw::unwinder_private_data_size];
                    EXCEPTION_BUFFER.stack_pointers[i] = StackPointerBacktrace {
                        stack_pointer: 0,
                        initial_backtrace_size: EXCEPTION_BUFFER.backtrace_size,
                        current_backtrace_size: 0,
                    };
                    EXCEPTION_BUFFER.exception_count += 1;
                    let _result =
                        _Unwind_ForcedUnwind(&mut EXCEPTION_BUFFER.uw_exceptions[i], stop_fn, core::ptr::null_mut());
                }
            }
        } else {
            error!("too many nested exceptions");
            // TODO: better reporting?
            let exception = Exception {
                id: get_exception_id("RuntimeError"),
                file: file!().as_c_slice(),
                line: line!(),
                column: column!(),
                // https://github.com/rust-lang/rfcs/pull/1719
                function: "__artiq_raise".as_c_slice(),
                message: "too many nested exceptions".as_c_slice(),
                param: [0, 0, 0],
            };
            EXCEPTION_BUFFER.exceptions[MAX_INFLIGHT_EXCEPTIONS] = Some(mem::transmute(exception));
            EXCEPTION_BUFFER.stack_pointers[MAX_INFLIGHT_EXCEPTIONS] = Default::default();
            EXCEPTION_BUFFER.exception_count += 1;
            uncaught_exception()
        }
    }
    unreachable!();
}

fn read_exception_string<'a>(reader: &mut Cursor<&[u8]>) -> Result<CSlice<'a, u8>, ReadError> {
    let len = reader.read_u32()? as usize;
    if len == usize::MAX {
        let data = reader.read_u32()?;
        Ok(unsafe { CSlice::new(data as *const u8, len) })
    } else {
        let pos = reader.position();
        let slice = unsafe {
            let ptr = reader.get_ref().as_ptr().offset(pos as isize);
            CSlice::new(ptr, len)
        };
        reader.set_position(pos + len);
        Ok(slice)
    }
}

fn read_exception(raw_exception: &[u8]) -> Result<Exception, ReadError> {
    let mut reader = Cursor::new(raw_exception);

    let mut byte = reader.read_u8()?;
    // to sync
    while byte != 0x5a {
        byte = reader.read_u8()?;
    }
    // skip sync bytes, 0x09 indicates exception
    while byte != 0x09 {
        byte = reader.read_u8()?;
    }
    let _len = reader.read_u32()?;
    // ignore the remaining exceptions, stack traces etc. - unwinding from another device would be unwise anyway
    Ok(Exception {
        id: reader.read_u32()?,
        message: read_exception_string(&mut reader)?,
        param: [
            reader.read_u64()? as i64,
            reader.read_u64()? as i64,
            reader.read_u64()? as i64,
        ],
        file: read_exception_string(&mut reader)?,
        line: reader.read_u32()?,
        column: reader.read_u32()?,
        function: read_exception_string(&mut reader)?,
    })
}

pub fn raise_raw(raw_exception: &[u8]) -> ! {
    use crate::artiq_raise;
    if let Ok(exception) = read_exception(raw_exception) {
        unsafe { raise(&exception) };
    } else {
        artiq_raise!("SubkernelError", "Error passing exception");
    }
}

pub unsafe extern "C" fn resume() -> ! {
    trace!("resume");
    assert!(EXCEPTION_BUFFER.exception_count != 0);
    let i = EXCEPTION_BUFFER.exception_stack[EXCEPTION_BUFFER.exception_count - 1];
    assert!(i != -1);
    let _result = _Unwind_ForcedUnwind(
        &mut EXCEPTION_BUFFER.uw_exceptions[i as usize],
        stop_fn,
        core::ptr::null_mut(),
    );
    unreachable!()
}

pub unsafe extern "C" fn end_catch() {
    let mut count = EXCEPTION_BUFFER.exception_count;
    assert!(count != 0);
    // we remove all exceptions with SP <= current exception SP
    // i.e. the outer exception escapes the finally block
    let index = EXCEPTION_BUFFER.exception_stack[count - 1] as usize;
    EXCEPTION_BUFFER.exception_stack[count - 1] = -1;
    EXCEPTION_BUFFER.exceptions[index] = None;
    let outer_sp = EXCEPTION_BUFFER.stack_pointers[index].stack_pointer;
    count -= 1;
    for i in (0..count).rev() {
        let index = EXCEPTION_BUFFER.exception_stack[i];
        assert!(index != -1);
        let index = index as usize;
        let sp = EXCEPTION_BUFFER.stack_pointers[index].stack_pointer;
        if sp >= outer_sp {
            break;
        }
        EXCEPTION_BUFFER.exceptions[index] = None;
        EXCEPTION_BUFFER.exception_stack[i] = -1;
        count -= 1;
    }
    EXCEPTION_BUFFER.exception_count = count;
    EXCEPTION_BUFFER.backtrace_size = if count > 0 {
        let index = EXCEPTION_BUFFER.exception_stack[count - 1];
        assert!(index != -1);
        EXCEPTION_BUFFER.stack_pointers[index as usize].current_backtrace_size
    } else {
        0
    };
}

extern "C" fn cleanup(_unwind_code: uw::_Unwind_Reason_Code, _uw_exception: *mut uw::_Unwind_Exception) {
    unimplemented!()
}

fn uncaught_exception() -> ! {
    unsafe {
        // dump way to reorder the stack
        for i in 0..EXCEPTION_BUFFER.exception_count {
            if EXCEPTION_BUFFER.exception_stack[i] != i as isize {
                // find the correct index
                let index = EXCEPTION_BUFFER
                    .exception_stack
                    .iter()
                    .position(|v| *v == i as isize)
                    .unwrap();
                let a = EXCEPTION_BUFFER.exception_stack[index];
                let b = EXCEPTION_BUFFER.exception_stack[i];
                assert!(a != -1 && b != -1);
                core::mem::swap(
                    &mut EXCEPTION_BUFFER.exception_stack[index],
                    &mut EXCEPTION_BUFFER.exception_stack[i],
                );
                core::mem::swap(
                    &mut EXCEPTION_BUFFER.exceptions[a as usize],
                    &mut EXCEPTION_BUFFER.exceptions[b as usize],
                );
                core::mem::swap(
                    &mut EXCEPTION_BUFFER.stack_pointers[a as usize],
                    &mut EXCEPTION_BUFFER.stack_pointers[b as usize],
                );
            }
        }
    }
    unsafe {
        crate::kernel::core1::terminate(
            EXCEPTION_BUFFER.exceptions[..EXCEPTION_BUFFER.exception_count].as_ref(),
            EXCEPTION_BUFFER.stack_pointers[..EXCEPTION_BUFFER.exception_count].as_ref(),
            EXCEPTION_BUFFER.backtrace[..EXCEPTION_BUFFER.backtrace_size].as_mut(),
        )
    }
}

// stop function which would be executed when we unwind each frame
extern "C" fn stop_fn(
    _version: c_int,
    actions: i32,
    _uw_exception_class: uw::_Unwind_Exception_Class,
    _uw_exception: *mut uw::_Unwind_Exception,
    context: *mut uw::_Unwind_Context,
    _stop_parameter: *mut c_void,
) -> uw::_Unwind_Reason_Code {
    unsafe {
        let load_addr = KERNEL_IMAGE.as_ref().unwrap().get_load_addr();
        let backtrace_size = EXCEPTION_BUFFER.backtrace_size;
        // we try to remove unrelated backtrace here to save some buffer size
        if backtrace_size < MAX_BACKTRACE_SIZE {
            let ip = uw::_Unwind_GetIP(context);
            if ip >= load_addr {
                let ip = ip - load_addr;
                let sp = uw::_Unwind_GetGR(context, uw::UNWIND_SP_REG);
                trace!("SP: {:X}, backtrace_size: {}", sp, backtrace_size);
                EXCEPTION_BUFFER.backtrace[backtrace_size] = (ip, sp);
                EXCEPTION_BUFFER.backtrace_size += 1;
                let last_index = EXCEPTION_BUFFER.exception_stack[EXCEPTION_BUFFER.exception_count - 1];
                assert!(last_index != -1);
                let sp_info = &mut EXCEPTION_BUFFER.stack_pointers[last_index as usize];
                sp_info.stack_pointer = sp;
                sp_info.current_backtrace_size = backtrace_size + 1;
            }
        } else {
            trace!("backtrace size exceeded");
        }

        if actions as u32 & uw::_US_END_OF_STACK as u32 != 0 {
            uncaught_exception()
        } else {
            uw::_URC_NO_REASON
        }
    }
}

// Must be kept in sync with preallocate_runtime_exception_names() in `artiq.compiler.embedding`
static EXCEPTION_ID_LOOKUP: [(&str, u32); 13] = [
    ("RuntimeError", 0),
    ("RTIOUnderflow", 1),
    ("RTIOOverflow", 2),
    ("RTIODestinationUnreachable", 3),
    ("DMAError", 4),
    ("I2CError", 5),
    ("CacheError", 6),
    ("SPIError", 7),
    ("ZeroDivisionError", 8),
    ("IndexError", 9),
    ("UnwrapNoneError", 10),
    ("SubkernelError", 11),
    // CoaXPress grabber (ARTIQ 8 backport). Must stay in sync with
    // preallocate_runtime_exception_names() in artiq.compiler.embedding.
    ("CXPError", 12),
];

pub fn get_exception_id(name: &str) -> u32 {
    for (n, id) in EXCEPTION_ID_LOOKUP.iter() {
        if *n == name {
            return *id;
        }
    }
    unimplemented!("unallocated internal exception id")
}

#[macro_export]
macro_rules! artiq_raise {
    ($name:expr, $message:expr, $param0:expr, $param1:expr, $param2:expr) => {{
        use cslice::AsCSlice;
        let name_id = $crate::eh_artiq::get_exception_id($name);
        let message_cl = $message.clone();
        let exn = $crate::eh_artiq::Exception {
            id: name_id,
            file: file!().as_c_slice(),
            line: line!(),
            column: column!(),
            // https://github.com/rust-lang/rfcs/pull/1719
            function: "(Rust function)".as_c_slice(),
            message: message_cl.as_c_slice(),
            param: [$param0, $param1, $param2],
        };
        #[allow(unused_unsafe)]
        unsafe {
            $crate::eh_artiq::raise(&exn)
        }
    }};
    ($name:expr, $message:expr) => {{ artiq_raise!($name, $message, 0, 0, 0) }};
}

/// Takes as input exception id from host
/// Generates a new exception with:
///   * `id` set to `exn_id`
///   * `message` set to corresponding exception name from `EXCEPTION_ID_LOOKUP`
///
/// The message is matched on host to ensure correct exception is being referred
/// This test checks the synchronization of exception ids for runtime errors
#[no_mangle]
pub extern "C" fn test_exception_id_sync(exn_id: u32) {
    let message = EXCEPTION_ID_LOOKUP
        .iter()
        .find_map(|&(name, id)| if id == exn_id { Some(name) } else { None })
        .unwrap_or("unallocated internal exception id");

    let exn = Exception {
        id: exn_id,
        file: file!().as_c_slice(),
        line: 0,
        column: 0,
        function: "test_exception_id_sync".as_c_slice(),
        message: message.as_c_slice(),
        param: [0, 0, 0],
    };
    unsafe { raise(&exn) };
}
