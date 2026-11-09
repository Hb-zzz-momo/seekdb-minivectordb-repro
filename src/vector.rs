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
    }
}
