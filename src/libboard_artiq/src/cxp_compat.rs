// ARTIQ 8 compatibility shim for the CoaXPress firmware.
//
// The ARTIQ 10 CXP modules call the free functions `timer::get_ms()` and
// `timer::async_delay_ms(ms)`, which exist in the ARTIQ 10 libboard_zynq but NOT in the
// ARTIQ 8 one. This module reimplements them on top of the ARTIQ 8 APIs
// (`GlobalTimer` + `libasync::delay`). The CXP modules import it aliased as `timer`
// (`use crate::cxp_compat as timer;`) so their call sites are unchanged.

use libasync::delay;
use libboard_zynq::{time::Milliseconds, timer::GlobalTimer};

/// Milliseconds of uptime.
///
/// Uses the already-running global timer via the non-resetting accessor. NOTE:
/// `GlobalTimer::start()` *resets* the counter, so it must NOT be used here — the timer
/// is started once at boot by the runtime, and `GlobalTimer::get()` returns a handle to
/// it without disturbing the count.
pub fn get_ms() -> u64 {
    unsafe { GlobalTimer::get() }.get_time().0
}

/// Cooperative async delay of `ms` milliseconds (yields to the executor).
pub async fn async_delay_ms(ms: u64) {
    let timer = unsafe { GlobalTimer::get() };
    let mut countdown = timer.countdown();
    delay(&mut countdown, Milliseconds(ms)).await;
}
