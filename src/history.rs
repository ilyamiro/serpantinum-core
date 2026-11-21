//! The last minutes of CPU, memory and temperature, sampled once a second whether or not anybody
//! watches, so a graph has its whole width as soon as it appears. Callers ask for a coarser step and
//! get averages of whole seconds, in buckets fixed to the sample count so every call agrees.

use std::collections::VecDeque;

use serde_json::json;

pub const CAPACITY: usize = 640;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Load {
    pub cpu: u32,
    pub ram_percent: u32,
    pub ram_gb: f64,
    pub temp: u32,
}

#[derive(Debug, Default)]
pub struct History {
    samples: VecDeque<Load>,
    total: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Series {
    /// How many buckets of this step have completed since the daemon started
    pub buckets: u64,
    pub cpu: Vec<f64>,
    pub ram_percent: Vec<f64>,
    pub ram_gb: Vec<f64>,
    pub temp: Vec<f64>,
}

impl Series {
    pub fn json(&self) -> serde_json::Value {
        json!({
            "buckets": self.buckets,
            "cpu": self.cpu,
            "ramPercent": self.ram_percent,
            "ramGb": self.ram_gb,
            "temp": self.temp,
        })
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

impl History {
    pub fn push(&mut self, load: Load) {
        if self.samples.len() == CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(load);
        self.total += 1;
    }

    /// Samples still to come before the next bucket of `step` seconds completes
    pub fn samples_until_bucket(&self, step: usize) -> u64 {
        let step = step.max(1) as u64;
        step - self.total % step
    }

    /// The newest `count` complete buckets of `step` seconds, oldest first
    pub fn series(&self, step: usize, count: usize) -> Series {
        let step = step.max(1) as u64;
        let buckets = self.total / step;
        let oldest = self.total - self.samples.len() as u64;
        let first = oldest.div_ceil(step).max(buckets.saturating_sub(count as u64));
        let mut series = Series { buckets, ..Series::default() };
        for bucket in first..buckets {
            let start = (bucket * step - oldest) as usize;
            let slice = self.samples.range(start..start + step as usize);
            let (mut cpu, mut ram_percent, mut ram_gb, mut temp) = (0.0, 0.0, 0.0, 0.0);
            for load in slice {
                cpu += f64::from(load.cpu);
                ram_percent += f64::from(load.ram_percent);
                ram_gb += load.ram_gb;
                temp += f64::from(load.temp);
            }
            let n = step as f64;
            series.cpu.push(round1(cpu / n));
            series.ram_percent.push(round1(ram_percent / n));
            series.ram_gb.push(round1(ram_gb / n));
            series.temp.push(round1(temp / n));
        }
        series
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(cpu: u32) -> Load {
        Load { cpu, ram_percent: cpu / 2, ram_gb: f64::from(cpu) / 10.0, temp: 40 + cpu }
    }

    #[test]
    fn single_second_steps_return_the_samples_themselves() {
        let mut history = History::default();
        for cpu in [10, 20, 30] {
            history.push(load(cpu));
        }
        let series = history.series(1, 60);
        assert_eq!(series.buckets, 3);
        assert_eq!(series.cpu, [10.0, 20.0, 30.0]);
        assert_eq!(series.ram_percent, [5.0, 10.0, 15.0]);
        assert_eq!(series.ram_gb, [1.0, 2.0, 3.0]);
        assert_eq!(series.temp, [50.0, 60.0, 70.0]);
        assert_eq!(history.series(1, 2).cpu, [20.0, 30.0]);
    }

    #[test]
    fn coarser_steps_average_whole_buckets_and_skip_the_unfinished_one() {
        let mut history = History::default();
        for cpu in [10, 20, 30, 40, 50, 60, 70] {
            history.push(load(cpu));
        }
        let series = history.series(3, 60);
        assert_eq!(series.buckets, 2);
        assert_eq!(series.cpu, [20.0, 50.0]);
        assert_eq!(history.samples_until_bucket(3), 2);
        history.push(load(80));
        history.push(load(90));
        assert_eq!(history.series(3, 60).cpu, [20.0, 50.0, 80.0]);
        assert_eq!(history.samples_until_bucket(3), 3);
    }

    #[test]
    fn buckets_stay_put_while_old_samples_fall_out() {
        let mut history = History::default();
        for i in 0..(CAPACITY as u32 + 5) {
            history.push(load(i % 100));
        }
        let series = history.series(10, 1000);
        assert_eq!(series.buckets, (CAPACITY as u64 + 5) / 10);
        assert_eq!(series.cpu.len(), CAPACITY / 10 - 1);
        assert_eq!(history.series(10, 3).cpu.len(), 3);
        assert_eq!(History::default().series(5, 10), Series::default());
    }
}
