use alloc::vec;
use core::{ffi::VaList, ptr, str};

use libc::{c_char, c_int, size_t};
use libm;
use log::{info, warn};

#[cfg(has_drtio)]
use super::subkernel;
use super::{cache,
            core1::rtio_get_destination_status,
            dma, i2c,
            rpc::{rpc_recv, rpc_send, rpc_send_async},
            rtio};
use crate::eh_artiq;

extern "C" {
    fn vsnprintf_(buffer: *mut c_char, count: size_t, format: *const c_char, va: VaList) -> c_int;
}

unsafe extern "C" fn core_log(fmt: *const c_char, mut args: ...) {
    let size = vsnprintf_(ptr::null_mut(), 0, fmt, args.as_va_list()) as usize;
    let mut buf = vec![0; size + 1];
    vsnprintf_(buf.as_mut_ptr() as *mut i8, size + 1, fmt, args.as_va_list());
    let buf: &[u8] = &buf.as_slice()[..size - 1]; // strip \n and NUL
    match str::from_utf8(buf) {
        Ok(s) => info!("kernel: {}", s),
        Err(e) => {
            info!("kernel: {}", (str::from_utf8(&buf[..e.valid_up_to()]).unwrap()));
            warn!("kernel: invalid utf-8");
        }
    }
}

unsafe extern "C" fn rtio_log(fmt: *const c_char, mut args: ...) {
    let size = vsnprintf_(ptr::null_mut(), 0, fmt, args.as_va_list()) as usize;
    let mut buf = vec![0; size + 1];
    vsnprintf_(buf.as_mut_ptr(), size + 1, fmt, args.as_va_list());
    rtio::write_log(buf.as_slice());
}

macro_rules! api {
    ($i:ident) => ({
        extern { static $i: u8; }
        unsafe { api!($i = &$i as *const _) }
    });
    ($i:ident, $d:item) => ({
        $d
        api!($i = $i)
    });
    ($i:ident = $e:expr) => {
        (stringify!($i), $e as *const ())
    }
}

macro_rules! api_libm_f64f64 {
    ($i:ident) => {{
        extern "C" fn $i(x: f64) -> f64 {
            libm::$i(x)
        }
        api!($i = $i)
    }};
}

macro_rules! api_libm_f64f64f64 {
    ($i:ident) => {{
        extern "C" fn $i(x: f64, y: f64) -> f64 {
            libm::$i(x, y)
        }
        api!($i = $i)
    }};
}

