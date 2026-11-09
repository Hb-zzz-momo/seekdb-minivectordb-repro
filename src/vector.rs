//! Distance kernels.
//!
//! `l2_sq` is the reference: query results and their order always come from
//! it. `l2_sq_f32` is a fast float32 version (AVX2/FMA when the CPU has it,
//! checked at run time) whose error against the reference is bounded by
//! `F32Bound`, so a TopK scan can use it to skip rows that cannot place
//! without changing any result.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// Squared Euclidean distance between two equal-length float32 vectors.
/// Differences and sums are taken in f64 so the result orders candidates the
/// same way exact arithmetic would, which keeps TopK tie handling stable.
#[inline]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = [0f64; 4];
    let (ca, ra) = a.as_chunks::<4>();
    let (cb, rb) = b.as_chunks::<4>();
    for (x, y) in ca.iter().zip(cb) {
        for j in 0..4 {
            let d = x[j] as f64 - y[j] as f64;
            acc[j] += d * d;
        }
    }
    let mut sum = (acc[0] + acc[1]) + (acc[2] + acc[3]);
    for (x, y) in ra.iter().zip(rb) {
        let d = *x as f64 - *y as f64;
        sum += d * d;
    }
    sum
}

/// `L2_DISTANCE`: Euclidean distance.
#[inline]
pub fn l2_distance(a: &[f32], b: &[f32]) -> f64 {
    l2_sq(a, b).sqrt()
}

/// Squared Euclidean distance computed in float32. Never NaN; `+inf` when
/// the sum overflows float32.
#[inline]
pub fn l2_sq_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma") {
        // SAFETY: the CPU supports AVX2 and FMA, checked just above.
        return unsafe { l2_sq_f32_avx2(a, b) };
    }
    l2_sq_f32_scalar(a, b)
}

fn l2_sq_f32_scalar(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0f32;
    for (x, y) in a.iter().zip(b) {
        let d = x - y;
        sum += d * d;
    }
    sum
}

/// Four accumulators of eight lanes, so each lane sums `len / 32` terms.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn l2_sq_f32_avx2(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (pa, pb) = (a.as_ptr(), b.as_ptr());
    let mut acc = [_mm256_setzero_ps(); 4];
    let mut i = 0;
    while i + 32 <= n {
        for (j, s) in acc.iter_mut().enumerate() {
            let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i + 8 * j)), _mm256_loadu_ps(pb.add(i + 8 * j)));
            *s = _mm256_fmadd_ps(d, d, *s);
        }
        i += 32;
    }
    while i + 8 <= n {
        let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
        acc[0] = _mm256_fmadd_ps(d, d, acc[0]);
        i += 8;
    }
    let s = _mm256_add_ps(_mm256_add_ps(acc[0], acc[1]), _mm256_add_ps(acc[2], acc[3]));
    let s = _mm_add_ps(_mm256_castps256_ps128(s), _mm256_extractf128_ps::<1>(s));
    let s = _mm_add_ps(s, _mm_movehl_ps(s, s));
    let s = _mm_add_ss(s, _mm_shuffle_ps::<0b01>(s, s));
    let mut sum = _mm_cvtss_f32(s);
    while i < n {
        let d = *pa.add(i) - *pb.add(i);
        sum = d.mul_add(d, sum);
        i += 1;
    }
    sum
}

/// Error bound of `l2_sq_f32` for one vector length: whenever its result is
/// finite, `l2_sq_f32(a, b) <= (1 + rel) * exact + abs`, where `exact` is the
/// squared distance in real arithmetic.
///
/// Every term is non-negative, so the relative error of the sum is at most
/// `m * u` (u = 2^-24) for `m` roundings on a term's path: two from the
/// subtraction, then one per accumulation step. The scalar kernel has
/// `m <= 2 * len + 2`; the AVX2 one has `len / 32 + 3` chain steps, five
/// reduction levels and at most seven tail steps. `rel` is
/// `(len + 64) * 4u`, which covers both with room to spare, and also absorbs
/// the < 1e-13 relative error of the f64 reference and of squaring its
/// square root. `abs` covers rounding of results in the subnormal range, at
/// most 2^-150 per rounding.
#[derive(Clone, Copy)]
pub struct F32Bound {
    rel: f64,
    abs: f64,
}

impl F32Bound {
    pub fn new(len: usize) -> F32Bound {
        let len = len as f64;
        F32Bound { rel: (len + 64.0) * 2f64.powi(-22), abs: (len + 8.0) * 2f64.powi(-146) }
    }

