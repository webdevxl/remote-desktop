//! Host monotonic clock in microseconds, on the same timebase as ScreenCaptureKit's
//! `displayTime` (mach absolute time).

use std::sync::OnceLock;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut info) };
        (u64::from(info.numer), u64::from(info.denom.max(1)))
    })
}

pub fn mach_to_us(ticks: u64) -> u64 {
    let (numer, denom) = timebase();
    ((u128::from(ticks) * u128::from(numer)) / (u128::from(denom) * 1000)) as u64
}

pub fn now_us() -> u64 {
    mach_to_us(unsafe { mach_absolute_time() })
}
