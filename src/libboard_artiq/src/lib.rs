#![no_std]
#![feature(never_type)]
#![feature(naked_functions)]
#![feature(asm)]

extern crate alloc;
extern crate core_io;
extern crate crc;
extern crate embedded_hal;
extern crate io;
extern crate libasync;
extern crate libboard_zynq;
extern crate libconfig;
extern crate libcortex_a9;
extern crate libregister;
extern crate log;
extern crate log_buffer;

pub mod drtio_routing;
#[cfg(has_drtio)]
pub mod drtioaux;
#[cfg(has_drtio)]
pub mod drtioaux_async;
pub mod drtioaux_proto;
pub mod fiq;
#[cfg(feature = "target_kasli_soc")]
pub mod io_expander;
pub mod logger;
#[cfg(any(has_drtio, has_cxp_grabber))]
#[rustfmt::skip]
#[path = "../../../build/mem.rs"]
pub mod mem;
#[rustfmt::skip]
#[path = "../../../build/pl.rs"]
pub mod pl;
#[cfg(has_drtio_eem)]
pub mod drtio_eem;
#[cfg(has_grabber)]
pub mod grabber;
#[cfg(has_cxp_grabber)]
pub mod cxp_camera_setup;
#[cfg(has_cxp_grabber)]
pub mod cxp_compat;
#[cfg(has_cxp_grabber)]
pub mod cxp_ctrl;
#[cfg(has_cxp_grabber)]
pub mod cxp_grabber;
#[cfg(all(has_cxp_grabber, has_cxp_led))]
pub mod cxp_led;
#[cfg(has_cxp_grabber)]
pub mod cxp_packet;
#[cfg(has_cxp_grabber)]
pub mod cxp_phys;
#[cfg(has_si5324)]
pub mod si5324;
#[cfg(has_si549)]
pub mod si549;
use core::{cmp, str};

pub fn identifier_read(buf: &mut [u8]) -> &str {
    unsafe {
        pl::csr::identifier::address_write(0);
        let len = pl::csr::identifier::data_read();
        let len = cmp::min(len, buf.len() as u8);
        for i in 0..len {
            pl::csr::identifier::address_write(1 + i);
            buf[i as usize] = pl::csr::identifier::data_read();
        }
        str::from_utf8_unchecked(&buf[..len as usize])
    }
}
