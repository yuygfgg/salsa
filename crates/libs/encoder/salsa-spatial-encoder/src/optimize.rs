//! Limited-memory BFGS with an Armijo line search.

use anyhow::Result;

struct Curvature {
    step: Vec<f64>,
    gradient_change: Vec<f64>,
    inverse_dot: f64,
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(a, b)| a * b).sum()
}

/// Return a small sequence of improving points for subsequent quantized RD
/// evaluation. The step bound uses the units of the supplied parameters.
pub fn minimize(
    initial: Vec<f64>,
    iterations: usize,
    max_step: f64,
    mut evaluate: impl FnMut(&[f64]) -> Result<(f64, Vec<f64>)>,
) -> Result<Vec<Vec<f64>>> {
    let mut point = initial;
    let (mut value, mut gradient) = evaluate(&point)?;
    let mut history: Vec<Curvature> = Vec::new();
    let mut candidates = Vec::new();

    for iteration in 0..iterations {
        let mut direction = gradient.clone();
        let mut alphas = Vec::with_capacity(history.len());
        for item in history.iter().rev() {
            let alpha = item.inverse_dot * dot(&item.step, &direction);
            for (q, y) in direction.iter_mut().zip(&item.gradient_change) {
                *q -= alpha * y;
            }
            alphas.push(alpha);
        }
        let scale = history.last().map_or(1.0, |item| {
            1.0 / (item.inverse_dot * dot(&item.gradient_change, &item.gradient_change))
        });
        for q in &mut direction {
            *q *= scale;
        }
        for (item, alpha) in history.iter().zip(alphas.iter().rev()) {
            let beta = item.inverse_dot * dot(&item.gradient_change, &direction);
            for (q, s) in direction.iter_mut().zip(&item.step) {
                *q += s * (alpha - beta);
            }
        }
        for q in &mut direction {
            *q = -*q;
        }
        if dot(&gradient, &direction) >= 0.0 {
            history.clear();
            direction = gradient.iter().map(|x| -x).collect();
        }
        let largest = direction.iter().map(|x| x.abs()).fold(0.0, f64::max);
        if !largest.is_finite() || largest < 1e-14 {
            break;
        }
        // Scale the first steepest-descent step to useful parameter units.
        if history.is_empty() || largest > max_step {
            for q in &mut direction {
                *q *= max_step / largest;
            }
        }
        let slope = dot(&gradient, &direction);
        let mut amount = 1.0;
        let mut accepted = None;
        for _ in 0..12 {
            let trial: Vec<_> = point
                .iter()
                .zip(&direction)
                .map(|(x, d)| x + amount * d)
                .collect();
            let (trial_value, trial_gradient) = evaluate(&trial)?;
            if trial_value.is_finite() && trial_value <= value + 1e-4 * amount * slope {
                accepted = Some((trial, trial_value, trial_gradient));
                break;
            }
            amount *= 0.5;
        }
        let Some((next, next_value, next_gradient)) = accepted else {
            break;
        };
        let step: Vec<_> = next.iter().zip(&point).map(|(a, b)| a - b).collect();
        let change: Vec<_> = next_gradient
            .iter()
            .zip(&gradient)
            .map(|(a, b)| a - b)
            .collect();
        let curvature = dot(&step, &change);
        if curvature > 1e-12 * (dot(&step, &step) * dot(&change, &change)).sqrt() {
            if history.len() == 6 {
                history.remove(0);
            }
            history.push(Curvature {
                step,
                gradient_change: change,
                inverse_dot: 1.0 / curvature,
            });
        }
        let improvement = value - next_value;
        point = next;
        value = next_value;
        gradient = next_gradient;
        if (iteration + 1) % 4 == 0 {
            candidates.push(point.clone());
        }
        if improvement < 1e-9 {
            break;
        }
    }
    if candidates.last() != Some(&point) {
        candidates.push(point);
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_on_a_coupled_quadratic() {
        let points = minimize(vec![8.0, -4.0], 24, 4.0, |x| {
            let a = x[0] + 2.0 * x[1] - 3.0;
            let b = 3.0 * x[0] - x[1] - 2.0;
            Ok((a * a + b * b, vec![2.0 * a + 6.0 * b, 4.0 * a - 2.0 * b]))
        })
        .unwrap();
        let result = points.last().unwrap();
        assert!((result[0] - 1.0).abs() < 1e-5);
        assert!((result[1] - 1.0).abs() < 1e-5);
    }
}
