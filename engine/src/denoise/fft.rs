//! A small exact-size FFT for the denoiser's 960-sample window.
//!
//! RNNoise works on 20 ms windows of 960 samples at 48 kHz, i.e. 2^6 * 3 * 5, which is not a
//! power of two. This is a recursive mixed-radix decimation-in-time FFT for factors 2, 3 and 5,
//! with the same conventions as `rustfft`/`realfft` (forward `e^-2πikn/N`, neither direction
//! normalised), because the vendored denoiser code was written against those.

use std::ops::{Add, AddAssign, Mul, MulAssign};
use std::sync::OnceLock;

use super::{FREQ_SIZE, WINDOW_SIZE};

/// A single-precision complex number.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) struct Complex {
    pub re: f32,
    pub im: f32,
}

impl Complex {
    pub(crate) const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
    fn conj(self) -> Self {
        Self::new(self.re, -self.im)
    }
}
impl Add for Complex {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.re + o.re, self.im + o.im)
    }
}
impl AddAssign for Complex {
    fn add_assign(&mut self, o: Self) {
        self.re += o.re;
        self.im += o.im;
    }
}
impl Mul for Complex {
    type Output = Self;
    fn mul(self, o: Self) -> Self {
        Self::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
}
impl Mul<f32> for Complex {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self::new(self.re * k, self.im * k)
    }
}
impl MulAssign<f32> for Complex {
    fn mul_assign(&mut self, k: f32) {
        self.re *= k;
        self.im *= k;
    }
}

/// The non-redundant half of the spectrum of a real 960-sample window: bins 0 (DC) to 480
/// (Nyquist), so `FREQ_SIZE` of them. Indexing and iteration go through `Deref`.
#[derive(Clone)]
pub(crate) struct Spectrum(Box<[Complex; FREQ_SIZE]>);

impl Spectrum {
    pub(crate) fn new() -> Self {
        Self(Box::new([Complex::default(); FREQ_SIZE]))
    }
}
impl std::ops::Deref for Spectrum {
    type Target = [Complex];
    fn deref(&self) -> &[Complex] {
        &self.0[..]
    }
}
impl std::ops::DerefMut for Spectrum {
    fn deref_mut(&mut self) -> &mut [Complex] {
        &mut self.0[..]
    }
}
impl MulAssign<f32> for Spectrum {
    fn mul_assign(&mut self, k: f32) {
        for bin in self.0.iter_mut() {
            *bin *= k;
        }
    }
}
impl MulAssign<&[f32]> for Spectrum {
    /// Scales bin `i` by `gain[i]`.
    fn mul_assign(&mut self, gain: &[f32]) {
        assert_eq!(gain.len(), FREQ_SIZE);
        for (bin, &g) in self.0.iter_mut().zip(gain) {
            *bin *= g;
        }
    }
}

struct Plan {
    /// `e^(-2πik/N)` for `k` in `0..N`.
    twiddle: Vec<Complex>,
}

fn plan() -> &'static Plan {
    static PLAN: OnceLock<Plan> = OnceLock::new();
    PLAN.get_or_init(|| {
        let twiddle = (0..WINDOW_SIZE)
            .map(|k| {
                let angle = -2.0 * std::f64::consts::PI * k as f64 / WINDOW_SIZE as f64;
                Complex::new(angle.cos() as f32, angle.sin() as f32)
            })
            .collect();
        Plan { twiddle }
    })
}

fn smallest_factor(n: usize) -> usize {
    match () {
        _ if n % 2 == 0 => 2,
        _ if n % 3 == 0 => 3,
        _ if n % 5 == 0 => 5,
        _ => unreachable!("the window size only has the factors 2, 3 and 5"),
    }
}

/// `W_N^j`, conjugated for the inverse transform.
fn root(plan: &Plan, j: usize, inverse: bool) -> Complex {
    let w = plan.twiddle[j % WINDOW_SIZE];
    if inverse { w.conj() } else { w }
}

