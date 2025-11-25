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

    /// Standard deviation over last N samples. N must be >=1 and <=len.
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

/// Naive reference calculation for sum of squares, which here means
/// the sum of the squared differences between data values and the mean.
///
/// If `is_sample` == `true`, uses Bessel's correction (N-1 divisor),
/// meaning the result is the sample variance; otherwise uses
/// N divisor (population variance).
pub fn sum_of_squares(data: &[u32], is_sample: bool) -> f64 {
    #[cfg(test)]
    {
        eprintln!("sum_of_squares: data={data:?}, is_sample={is_sample}");
    }
    let len: f64 = data.len() as f64;
    let divisor: f64 = if is_sample {
        len - 1.0
    } else {
        len
    };
    let mean: f64 = data.iter().sum::<u32>() as f64 / len;
    data.iter().map(|&x| {
        (x as f64 - mean).powf(2.0)
    }).sum::<f64>() / divisor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic() {
        let mut lw: LatencyWindow = LatencyWindow::new(1);

        // basic empty checks
        assert!(lw.is_empty());
        assert_eq!(lw.len(), 0);
        assert_eq!(lw.maxlen(), 3);
        assert!(lw.last().is_err());
        assert!(lw.variance().is_err());
        assert!(lw.stdev().is_err());
        assert!(lw.mean_min_max().is_err());

        lw.push(10); // 10ms
        assert!(! lw.is_empty());
        assert_eq!(lw.len(), 1);
        assert_eq!(lw.last().unwrap(), 10);
        assert_eq!(lw.mean_min_max().unwrap(), (10.0, 10, 10));
        assert_eq!(lw.variance().unwrap(), 0.0);
        assert_eq!(lw.stdev().unwrap(), 0.0);
    }

    #[test]
    fn test_push() {
        let mut lw: LatencyWindow = LatencyWindow::new(4);
        let data = [10, 20, 30, 40];

        /////// first 2 pushes ////////
        lw.push(10);
        lw.push(20);
        assert_eq!(lw.last().unwrap(), 20, "Wrong last() after 2 pushes");
        assert_eq!(lw.mean_min_max().unwrap(), (15.0, 10, 20), "Wrong mean/min/max after 2 pushes");

        let exp_var: f64 = sum_of_squares(&data[..=1], false);
        assert_eq!(lw.variance().unwrap(), exp_var, "Wrong population variance after 2 pushes");
        assert_eq!(lw.stdev().unwrap(), 5.0, "Wrong population stdev after 2 pushes");

        let exp_var: f64 = sum_of_squares(&data[..=1], true);
        assert_eq!(lw.stdev_n(2).unwrap(), exp_var.sqrt(), "Wrong sample stdev(2) after 2 pushes");

        /////// 3 pushes ////////
        lw.push(30);
        assert_eq!(lw.last().unwrap(), 30, "Wrong last() after 3 pushes");
        assert_eq!(lw.mean_min_max().unwrap(), (20.0, 10, 30), "Wrong mean/min/max after 3 pushes");

        let exp_var: f64 = sum_of_squares(&data[..=2], false);
        assert_eq!(lw.variance().unwrap(), exp_var, "Wrong population variance after 3 pushes");
        assert_eq!(lw.stdev().unwrap(), exp_var.sqrt(), "Wrong population stdev after 3 pushes");

        let exp_var: f64 = sum_of_squares(&data[1..=2], true);
        assert_eq!(lw.stdev_n(2).unwrap(), exp_var.sqrt(), "Wrong sample stdev(2) after 3 pushes");
        let exp_var: f64 = sum_of_squares(&data[..=2], true);
        assert_eq!(lw.stdev_n(3).unwrap(), exp_var.sqrt(), "Wrong sample stdev(3) after 3 pushes");

        //////// 4 pushes ////////
        lw.push(40);
        assert_eq!(lw.last().unwrap(), 40, "Wrong last() after 4 pushes");
        assert_eq!(lw.mean_min_max().unwrap(), (25.0, 10, 40), "Wrong mean/min/max after 4 pushes");

        let exp_var: f64 = sum_of_squares(&data, false);
        assert_eq!(lw.variance().unwrap(), exp_var, "Wrong population variance after 4 pushes");
        assert_eq!(lw.stdev().unwrap(), exp_var.sqrt(), "Wrong population stdev after 4 pushes");

        let exp_var: f64 = sum_of_squares(&data[2..=3], true);
        assert_eq!(lw.stdev_n(2).unwrap(), exp_var.sqrt(), "Wrong sample stdev(2) after 4 pushes");
        let exp_var: f64 = sum_of_squares(&data[1..=3], true);
        assert_eq!(lw.stdev_n(3).unwrap(), exp_var.sqrt(), "Wrong sample stdev(3) after 4 pushes");
        let exp_var: f64 = sum_of_squares(&data, true);
        assert_eq!(lw.stdev_n(4).unwrap(), exp_var.sqrt(), "Wrong sample stdev(4) after 4 pushes");
    }

    #[test]
    fn test_eviction() {
        let mut lw: LatencyWindow = LatencyWindow::new(3);
        let data = [20, 30, 40];

        /////// first 3 pushes should fit, 4th should overflow and rotate ////////
        lw.push(10);
        assert_eq!(lw.len(), 1, "Wrong len() after 1 push, should be 1");
        lw.push(20);
        assert_eq!(lw.len(), 2, "Wrong len() after 2 pushes, should be 2");
        lw.push(30);
        assert_eq!(lw.len(), 3, "Wrong len() after 3 pushes, should be 3");
        lw.push(40);
        assert_eq!(lw.len(), 3, "Wrong len() after 4 pushes, should be 3");
        assert_eq!(lw.last().unwrap(), 40, "Wrong last() after 4 pushes");
        assert_eq!(lw.mean_min_max().unwrap(), (30.0, 20, 40), "Wrong mean/min/max, should have evicted 10");

        let exp_var: f64 = sum_of_squares(&data, false);
        assert_eq!(lw.variance().unwrap(), exp_var, "Wrong population variance after eviction");
        assert_eq!(lw.stdev().unwrap(), exp_var.sqrt(), "Wrong population stdev after eviction");

        let exp_var: f64 = sum_of_squares(&data[1..=2], true);
        assert_eq!(lw.stdev_n(2).unwrap(), exp_var.sqrt(), "Wrong sample stdev(2) after eviction");
        let exp_var: f64 = sum_of_squares(&data, true);
        assert_eq!(lw.stdev_n(3).unwrap(), exp_var.sqrt(), "Wrong sample stdev(3) after eviction");
    }
}