    /// Smallest f32 distance a row whose `l2_distance` is at most `dist`
    /// can still get, rounded up generously: rows above it cannot reach `dist`.
    pub fn cutoff(&self, dist: f64) -> f64 {
        dist * dist * (1.0 + self.rel) + self.abs
    }

    /// False only when `approx` proves the row's distance exceeds the one
    /// `cutoff` was computed from.
    #[inline]
    pub fn may_reach(approx: f32, cutoff: f64) -> bool {
        approx as f64 <= cutoff || !approx.is_finite()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(l2_distance(&[0.0, 0.0], &[3.0, 4.0]), 5.0);
        let a: Vec<f32> = (0..131).map(|i| i as f32).collect();
        let b: Vec<f32> = (0..131).map(|i| i as f32 + 1.0).collect();
        assert_eq!(l2_sq(&a, &b), 131.0);
        assert_eq!(l2_distance(&a, &a), 0.0);
        assert_eq!(l2_sq_f32(&a, &b), 131.0);
        assert_eq!(l2_sq_f32(&[], &[]), 0.0);
    }

    /// xorshift64*, enough for test data.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn f32_in(&mut self, scale: f32) -> f32 {
            ((self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * scale
        }
    }

    /// Squared distance accurate to about 1e-16 relative (exact f64
    /// differences, compensated sum), far below the f32 error being tested.
    fn exact_sq(a: &[f32], b: &[f32]) -> f64 {
        let terms: Vec<f64> = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).collect();
        let mut sum = 0f64;
        let mut comp = 0f64;
        for t in terms {
            let y = t - comp;
            let s = sum + y;
            comp = (s - sum) - y;
            sum = s;
        }
        sum
    }

    #[test]
    fn f32_kernels_stay_within_bound() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for &scale in &[1.0f32, 1e-3, 1e3, 1e-20, 1e15, 3e-39] {
            for len in [1usize, 7, 8, 9, 31, 32, 33, 100, 128, 129, 300, 1024] {
                let bound = F32Bound::new(len);
                for _ in 0..20 {
                    let a: Vec<f32> = (0..len).map(|_| rng.f32_in(scale)).collect();
                    let b: Vec<f32> = a.iter().map(|x| x + rng.f32_in(scale) * 1e-3).collect();
                    let exact = exact_sq(&a, &b);
                    let limit = exact * (1.0 + bound.rel) + bound.abs;
                    for (name, got) in [("dispatch", l2_sq_f32(&a, &b)), ("scalar", l2_sq_f32_scalar(&a, &b))] {
                        assert!(got.is_finite() && (got as f64) <= limit, "{name} len {len} scale {scale}: {got} > {limit}");
                    }
                    assert!(((l2_sq(&a, &b) - exact) / exact.max(f64::MIN_POSITIVE)).abs() < 1e-13);
                }
            }
        }
    }

    #[test]
    fn f32_kernel_overflow_is_infinite_not_nan() {
        let a = vec![3e38f32; 40];
        let b = vec![-3e38f32; 40];
        assert_eq!(l2_sq_f32(&a, &b), f32::INFINITY);
        assert_eq!(l2_sq_f32_scalar(&a, &b), f32::INFINITY);
        assert!(F32Bound::may_reach(f32::INFINITY, 1.0));
        assert!(l2_sq(&a, &b).is_finite());
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    /// cargo test --release -- --ignored --nocapture kernel_speed
    #[test]
    #[ignore]
    fn kernel_speed() {
        let (dim, n) = (128, 10_000);
        let mut x = 1u64;
        let mut next = move || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        let data: Vec<f32> = (0..n * dim).map(|_| next()).collect();
        let q: Vec<f32> = (0..dim).map(|_| next()).collect();
        type Kernel<'a> = &'a dyn Fn(&[f32], &[f32]) -> f64;
        let kernels: [(&str, Kernel); 3] = [
            ("f64 reference", &|a, b| l2_sq(a, b)),
            ("f32 dispatch", &|a, b| l2_sq_f32(a, b) as f64),
            ("f32 scalar", &|a, b| l2_sq_f32_scalar(a, b) as f64),
        ];
        for (name, k) in kernels {
            let t = Instant::now();
            let mut s = 0.0;
            for _ in 0..50 {
                for row in data.chunks_exact(dim) {
                    s += k(black_box(row), black_box(&q));
                }
            }
            let ns = t.elapsed().as_nanos() as f64 / (50 * n) as f64;
            println!("{name:<14} {ns:6.1} ns/row  ({s:.1})");
        }
    }
}