fn transform(
    plan: &Plan,
    input: &[Complex],
    input_stride: usize,
    out: &mut [Complex],
    n: usize,
    inverse: bool,
) {
    if n == 1 {
        out[0] = input[0];
        return;
    }
    let p = smallest_factor(n);
    let m = n / p;
    for r in 0..p {
        transform(plan, &input[r * input_stride..], input_stride * p, &mut out[r * m..(r + 1) * m], m, inverse);
    }
    let step = WINDOW_SIZE / n;
    let p_step = WINDOW_SIZE / p;
    let mut tmp = [Complex::default(); 5];
    for k in 0..m {
        for r in 0..p {
            tmp[r] = out[r * m + k] * root(plan, r * k * step, inverse);
        }
        for q in 0..p {
            let mut acc = Complex::default();
            for r in 0..p {
                acc += tmp[r] * root(plan, ((r * q) % p) * p_step, inverse);
            }
            out[k + q * m] = acc;
        }
    }
}

/// Forward real FFT of `input` (960 samples) into the half spectrum, not normalised.
pub(crate) fn real_fft(input: &[f32; WINDOW_SIZE], spectrum: &mut Spectrum) {
    let plan = plan();
    let mut complex_in = [Complex::default(); WINDOW_SIZE];
    for (c, &x) in complex_in.iter_mut().zip(input) {
        c.re = x;
    }
    let mut full = [Complex::default(); WINDOW_SIZE];
    transform(plan, &complex_in, 1, &mut full, WINDOW_SIZE, false);
    spectrum.copy_from_slice(&full[..FREQ_SIZE]);
}

/// Inverse real FFT of the half spectrum into 960 samples, not normalised (a forward transform
/// followed by this one scales the signal by 960). The imaginary parts of the DC and Nyquist
/// bins are ignored, as they are for any real signal.
pub(crate) fn real_ifft(spectrum: &Spectrum, output: &mut [f32; WINDOW_SIZE]) {
    let plan = plan();
    let mut full = [Complex::default(); WINDOW_SIZE];
    full[0] = Complex::new(spectrum[0].re, 0.0);
    full[FREQ_SIZE - 1] = Complex::new(spectrum[FREQ_SIZE - 1].re, 0.0);
    for k in 1..FREQ_SIZE - 1 {
        full[k] = spectrum[k];
        full[WINDOW_SIZE - k] = spectrum[k].conj();
    }
    let mut time = [Complex::default(); WINDOW_SIZE];
    transform(plan, &full, 1, &mut time, WINDOW_SIZE, true);
    for (o, t) in output.iter_mut().zip(&time) {
        *o = t.re;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(n: usize) -> Vec<f32> {
        let mut state = 0x1234_5678u32;
        (0..n)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    #[test]
    fn forward_transform_matches_a_naive_dft() {
        let input: [f32; WINDOW_SIZE] = pseudo_random(WINDOW_SIZE).try_into().unwrap();
        let mut spectrum = Spectrum::new();
        real_fft(&input, &mut spectrum);
        for k in [0usize, 1, 2, 17, 100, 239, 240, 479, 480] {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (n, &x) in input.iter().enumerate() {
                let angle = -2.0 * std::f64::consts::PI * (k * n % WINDOW_SIZE) as f64 / WINDOW_SIZE as f64;
                re += f64::from(x) * angle.cos();
                im += f64::from(x) * angle.sin();
            }
            assert!((f64::from(spectrum[k].re) - re).abs() < 1e-3, "bin {k} re");
            assert!((f64::from(spectrum[k].im) - im).abs() < 1e-3, "bin {k} im");
        }
    }

    #[test]
    fn inverse_undoes_forward_up_to_the_window_size() {
        let input: [f32; WINDOW_SIZE] = pseudo_random(WINDOW_SIZE).try_into().unwrap();
        let mut spectrum = Spectrum::new();
        real_fft(&input, &mut spectrum);
        let mut back = [0.0f32; WINDOW_SIZE];
        real_ifft(&spectrum, &mut back);
        for (a, b) in input.iter().zip(&back) {
            assert!((a * WINDOW_SIZE as f32 - b).abs() < 1e-2);
        }
    }
}
