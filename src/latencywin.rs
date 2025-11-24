// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{cmp::max, collections::VecDeque};

/// O(1) amortized rolling latency window over the last N samples.
#[derive(Debug)]
pub struct LatencyWindow {
    cap: usize,
    buf: Vec<u32>,                  // ring buffer of values
    head: usize,                    // next write position
    len: usize,
    sum: f64,                       // running sum
    sum_sq: f64,                    // running sum of squares
    variance: f64,                  // running population variance (M2 / N)
    stdev: f64,                     // running population standard deviation as f64
    minq: VecDeque<(u32, usize)>,   // monotonic increasing (value, index)
    maxq: VecDeque<(u32, usize)>,   // monotonic decreasing (value, index)
    index: usize,                   // monotonically increasing sample index
}

impl LatencyWindow {
    /// Create new LatencyWindow with capacity `cap` (clamped to 3 minimum).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: max(cap, 3),
            buf: vec![0; cap],
            head: 0,
            len: 0,
            sum: 0.0,
            sum_sq: 0.0,
            variance: 0.0,
            stdev: 0.0,
            minq: VecDeque::new(),
            maxq: VecDeque::new(),
            index: 0,
        }
    }

    /// Push a new value, evicting oldest if full.
    pub fn push(&mut self, val: u32) {
        let idx: usize = self.index;
        self.index = self.index.wrapping_add(1);
        let val_f: f64 = val as f64;

        if self.len < self.cap {
            // Growing
            self.buf[self.head] = val;
            self.head = (self.head + 1) % self.cap;
            self.len += 1;
            self.sum += val_f;
            self.sum_sq += val_f * val_f;
        } else {
            // Evict oldest at head
            let tail_pos: usize = self.head;
            let old: f64 = self.buf[tail_pos] as f64;
            self.buf[tail_pos] = val;
            self.head = (self.head + 1) % self.cap;
            self.sum += val_f - old;
            self.sum_sq += val_f * val_f - old * old;

            // The global “logical index” of the evicted element is idx - cap,
            // but we only track indices of pushed elements in queues;
            // we’ll drop out-of-range by age below.
        }

        // Compute population variance and stdev
        if self.len > 1 {
            let len_f: f64 = self.len as f64;
            self.variance = (self.sum_sq - (self.sum * self.sum / len_f)) / len_f;
            self.stdev = self.variance.sqrt();
        }

        // Drop aged-out heads *before* adding new
        let cutoff: usize = idx.saturating_sub(self.cap.saturating_sub(1));
        while let Some(&(_, i)) = self.minq.front() {
            if i < cutoff {
                self.minq.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(_, i)) = self.maxq.front() {
            if i < cutoff {
                self.maxq.pop_front();
            } else {
                break;
            }
        }

        // Update min deque (pop larger tails)
        while let Some(&(v, _)) = self.minq.back() {
            if v > val {
                self.minq.pop_back();
            } else {
                break;
            }
        }
        self.minq.push_back((val, idx));

        // Update max deque (pop smaller tails)
        while let Some(&(v, _)) = self.maxq.back() {
            if v < val {
                self.maxq.pop_back();
            } else {
                break;
            }
        }
        self.maxq.push_back((val, idx));
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn maxlen(&self) -> usize {
        self.cap
    }

    #[inline]
    fn no_samples_check(&self) -> Result<(), String> {
        if self.is_empty() {
            return Err("no samples".into());
        }
        Ok(())
    }

    #[inline]
    fn float_val_check(&self, val: f64) -> Result<(), String> {
        if val.is_nan() || val.is_infinite() {
            return Err("invalid float (NaN or infinite)".into());
        }
        Ok(())
    }

    /// Latest latency sample.
    pub fn last(&self) -> Result<u32, String> {
        self.no_samples_check()?;
        let last_idx: usize = (self.head + self.cap - 1) % self.cap;
        Ok(self.buf[last_idx])
    }

    /// Population variance [M2 / N] (running total over all samples).
    pub fn variance(&self) -> Result<f64, String> {
        self.no_samples_check()?;
        self.float_val_check(self.variance)?;
        Ok(self.variance)
    }

    /// Standard population deviation (running total over all samples).
    pub fn stdev(&self) -> Result<f64, String> {
        self.no_samples_check()?;
        self.float_val_check(self.stdev)?;
        Ok(self.stdev)
    }

    /// Standard deviation in ms over last N samples. N must be >=1 and <=len.
    pub fn stdev_n(&self, n: usize) -> Result<f64, String> {
        self.no_samples_check()?;
        if n == 1 {
            return Ok(0.0);
        }
        if n < 1 || n > self.len {
            return Err("invalid sample count".into());
        }
        let mut var: f64 = 0.0;
        let mut sub_sum: f64 = 0.0;
        for i in 0..n {
            let idx: usize = (self.head + self.cap - 1 - i) % self.cap;
            sub_sum += self.buf[idx] as f64;
        }
        let sub_mean: f64 = sub_sum / n as f64;
        for i in 0..n {
            let idx: usize = (self.head + self.cap - 1 - i) % self.cap;
            let v: f64 = self.buf[idx] as f64;
            var += (v - sub_mean).powi(2);
        }
        var /= (n as f64) - 1.0;  // sample variance (N-1)
        self.float_val_check(var)?;
        Ok(var.sqrt())
    }

    /// Mean/min/max values.
    pub fn mean_min_max(&self) -> Result<(f64, u32, u32), String> {
        self.no_samples_check()?;
        let mean: f64 = self.sum / self.len as f64;
        let min: u32 = self.minq.front().map(|(v, _)| *v).unwrap_or_default();
        let max: u32 = self.maxq.front().map(|(v, _)| *v).unwrap_or_default();
        Ok((mean, min, max))
    }
}
