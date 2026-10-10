pub mod audio;
pub mod noise_gate;
pub mod processing;

use crossbeam_queue::ArrayQueue;

pub const RING_SIZE: usize = 48000 * 2;

/// Proven bounded lock-free queue: callbacks never wait for a UI or audio lock.
pub struct SpscRing {
    samples: ArrayQueue<f32>,
}

impl SpscRing {
    pub fn new(size: usize) -> Self {
        Self {
            samples: ArrayQueue::new(size),
        }
    }
    /// Call only after stopping the producer and consumer streams.
    pub fn reset(&self) {
        while self.pop().is_some() {}
    }
    pub fn push(&self, sample: f32) -> bool {
        self.samples.push(sample).is_ok()
    }
    pub fn pop(&self) -> Option<f32> {
        self.samples.pop()
    }
    pub fn available(&self) -> usize {
        self.samples.len()
    }
}

/// Linear conversion with callback-owned lookahead. Missing samples don't
/// advance fractional position, so input block boundaries cannot skip audio.
pub struct Resampler {
    ratio: f64,
    position: f64,
    first: Option<f32>,
    second: Option<f32>,
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        assert!(input_rate > 0 && output_rate > 0);
        Self {
            ratio: input_rate as f64 / output_rate as f64,
            position: 0.0,
            first: None,
            second: None,
        }
    }
    pub fn next_sample(&mut self, ring: &SpscRing) -> Option<f32> {
        while self.position >= 1.0 {
            self.first.take().or_else(|| ring.pop())?;
            self.first = self.second.take();
            self.position -= 1.0;
        }
        if self.first.is_none() {
            self.first = ring.pop();
        }
        let first = self.first?;
        let frac = self.position as f32;
        let sample = if frac == 0.0 {
            first
        } else {
            if self.second.is_none() {
                self.second = ring.pop();
            }
            first + (self.second? - first) * frac
        };
        self.position += self.ratio;
        Some(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_queue_preserves_unread_samples_and_requested_capacity() {
        let ring = SpscRing::new(2);
        assert!(ring.push(1.0));
        assert!(ring.push(2.0));
        assert!(!ring.push(3.0));
        assert_eq!(ring.available(), 2);
        assert_eq!(ring.pop(), Some(1.0));
        assert!(ring.push(3.0));
        assert_eq!(ring.pop(), Some(2.0));
        assert_eq!(ring.pop(), Some(3.0));
        ring.reset();
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn resampling_matches_across_input_chunk_boundaries() {
        for (input_rate, output_rate) in [(48000, 44100), (44100, 48000), (48000, 48000)] {
            let input: Vec<f32> = (0..1000).map(|i| i as f32).collect();
            let run = |chunk_size| {
                let ring = SpscRing::new(1024);
                let mut resampler = Resampler::new(input_rate, output_rate);
                let mut output = Vec::new();
                for chunk in input.chunks(chunk_size) {
                    for &sample in chunk {
                        assert!(ring.push(sample));
                    }
                    while let Some(sample) = resampler.next_sample(&ring) {
                        output.push(sample);
                    }
                }
                output
            };
            let whole = run(1000);
            assert_eq!(whole, run(7));
            let ratio = input_rate as f64 / output_rate as f64;
            assert!((whole.len() as f64 - 1000.0 / ratio).abs() <= 2.0);
            for (i, sample) in whole.iter().enumerate() {
                assert!((*sample as f64 - i as f64 * ratio).abs() < 0.001);
            }
        }
    }

    #[test]
    fn concurrent_producer_and_consumer_preserve_order() {
        let ring = std::sync::Arc::new(SpscRing::new(31));
        let producer_ring = ring.clone();
        let producer = std::thread::spawn(move || {
            for i in 0..10000 {
                while !producer_ring.push(i as f32) {
                    std::thread::yield_now();
                }
            }
        });
        let mut resampler = Resampler::new(48000, 48000);
        for i in 0..10000 {
            let sample = loop {
                if let Some(sample) = resampler.next_sample(&ring) {
                    break sample;
                }
                std::thread::yield_now();
            };
            assert_eq!(sample, i as f32);
        }
        producer.join().unwrap();
    }
}
