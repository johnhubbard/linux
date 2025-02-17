// SPDX-License-Identifier: GPL-2.0

//! Nova Core Timer subdevice

use core::time::Duration;

use kernel::num::U64Ext;
use kernel::prelude::*;

use crate::driver::Bar0;
use crate::regs;

pub(crate) struct Timer {}

impl Timer {
    pub(crate) fn new() -> Self {
        Self {}
    }

    pub(crate) fn read(bar: &Bar0) -> u64 {
        loop {
            let hi = regs::PtimerTime1::read(bar);
            let lo = regs::PtimerTime0::read(bar);

            if hi == regs::PtimerTime1::read(bar) {
                return u64::from_u32s(hi.hi(), lo.lo());
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn time(bar: &Bar0, time: u64) {
        let (hi, lo) = time.into_u32s();

        regs::PtimerTime1::write(bar, hi);
        regs::PtimerTime0::write(bar, lo);
    }

    /// Wait until `cond` is true or `timeout` elapsed, based on GPU time.
    ///
    /// When `cond` evaluates to `Some`, its return value is returned.
    ///
    /// `Err(ETIMEDOUT)` is returned if `timeout` has been reached without `cond` evaluating to
    /// `Some`, or if the timer device is stuck for some reason.
    pub(crate) fn wait_on<R, F: Fn() -> Option<R>>(
        &self,
        bar: &Bar0,
        timeout: Duration,
        cond: F,
    ) -> Result<R> {
        // Number of consecutive time reads after which we consider the timer frozen if it hasn't
        // moved forward.
        const MAX_STALLED_READS: usize = 16;

        let (mut cur_time, mut prev_time, deadline) = (|| {
            let cur_time = Timer::read(bar);
            let deadline =
                cur_time.saturating_add(u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX));

            (cur_time, cur_time, deadline)
        })();
        let mut num_reads = 0;

        loop {
            if let Some(ret) = cond() {
                return Ok(ret);
            }

            (|| {
                cur_time = Timer::read(bar);

                /* Check if the timer is frozen for some reason. */
                if cur_time == prev_time {
                    if num_reads >= MAX_STALLED_READS {
                        return Err(ETIMEDOUT);
                    }
                    num_reads += 1;
                } else {
                    if cur_time >= deadline {
                        return Err(ETIMEDOUT);
                    }

                    num_reads = 0;
                    prev_time = cur_time;
                }

                Ok(())
            })()?;
        }
    }
}