pub fn resolve(required: &[u8]) -> Option<u32> {
    #[rustfmt::skip]
    let api = &[
        // timing
        api!(now_mu = rtio::now_mu),
        api!(at_mu = rtio::at_mu),
        api!(delay_mu = rtio::delay_mu),

        // rpc
        api!(rpc_send = rpc_send),
        api!(rpc_send_async = rpc_send_async),
        api!(rpc_recv = rpc_recv),

        // rtio
        api!(rtio_init = rtio::init),
        api!(rtio_get_destination_status = rtio_get_destination_status),
        api!(rtio_get_counter = rtio::get_counter),
        api!(rtio_output = rtio::output),
        api!(rtio_output_wide = rtio::output_wide),
        api!(rtio_input_timestamp = rtio::input_timestamp),
        api!(rtio_input_data = rtio::input_data),
        api!(rtio_input_timestamped_data = rtio::input_timestamped_data),

        // log
        api!(core_log = core_log),
        api!(rtio_log = rtio_log),

        // rtio dma
        api!(dma_record_start = dma::dma_record_start),
        api!(dma_record_stop = dma::dma_record_stop),
        api!(dma_erase = dma::dma_erase),
        api!(dma_retrieve = dma::dma_retrieve),
        api!(dma_playback = dma::dma_playback),

        // cache
        api!(cache_get = cache::get),
        api!(cache_put = cache::put),

        // i2c
        api!(i2c_start = i2c::start),
        api!(i2c_restart = i2c::restart),
        api!(i2c_stop = i2c::stop),
        api!(i2c_write = i2c::write),
        api!(i2c_read = i2c::read),
        api!(i2c_switch_select = i2c::switch_select),

        // subkernel
        #[cfg(has_drtio)]
        api!(subkernel_load_run = subkernel::load_run),
        #[cfg(has_drtio)]
        api!(subkernel_await_finish = subkernel::await_finish),
        #[cfg(has_drtio)]
        api!(subkernel_send_message = subkernel::send_message),
        #[cfg(has_drtio)]
        api!(subkernel_await_message = subkernel::await_message),

        // Double-precision floating-point arithmetic helper functions
        // RTABI chapter 4.1.2, Table 2
        api!(__aeabi_dadd),
        api!(__aeabi_ddiv),
        api!(__aeabi_dmul),
        api!(__aeabi_dsub),

        // Double-precision floating-point comparison helper functions
        // RTABI chapter 4.1.2, Table 3
        api!(__aeabi_dcmpeq),
        api!(__aeabi_dcmpeq),
        api!(__aeabi_dcmplt),
        api!(__aeabi_dcmple),
        api!(__aeabi_dcmpge),
        api!(__aeabi_dcmpgt),
        api!(__aeabi_dcmpun),

        // Single-precision floating-point arithmetic helper functions
        // RTABI chapter 4.1.2, Table 4
        api!(__aeabi_fadd),
        api!(__aeabi_fdiv),
        api!(__aeabi_fmul),
        api!(__aeabi_fsub),

        // Single-precision floating-point comparison helper functions
        // RTABI chapter 4.1.2, Table 5
        api!(__aeabi_fcmpeq),
        api!(__aeabi_fcmpeq),
        api!(__aeabi_fcmplt),
        api!(__aeabi_fcmple),
        api!(__aeabi_fcmpge),
        api!(__aeabi_fcmpgt),
        api!(__aeabi_fcmpun),

        // Floating-point to integer conversions.
        // RTABI chapter 4.1.2, Table 6
        api!(__aeabi_d2iz),
        api!(__aeabi_d2uiz),
        api!(__aeabi_d2lz),
        api!(__aeabi_d2ulz),
        api!(__aeabi_f2iz),
        api!(__aeabi_f2uiz),
        api!(__aeabi_f2lz),
        api!(__aeabi_f2ulz),

        // Conversions between floating types.
        // RTABI chapter 4.1.2, Table 7
        api!(__aeabi_f2d),

        // Integer to floating-point conversions.
        // RTABI chapter 4.1.2, Table 8
        api!(__aeabi_i2d),
        api!(__aeabi_ui2d),
        api!(__aeabi_l2d),
        api!(__aeabi_ul2d),
        api!(__aeabi_i2f),
        api!(__aeabi_ui2f),
        api!(__aeabi_l2f),
        api!(__aeabi_ul2f),

        // Long long helper functions
        // RTABI chapter 4.2, Table 9
        api!(__aeabi_lmul),
        api!(__aeabi_llsl),
        api!(__aeabi_llsr),
        api!(__aeabi_lasr),

        // Integer division functions
        // RTABI chapter 4.3.1
        api!(__aeabi_idiv),
        api!(__aeabi_ldivmod),
        api!(__aeabi_idivmod),
        api!(__aeabi_uidiv),
        api!(__aeabi_uldivmod),
        api!(__aeabi_uidivmod),

        // 4.3.4 Memory copying, clearing, and setting
        api!(__aeabi_memcpy8),
        api!(__aeabi_memcpy4),
        api!(__aeabi_memcpy),
        api!(__aeabi_memmove8),
        api!(__aeabi_memmove4),
        api!(__aeabi_memmove),
        api!(__aeabi_memset8),
        api!(__aeabi_memset4),
        api!(__aeabi_memset),
        api!(__aeabi_memclr8),
        api!(__aeabi_memclr4),
        api!(__aeabi_memclr),

        // libc
        api!(
            memcpy,
            extern "C" {
                fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8;
            }
        ),
        api!(
            memmove,
            extern "C" {
                fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8;
            }
        ),
        api!(
            memset,
            extern "C" {
                fn memset(s: *mut u8, c: i32, n: usize) -> *mut u8;
            }
        ),
        api!(
            memcmp,
            extern "C" {
                fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32;
            }
        ),

        // exceptions
        api!(_Unwind_Resume = unwind::_Unwind_Resume),
        api!(__nac3_personality = eh_artiq::artiq_personality),
        api!(__nac3_raise = eh_artiq::raise),
        api!(__nac3_resume = eh_artiq::resume),
        api!(__nac3_end_catch = eh_artiq::end_catch),

        // legacy exception symbols
        api!(__artiq_personality = eh_artiq::artiq_personality),
        api!(__artiq_raise = eh_artiq::raise),
        api!(__artiq_resume = eh_artiq::resume),
        api!(__artiq_end_catch = eh_artiq::end_catch),

        // Implementations for LLVM math intrinsics
        api!(__powidf2),

        // libm
        api_libm_f64f64!(acos),
        api_libm_f64f64!(acosh),
        api_libm_f64f64!(asin),
        api_libm_f64f64!(asinh),
        api_libm_f64f64!(atan),
        api_libm_f64f64f64!(atan2),
        api_libm_f64f64!(atanh),
        api_libm_f64f64!(cbrt),
        api_libm_f64f64!(ceil),
        api_libm_f64f64f64!(copysign),
        api_libm_f64f64!(cos),
        api_libm_f64f64!(cosh),
        api_libm_f64f64!(erf),
        api_libm_f64f64!(erfc),
        api_libm_f64f64!(exp),
        api_libm_f64f64!(exp2),
        api_libm_f64f64!(exp10),
        api_libm_f64f64!(expm1),
        api_libm_f64f64!(fabs),
        api_libm_f64f64!(floor),
        {
            extern "C" fn fma(x: f64, y: f64, z: f64) -> f64 {
                libm::fma(x, y, z)
            }
            api!(fma = fma)
        },
        api_libm_f64f64f64!(fmax),
        api_libm_f64f64f64!(fmin),
        api_libm_f64f64f64!(fmod),
        api_libm_f64f64f64!(hypot),
        api_libm_f64f64!(j0),
        api_libm_f64f64!(j1),
        {
            extern "C" fn jn(n: i32, x: f64) -> f64 {
                libm::jn(n, x)
            }
            api!(jn = jn)
        },
        api_libm_f64f64!(lgamma),
        api_libm_f64f64!(log),
        api_libm_f64f64!(log2),
        api_libm_f64f64!(log10),
        api_libm_f64f64f64!(nextafter),
        api_libm_f64f64f64!(pow),
        api_libm_f64f64!(round),
        api_libm_f64f64!(rint),
        api_libm_f64f64!(sin),
        api_libm_f64f64!(sinh),
        api_libm_f64f64!(sqrt),
        api_libm_f64f64!(tan),
        api_libm_f64f64!(tanh),
        api_libm_f64f64!(tgamma),
        api_libm_f64f64!(trunc),
        api_libm_f64f64!(y0),
        api_libm_f64f64!(y1),
        {
            extern "C" fn yn(n: i32, x: f64) -> f64 {
                libm::yn(n, x)
            }
            api!(yn = yn)
        },
        {
            extern "C" fn sincos(x: f64, sin: &mut f64, cos: &mut f64) {
                let (sin_, cos_) = libm::sincos(x);
                *sin = sin_;
                *cos = cos_;
            }
            api!(sincos = sincos)
        },
        /*
         * syscall for unit tests
         * Used in `artiq.tests.coredevice.test_exceptions.ExceptionTest.test_raise_exceptions_kernel`
         * This syscall checks that the exception IDs used in the Python `EmbeddingMap` (in `artiq.language.embedding`)
         * match the `EXCEPTION_ID_LOOKUP` defined in the firmware (`libksupport::src::eh_artiq`)
         */
        api!(test_exception_id_sync = eh_artiq::test_exception_id_sync)
    ];
    api.iter()
        .find(|&&(exported, _)| exported.as_bytes() == required)
        .map(|&(_, ptr)| ptr as u32)
}
