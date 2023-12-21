use anyhow::{anyhow, bail};
use itertools::Itertools;
use radix_fmt::radix_36;
use rand::{distributions::uniform::SampleRange, seq::SliceRandom, thread_rng, Rng};
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    ffi::{OsStr, OsString},
    iter::once,
};

const PASTE_ID_SYMBOLS: [char; 36] = [
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

pub fn sample_unique(ids: &HashSet<OsString>, mut min_length: usize, max_iter: usize) -> String {
    let generate = |len| {
        PASTE_ID_SYMBOLS
            .choose_multiple(&mut thread_rng(), len)
            .collect::<String>()
    };

    let mut lucky = generate(min_length);

    if !ids.is_empty() {
        let mut i = 0;
        while ids.contains(OsStr::new(&lucky)) {
            if i > max_iter {
                // not so lucky
                i = 0;
                min_length *= 2;
            }
            lucky = generate(min_length);
            i += 1;
        }
    }

    lucky
}

struct IdGenerator {
    min: u32,
    max: u32,
    numbers: BTreeSet<u32>,
    cache: VecDeque<u32>,
    max_pregen_size: usize,
}

impl IdGenerator {
    pub fn new(
        min: &str,
        max: &str,
        mut max_pregen_size: usize,
        present_values: Option<HashSet<String>>,
    ) -> anyhow::Result<IdGenerator> {
        if max_pregen_size == 0 {
            max_pregen_size = usize::MAX;
        }

        let min = u32_from_b36str(min)
            .ok_or(anyhow!("min: {} is not b36 or not in range of u32", min))?;
        let max = u32_from_b36str(max)
            .ok_or(anyhow!("max: {} is not b36 or not in range of u32", max))?;

        if min == max {
            bail!("invalid values for min and max IDs")
        }

        let (numbers, cache) = if let Some(ids) = present_values {
            (
                ids.iter()
                    .filter_map(|v| u32_from_b36str(v))
                    .collect::<BTreeSet<_>>(),
                VecDeque::new(),
            )
        } else {
            let first_id = [thread_rng().gen_range(min..max)];
            (BTreeSet::from(first_id), VecDeque::from(first_id))
        };

        Ok(IdGenerator {
            min,
            max,
            numbers,
            cache,
            max_pregen_size,
        })
    }

    pub fn get(&mut self, random: bool) -> Option<String> {
        if self.cache.is_empty() && !self.generate(random) {
            return None;
        }

        Some(format!("{}", radix_36(self.cache.pop_front().unwrap())))
    }

    // this assumes that numbers passed to this function
    // are originating from IdGenerator::get().
    // thus, we needn't check the cache.
    pub fn remove(&mut self, val: &str) -> bool {
        match u32_from_b36str(val) {
            Some(val) => self.numbers.remove(&val),
            None => false,
        }
    }

    fn generate(&mut self, random: bool) -> bool {
        let new_values: Vec<u32> = once(self.min)
            .chain(self.numbers.iter().cloned().chain(once(self.max)))
            .tuple_windows()
            .filter(|(lo, up)| up - lo > 1)
            .sorted_unstable_by(|(a, b), (c, d)| (b - a).cmp(&(d - c))) // stable sort is probably unnecessary.
            .take(self.max_pregen_size)
            .map(|(lo, up)| {
                if up - lo == 2 {
                    lo + 1
                } else if random {
                    (lo + 1..up).sample_single(&mut thread_rng())
                } else {
                    // mitigate overflows
                    ((up as u64 + lo as u64) / 2) as u32
                }
            })
            .collect();

        if new_values.is_empty() {
            return false;
        }

        self.numbers.extend(&new_values);
        self.cache.extend(new_values);

        true
    }
}

fn u32_from_b36str(val: &str) -> Option<u32> {
    let mut ret: u32 = 0;
    for (i, c) in val.chars().rev().enumerate() {
        ret = ret.checked_add(36u32.pow(i.try_into().ok()?) * c.to_digit(36)?)?;
    }

    Some(ret)
}

#[cfg(test)]
mod test {
    use radix_fmt::radix_36;

    extern crate test;
    use test::{black_box, Bencher};

    use super::*;

    #[test]
    fn test_b36_conversion() {
        for i in 0..1000 {
            assert_eq!(i, u32_from_b36str(&radix_36(i).to_string()).unwrap())
        }
    }

    #[test]
    fn test_random_generator() {
        let mut generator = IdGenerator::new(
            &radix_36(u32::MIN).to_string(),
            &radix_36(u32::MAX).to_string(),
            10,
            None,
        )
        .unwrap();
        let ids: Vec<_> = (0..=100000).map(|_| generator.get(true)).collect();
        let mut set = HashSet::<String>::new();
        for id in ids {
            let id = id.unwrap();
            assert!(set.insert(id.clone()), "{}", id)
        }
    }

    #[test]
    fn test_mean_generator() {
        let mut generator = IdGenerator::new(
            &radix_36(u32::MIN).to_string(),
            &radix_36(u32::MAX).to_string(),
            10,
            None,
        )
        .unwrap();
        let ids: Vec<_> = (0..=100000).map(|_| generator.get(false)).collect();
        let mut set = HashSet::<String>::new();
        for id in ids {
            let id = id.unwrap();
            assert!(set.insert(id.clone()), "{}", id)
        }
    }

    #[bench]
    fn bench_random_generator(b: &mut Bencher) {
        let mut generator = IdGenerator::new(
            &radix_36(u32::MIN).to_string(),
            &radix_36(u32::MAX).to_string(),
            20,
            None,
        )
        .unwrap();
        b.iter(|| black_box(generator.get(black_box(true))))
    }

    #[bench]
    fn bench_mean_generator(b: &mut Bencher) {
        let mut generator = IdGenerator::new(
            &radix_36(u32::MIN).to_string(),
            &radix_36(u32::MAX).to_string(),
            20,
            None,
        )
        .unwrap();
        b.iter(|| black_box(generator.get(black_box(false))))
    }

    #[bench]
    fn bench_sample_unique(b: &mut Bencher) {
        let mut set = HashSet::<OsString>::new();
        b.iter(|| {
            black_box(set.insert(OsString::from(sample_unique(
                black_box(&set),
                black_box(5),
                black_box(3),
            ))))
        })
    }
}
