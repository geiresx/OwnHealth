//! Python's `random.Random` for an integer seed: MT19937 seeded through `init_by_array`, with
//! `random()`, `randint()` and `uniform()` giving the same numbers as CPython. The synthetic exports
//! of the demo seed and the tests draw from it, so both backends seed the same values.

const N: usize = 624;
const M: usize = 397;

pub struct PyRandom {
    mt: [u32; N],
    index: usize,
}

impl PyRandom {
    /// `random.Random(seed)`.
    pub fn new(seed: u64) -> PyRandom {
        let mut key: Vec<u32> = Vec::new();
        let mut n = seed;
        while n > 0 {
            key.push(n as u32);
            n >>= 32;
        }
        if key.is_empty() {
            key.push(0);
        }
        let mut r = PyRandom { mt: [0; N], index: N };
        r.init_by_array(&key);
        r
    }

    fn init_genrand(&mut self, s: u32) {
        self.mt[0] = s;
        for i in 1..N {
            let prev = self.mt[i - 1];
            self.mt[i] = 1_812_433_253u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i as u32);
        }
        self.index = N;
    }

    fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19_650_218);
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..N.max(key.len()) {
            let prev = self.mt[i - 1];
            self.mt[i] = (self.mt[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                self.mt[0] = self.mt[N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..N - 1 {
            let prev = self.mt[i - 1];
            self.mt[i] = (self.mt[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_566_083_941)).wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                self.mt[0] = self.mt[N - 1];
                i = 1;
            }
        }
        self.mt[0] = 0x8000_0000;
    }

    fn next_u32(&mut self) -> u32 {
        if self.index >= N {
            for k in 0..N {
                let y = (self.mt[k] & 0x8000_0000) | (self.mt[(k + 1) % N] & 0x7fff_ffff);
                let mut v = self.mt[(k + M) % N] ^ (y >> 1);
                if y & 1 == 1 {
                    v ^= 0x9908_b0df;
                }
                self.mt[k] = v;
            }
            self.index = 0;
        }
        let mut y = self.mt[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    /// `random()`: a float in [0, 1) with 53 random bits.
    pub fn random(&mut self) -> f64 {
        let a = (self.next_u32() >> 5) as f64;
        let b = (self.next_u32() >> 6) as f64;
        (a * 67_108_864.0 + b) / 9_007_199_254_740_992.0
    }

    /// `getrandbits(k)` for 0 < k <= 64: 32-bit words, least significant first, the last one cut.
    fn getrandbits(&mut self, k: u32) -> u64 {
        if k <= 32 {
            return u64::from(self.next_u32() >> (32 - k));
        }
        let low = u64::from(self.next_u32());
        let high = u64::from(self.next_u32() >> (64 - k));
        low | (high << 32)
    }

    /// `randint(a, b)`: `a + _randbelow(b - a + 1)` by rejection on `getrandbits`.
    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        let n = (b - a + 1) as u64;
        if n <= 1 {
            return a;
        }
        let k = 64 - n.leading_zeros();
        loop {
            let r = self.getrandbits(k);
            if r < n {
                return a + r as i64;
            }
        }
    }

    /// `uniform(a, b)`.
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.random()
    }
}

/// Python's `round(v, 1)`: the decimal nearest to the exact binary value, ties to even.
pub fn round1(v: f64) -> f64 {
    format!("{v:.1}").parse().unwrap_or(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_cpython() {
        // python3 -c "import random; r=random.Random(7); print(r.random(), r.randint(-25, 25), r.uniform(-8, 8))"
        let mut r = PyRandom::new(7);
        assert_eq!(r.random(), 0.32383276483316237);
        assert_eq!(r.randint(-25, 25), -16);
        assert_eq!(r.uniform(-8.0, 8.0), -1.6828240572292241);
        // python3 -c "import random; r=random.Random(20261107); print([r.randint(150, 900) for _ in range(3)])"
        let mut r = PyRandom::new(20_261_107);
        let got: Vec<i64> = (0..3).map(|_| r.randint(150, 900)).collect();
        assert_eq!(got, vec![728, 826, 539]);
        // a seed wider than 32 bits: python3 -c "import random; r=random.Random(2**40+5); print(r.random(), r.randint(0, 10**6))"
        let mut r = PyRandom::new((1u64 << 40) + 5);
        assert_eq!(r.random(), 0.5043802970418443);
        assert_eq!(r.randint(0, 1_000_000), 281_652);
        // python3 -c "import random; r=random.Random(20261107); print(r.random(), r.random())"
        let mut r = PyRandom::new(20_261_107);
        assert_eq!(r.random(), 0.564_603_147_364_177_4);
        assert_eq!(r.random(), 0.380_302_268_575_995_6);
        // [round(r.uniform(-1.5, 1.5), 1) for _ in range(3)] on a fresh Random(20261107)
        let mut r = PyRandom::new(20_261_107);
        let got: Vec<f64> = (0..3).map(|_| round1(r.uniform(-1.5, 1.5))).collect();
        assert_eq!(got, [0.2, -0.4, 0.9]);
    }

    #[test]
    fn round1_is_pythons_round() {
        assert_eq!(round1(0.25), 0.2);
        assert_eq!(round1(57.95), 58.0);
        assert_eq!(round1(-0.05), -0.1);
    }
}
