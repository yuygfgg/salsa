// SPDX-License-Identifier: MPL-2.0
//! Small dense linear-algebra helpers (row-major).
//!
//! The clustering objective only needs matrices up to the object count
//! (typically <= 128), so a compact, allocation-light implementation is easier
//! to audit than pulling in a full linear-algebra stack.

pub fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn norm(a: &[f64]) -> f64 {
    dot(a, a).sqrt()
}

pub fn transpose(a: &[f64], rows: usize, cols: usize) -> Vec<f64> {
    let mut out = vec![0.0; rows * cols];
    for r in 0..rows {
        for c in 0..cols {
            out[c * rows + r] = a[r * cols + c];
        }
    }
    out
}

/// `out = a * b`, with `a` `rows x inner` and `b` `inner x cols`.
pub fn mat_mul(a: &[f64], rows: usize, inner: usize, b: &[f64], cols: usize) -> Vec<f64> {
    let mut out = vec![0.0; rows * cols];
    for r in 0..rows {
        let arow = &a[r * inner..(r + 1) * inner];
        let orow = &mut out[r * cols..(r + 1) * cols];
        for (k, &av) in arow.iter().enumerate() {
            if av == 0.0 {
                continue;
            }
            let brow = &b[k * cols..(k + 1) * cols];
            for c in 0..cols {
                orow[c] += av * brow[c];
            }
        }
    }
    out
}

/// Solve `a x = b` for a small square system by Gaussian elimination.
pub fn solve_small(mut a: Vec<f64>, mut b: Vec<f64>, n: usize) -> Option<Vec<f64>> {
    for col in 0..n {
        let mut pivot = col;
        let mut best = a[col * n + col].abs();
        for row in col + 1..n {
            let value = a[row * n + col].abs();
            if value > best {
                best = value;
                pivot = row;
            }
        }
        if best < 1e-12 {
            return None;
        }
        if pivot != col {
            for c in 0..n {
                a.swap(col * n + c, pivot * n + c);
            }
            b.swap(col, pivot);
        }
        let diag = a[col * n + col];
        for row in col + 1..n {
            let factor = a[row * n + col] / diag;
            if factor == 0.0 {
                continue;
            }
            for c in col..n {
                a[row * n + c] -= factor * a[col * n + c];
            }
            b[row] -= factor * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let mut sum = b[row];
        for c in row + 1..n {
            sum -= a[row * n + c] * x[c];
        }
        x[row] = sum / a[row * n + row];
    }
    Some(x)
}

/// Least-squares solve for a small `m x n` system via normal equations.
pub fn lstsq(a: &[f64], m: usize, n: usize, b: &[f64]) -> Option<Vec<f64>> {
    let mut normal = vec![0.0; n * n];
    let mut rhs = vec![0.0; n];
    for i in 0..n {
        for j in 0..n {
            let mut sum = 0.0;
            for row in 0..m {
                sum += a[row * n + i] * a[row * n + j];
            }
            normal[i * n + j] = sum;
        }
        let mut sum = 0.0;
        for row in 0..m {
            sum += a[row * n + i] * b[row];
        }
        rhs[i] = sum;
    }
    solve_small(normal, rhs, n)
}
