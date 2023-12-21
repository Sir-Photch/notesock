use anyhow::{anyhow, bail};

use num::{CheckedAdd, NumCast, PrimInt};
use radix_fmt::{radix_36, Radix};
use rand::{
    distributions::uniform::{SampleRange, SampleUniform},
    thread_rng,
};
use std::{collections::HashSet, fmt::Display, hash::Hash};

pub const ID_REGEXP: &str = "[a-z0-9]";

pub trait IdGenerator {
    // option communicates exhaustion of the id range
    fn get(&mut self) -> Option<String>;

    // this assumes that numbers passed to this function
    // are originating from IdGenerator::get().
    // thus, we needn't check the cache.
    fn remove(&mut self, val: &str) -> bool;
}

pub struct RandomIdGenerator<TRange: PrimInt> {
    min: TRange,
    max: TRange,
    max_iter: Option<usize>,
    set: HashSet<TRange>,
}

impl<TRange> RandomIdGenerator<TRange>
where
    TRange: PrimInt + Hash,
{
    pub fn new(
        min: &str,
        max: &str,
        max_iter: Option<usize>,
        present_values: Option<HashSet<String>>,
    ) -> anyhow::Result<RandomIdGenerator<TRange>> {
        let min = b36_to::<TRange>(min)
            .ok_or(anyhow!("min: {} is not b36 or not in range of u128", min))?;
        let max = b36_to::<TRange>(max)
            .ok_or(anyhow!("max: {} is not b36 or not in range of u128", max))?;

        if (min..max).is_empty() {
            bail!("Empty range from min to max")
        }

        let set = if let Some(ids) = present_values {
            ids.iter()
                .filter_map(|v| b36_to::<TRange>(v))
                .collect::<HashSet<_>>()
        } else {
            HashSet::<TRange>::new()
        };

        Ok(RandomIdGenerator {
            min,
            max,
            max_iter,
            set,
        })
    }
}

impl<TRange> IdGenerator for RandomIdGenerator<TRange>
where
    Radix<TRange>: Display,
    TRange: PrimInt + SampleUniform + Hash,
{
    fn get(&mut self) -> Option<String> {
        let mut id = (self.min..=self.max).sample_single(&mut thread_rng());
        let mut index = 0;
        while !self.set.insert(id) {
            id = (self.min..=self.max).sample_single(&mut thread_rng());
            if let Some(limit) = self.max_iter {
                index += 1;
                if index >= limit {
                    return None;
                }
            }
        }
        Some(radix_36(id).to_string())
    }
    fn remove(&mut self, val: &str) -> bool {
        let val = b36_to::<TRange>(val);
        match val {
            None => false,
            Some(id) => self.set.remove(&id),
        }
    }
}

fn b36_to<T: PrimInt + CheckedAdd>(val: &str) -> Option<T> {
    let mut ret: T = T::zero();
    for (i, c) in val.chars().rev().enumerate() {
        let a: T = NumCast::from(32u32.pow(i.try_into().ok()?))?;
        let b: T = NumCast::from(c.to_digit(36)?)?;
        ret = ret.checked_add(&(a * b))?
    }
    Some(ret)
}

#[cfg(test)]
mod test {
    use radix_fmt::radix_36;

    #[cfg(feature = "bench")]
    extern crate test;
    #[cfg(feature = "bench")]
    use test::{black_box, Bencher};

    use super::*;

    #[test]
    fn test_b36_conversion() {
        for i in 0..1000 {
            assert_eq!(i, b36_to::<u32>(&radix_36(i).to_string()).unwrap())
        }
    }

    #[cfg(feature = "bench")]
    #[bench]
    fn bench_simple_generator_full_range(b: &mut Bencher) {
        let mut generator = RandomIdGenerator::<u128>::new(
            &radix_36(u32::MIN).to_string(),
            &radix_36(u32::MAX).to_string(),
            Some(5),
            None,
        )
        .unwrap();
        b.iter(|| black_box(generator.get()));
    }

    #[cfg(feature = "bench")]
    #[bench]
    fn bench_simple_generator_part_range(b: &mut Bencher) {
        let mut generator =
            RandomIdGenerator::<u128>::new("11111", "zzzzz", Some(5), None).unwrap();
        b.iter(|| black_box(generator.get()));
    }
}
