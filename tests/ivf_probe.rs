//! Feasibility probe, not product code: how many rows must an IVF index scan
//! to keep Recall@10 >= 0.95, with and without a 10% equality filter, on
//! uniform and on clustered data shaped like the benchmark (10,000 x 128).
//!
//!   cargo test --release --test ivf_probe -- --ignored --nocapture

use minivectordb::vector::{l2_sq, l2_sq_f32};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
    fn gauss(&mut self) -> f32 {
        // Sum of four uniforms: close enough to normal for this purpose.
        (0..4).map(|_| self.unit()).sum::<f32>() * 0.866
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const DIM: usize = 128;

fn kmeans(data: &[f32], n: usize, lists: usize, rng: &mut Rng) -> (Vec<f32>, Vec<usize>) {
    let mut cent: Vec<f32> = (0..lists).flat_map(|_| data[rng.below(n) * DIM..][..DIM].to_vec()).collect();
    let mut assign = vec![0usize; n];
    for _ in 0..12 {
        for (i, a) in assign.iter_mut().enumerate() {
            let row = &data[i * DIM..][..DIM];
            *a = (0..lists)
                .min_by(|&x, &y| l2_sq_f32(row, &cent[x * DIM..][..DIM]).total_cmp(&l2_sq_f32(row, &cent[y * DIM..][..DIM])))
                .unwrap();
        }
        let mut sum = vec![0f64; lists * DIM];
        let mut cnt = vec![0usize; lists];
        for (i, &a) in assign.iter().enumerate() {
            cnt[a] += 1;
            for d in 0..DIM {
                sum[a * DIM + d] += data[i * DIM + d] as f64;
            }
        }
        for l in 0..lists {
            if cnt[l] > 0 {
                for d in 0..DIM {
                    cent[l * DIM + d] = (sum[l * DIM + d] / cnt[l] as f64) as f32;
                }
            }
        }
    }
    (cent, assign)
}

fn top10(cands: impl Iterator<Item = usize>, data: &[f32], q: &[f32]) -> Vec<usize> {
    let mut v: Vec<(f64, usize)> = cands.map(|i| (l2_sq(&data[i * DIM..][..DIM], q), i)).collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    v.into_iter().take(10).map(|(_, i)| i).collect()
}

fn run(name: &str, clustered: bool) {
    let (n, lists, queries) = (10_000, 100, 300);
    let mut rng = Rng(if clustered { 7 } else { 1 });
    let centers: Vec<Vec<f32>> = (0..50).map(|_| (0..DIM).map(|_| rng.unit()).collect()).collect();
    let point = |rng: &mut Rng| -> Vec<f32> {
        if clustered {
            let c = &centers[rng.below(centers.len())];
            c.iter().map(|x| x + rng.gauss() * 0.15).collect()
        } else {
            (0..DIM).map(|_| rng.unit()).collect()
        }
    };
    let data: Vec<f32> = (0..n).flat_map(|_| point(&mut rng)).collect();
    let cat: Vec<usize> = (0..n).map(|_| rng.below(10)).collect();
    let (cent, assign) = kmeans(&data, n, lists, &mut rng);
    let mut members = vec![Vec::new(); lists];
    for (i, &a) in assign.iter().enumerate() {
        members[a].push(i);
    }
    let qs: Vec<(Vec<f32>, Option<usize>)> =
        (0..queries).map(|j| (point(&mut rng), if j % 2 == 1 { Some(rng.below(10)) } else { None })).collect();
    println!("{name}: {n} rows, {lists} lists");
    println!("  nprobe  scanned  recall(no filter)  recall(filter)");
    for nprobe in [1, 2, 4, 8, 16, 24, 32, 48, 64] {
        let (mut rec, mut cnt, mut scanned) = ([0f64; 2], [0usize; 2], 0usize);
        for (q, f) in &qs {
            let keep = |i: &usize| f.is_none_or(|c| cat[*i] == c);
            let truth = top10((0..n).filter(keep), &data, q);
            let mut order: Vec<usize> = (0..lists).collect();
            order.sort_by(|&x, &y| l2_sq_f32(q, &cent[x * DIM..][..DIM]).total_cmp(&l2_sq_f32(q, &cent[y * DIM..][..DIM])));
            let cands: Vec<usize> = order[..nprobe].iter().flat_map(|&l| members[l].iter().copied()).collect();
            scanned += cands.len();
            let got = top10(cands.into_iter().filter(keep), &data, q);
            let g = f.is_some() as usize;
            rec[g] += got.iter().filter(|i| truth.contains(i)).count() as f64 / truth.len().max(1) as f64;
            cnt[g] += 1;
        }
        println!(
            "  {nprobe:>6}  {:>6.1}%  {:>17.4}  {:>14.4}",
            100.0 * scanned as f64 / (queries * n) as f64,
            rec[0] / cnt[0] as f64,
            rec[1] / cnt[1] as f64
        );
    }
}

#[test]
#[ignore]
fn ivf_recall_vs_scan() {
    run("uniform [-1, 1)", false);
    run("50 gaussian clusters, sigma 0.15", true);
}
