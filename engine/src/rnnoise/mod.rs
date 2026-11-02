//! Pure-Rust port of RNNoise (xiph/rnnoise at commit
//! 372f7b4b76cde4ca1ec4605353dd17898a99de38 — the exact revision Simple Voice
//! Chat's denoiser library builds, model data 0b50c45), so the engine keeps
//! its no-native-libraries guarantee while matching the official client's
//! noise suppression.
//!
//! Ported file by file: `denoise.c` (feature extraction, gains, synthesis),
//! `rnn.c`/`nnet.c` (network layers), `pitch.c`, `celt_lpc.c`, and the float
//! compute path of `nnet_arch.h`/`vec.h`, including the `tanh_approx`
//! rational approximation the official build uses. The model weights live in
//! `weights.bin` (see `scripts/extract_rnnoise_weights.py`).

mod fft;
mod pitch;
mod rnn;
#[rustfmt::skip]
mod tables;
mod weights;

use crate::kiss_fft::KissFftState;

pub const FRAME_SIZE: usize = 480;
const WINDOW_SIZE: usize = 2 * FRAME_SIZE;
const FREQ_SIZE: usize = FRAME_SIZE + 1;
const NB_BANDS: usize = 32;
const NB_FEATURES: usize = 2 * NB_BANDS + 1;
const PITCH_MIN_PERIOD: usize = 60;
const PITCH_MAX_PERIOD: usize = 768;
const PITCH_FRAME_SIZE: usize = 960;
const PITCH_BUF_SIZE: usize = PITCH_MAX_PERIOD + PITCH_FRAME_SIZE;

/// ERB band edges over the 480-bin spectrum (`eband20ms` in denoise.c).
const EBAND: [usize; NB_BANDS + 2] = [
    0, 2, 4, 6, 8, 10, 12, 15, 18, 21, 24, 28, 32, 36, 41, 47, 53, 60, 68, 77,
    87, 98, 110, 124, 140, 157, 176, 198, 223, 251, 282, 317, 356, 400,
];

/// One RNNoise denoiser state. Feed exactly [`Denoiser::frame_size`]
/// samples (10 ms at 48 kHz) per `process` call.
pub struct Denoiser {
    fft: KissFftState,
    model: &'static weights::Model,
    analysis_mem: Vec<f32>,
    synthesis_mem: Vec<f32>,
    pitch_buf: Vec<f32>,
    last_gain: f32,
    last_period: usize,
    mem_hp: [f32; 2],
    lastg: Vec<f32>,
    rnn: rnn::RnnState,
    delayed_x: [(f32, f32); FREQ_SIZE],
    delayed_p: [(f32, f32); FREQ_SIZE],
    delayed_ex: [f32; NB_BANDS],
    delayed_ep: [f32; NB_BANDS],
    delayed_exp: [f32; NB_BANDS],
}

impl Denoiser {
    /// RNNoise processes 480-sample (10 ms) mono frames at 48 kHz.
    pub const fn frame_size() -> usize {
        FRAME_SIZE
    }

    /// Builds a denoiser with the embedded model, or `None` when the
    /// weight blob is corrupt (never expected: it is checked by tests).
    pub fn new() -> Option<Self> {
        let model = weights::model()?;
        let fft = KissFftState::new(WINDOW_SIZE)?;
        Some(Self {
            fft,
            model,
            analysis_mem: vec![0.0; FRAME_SIZE],
            synthesis_mem: vec![0.0; FRAME_SIZE],
            pitch_buf: vec![0.0; PITCH_BUF_SIZE],
            last_gain: 0.0,
            last_period: 0,
            mem_hp: [0.0; 2],
            lastg: vec![0.0; NB_BANDS],
            rnn: rnn::RnnState::new(),
            delayed_x: [(0.0, 0.0); FREQ_SIZE],
            delayed_p: [(0.0, 0.0); FREQ_SIZE],
            delayed_ex: [0.0; NB_BANDS],
            delayed_ep: [0.0; NB_BANDS],
            delayed_exp: [0.0; NB_BANDS],
        })
    }

    /// `rnnoise_process_frame`: denoises 480 samples in place, returns the
    /// network's voice probability (0..1).
    pub fn process(&mut self, in_out: &mut [f32; FRAME_SIZE]) -> f32 {
        let mut x = [0.0f32; FRAME_SIZE];
        rnn::biquad(&mut x, &mut self.mem_hp, in_out, [-2.0, 1.0], [-1.99599, 0.99600]);
        let mut x_spectrum = [(0.0f32, 0.0f32); FREQ_SIZE];
        let mut p_spectrum = [(0.0f32, 0.0f32); FREQ_SIZE];
        let mut ex = [0.0f32; NB_BANDS];
        let mut ep = [0.0f32; NB_BANDS];
        let mut exp_corr = [0.0f32; NB_BANDS];
        let mut features = [0.0f32; NB_FEATURES];
        let silence = self.compute_frame_features(
            &mut x_spectrum, &mut p_spectrum, &mut ex, &mut ep, &mut exp_corr,
            &mut features, &x,
        );
        let mut g = [0.0f32; NB_BANDS];
        let mut vad_prob = 0.0f32;
        if !silence {
            rnn::compute_rnn(self.model, &mut self.rnn, &mut g, &mut vad_prob, &features);
            pitch_filter(
                &mut self.delayed_x, &self.delayed_p, &self.delayed_ex,
                &self.delayed_ep, &self.delayed_exp, &g,
            );
            for (band, gain) in g.iter_mut().enumerate() {
                let alpha = 0.6f32;
                // Cap the decay at 0.6 per frame (RT60 135 ms), like upstream.
                *gain = gain.max(alpha * self.lastg[band]);
                // Compensate for energy change across frames so rising
                // background noise does not leak.
                self.lastg[band] = (*gain * (self.delayed_ex[band] + 1e-3)
                    / (ex[band] + 1e-3))
                    .min(1.0);
            }
            let mut gf = [0.0f32; FREQ_SIZE];
            interp_band_gain(&mut gf, &g);
            for i in 0..FREQ_SIZE {
                self.delayed_x[i].0 *= gf[i];
                self.delayed_x[i].1 *= gf[i];
            }
        }
        let mut out = [0.0f32; FRAME_SIZE];
        Self::frame_synthesis_of(&self.fft, &mut self.synthesis_mem, &mut out, &self.delayed_x);
        *in_out = out;

        self.delayed_x = x_spectrum;
        self.delayed_p = p_spectrum;
        self.delayed_ex = ex;
        self.delayed_ep = ep;
        self.delayed_exp = exp_corr;
        vad_prob
    }

    /// `frame_analysis`: windowed 960-point transform of the last two frames.
    fn frame_analysis(&mut self, out: &mut [(f32, f32); FREQ_SIZE], ex: &mut [f32; NB_BANDS], input: &[f32; FRAME_SIZE]) {
        let mut x = [0.0f32; WINDOW_SIZE];
        x[..FRAME_SIZE].copy_from_slice(&self.analysis_mem);
        x[FRAME_SIZE..].copy_from_slice(input);
        self.analysis_mem.copy_from_slice(input);
        apply_window(&mut x);
        fft::forward(&self.fft, &x, out);
        *ex = band_energy(out);
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_frame_features(
        &mut self,
        x_spectrum: &mut [(f32, f32); FREQ_SIZE],
        p_spectrum: &mut [(f32, f32); FREQ_SIZE],
        ex: &mut [f32; NB_BANDS],
        ep: &mut [f32; NB_BANDS],
        exp_corr: &mut [f32; NB_BANDS],
        features: &mut [f32; NB_FEATURES],
        input: &[f32; FRAME_SIZE],
    ) -> bool {
        let mut e_total = 0.0f32;
        let mut ly = [0.0f32; NB_BANDS];
        self.frame_analysis(x_spectrum, ex, input);
        // Slide the pitch buffer and append the new frame.
        self.pitch_buf.copy_within(FRAME_SIZE.., 0);
        let end = PITCH_BUF_SIZE - FRAME_SIZE;
        self.pitch_buf[end..].copy_from_slice(input);
        // Downsample by 2 with the pre-emphasis FIR, then search the
        // decimated lag (upstream rnn_pitch_downsample + rnn_pitch_search).
        let mut down = [0.0f32; PITCH_BUF_SIZE / 2];
        pitch::downsample(&self.pitch_buf, &mut down);
        let mut pitch_index = 0usize;
        pitch::search(
            &down[PITCH_MAX_PERIOD / 2..],
            &down,
            PITCH_FRAME_SIZE,
            PITCH_MAX_PERIOD - 3 * PITCH_MIN_PERIOD,
            &mut pitch_index,
        );
        pitch_index = PITCH_MAX_PERIOD - pitch_index;
        let gain = pitch::remove_doubling(
            &down,
            PITCH_MAX_PERIOD,
            PITCH_MIN_PERIOD,
            PITCH_FRAME_SIZE,
            &mut pitch_index,
            self.last_period,
            self.last_gain,
        );
        self.last_period = pitch_index;
        self.last_gain = gain;
        let mut p = [0.0f32; WINDOW_SIZE];
        let start = PITCH_BUF_SIZE - WINDOW_SIZE - pitch_index;
        for (i, sample) in p.iter_mut().enumerate() {
            *sample = self.pitch_buf[start + i];
        }
        apply_window(&mut p);
        fft::forward(&self.fft, &p, p_spectrum);
        *ep = band_energy(p_spectrum);
        *exp_corr = band_corr(x_spectrum, p_spectrum);
        for i in 0..NB_BANDS {
            exp_corr[i] /= (0.001 + ex[i] * ep[i]).sqrt();
        }
        let mut dct_out = [0.0f32; NB_BANDS];
        dct(&mut dct_out, exp_corr);
        features[NB_BANDS..2 * NB_BANDS].copy_from_slice(&dct_out);
        features[2 * NB_BANDS] = 0.01 * (pitch_index as f32 - 300.0);
        let mut log_max = -2.0f32;
        let mut follow = -2.0f32;
        for i in 0..NB_BANDS {
            ly[i] = (1e-2 + ex[i]).log10();
            ly[i] = (log_max - 7.0).max((follow - 1.5).max(ly[i]));
            log_max = log_max.max(ly[i]);
            follow = (follow - 1.5).max(ly[i]);
            e_total += ex[i];
        }
        if e_total < 0.04 {
            // No audio: keep the network state instead of learning silence.
            features.fill(0.0);
            return true;
        }
        dct(&mut dct_out, &ly);
        features[..NB_BANDS].copy_from_slice(&dct_out);
        features[0] -= 12.0;
        features[1] -= 4.0;
        false
    }

    fn frame_synthesis(&mut self, out: &mut [f32; FRAME_SIZE], y: &[(f32, f32); FREQ_SIZE]) {
        let mut x = [0.0f32; WINDOW_SIZE];
        fft::inverse(&self.fft, y, &mut x);
        apply_window(&mut x);
        for i in 0..FRAME_SIZE {
            out[i] = x[i] + self.synthesis_mem[i];
        }
        self.synthesis_mem.copy_from_slice(&x[FRAME_SIZE..]);
    }

    fn frame_synthesis_of(
        fft: &KissFftState,
        synthesis_mem: &mut [f32],
        out: &mut [f32; FRAME_SIZE],
        y: &[(f32, f32); FREQ_SIZE],
    ) {
        let mut x = [0.0f32; WINDOW_SIZE];
        fft::inverse(fft, y, &mut x);
        apply_window(&mut x);
        for i in 0..FRAME_SIZE {
            out[i] = x[i] + synthesis_mem[i];
        }
        synthesis_mem[..FRAME_SIZE].copy_from_slice(&x[FRAME_SIZE..]);
    }
}

fn apply_window(x: &mut [f32]) {
    for i in 0..FRAME_SIZE {
        x[i] *= tables::HALF_WINDOW[i];
        x[WINDOW_SIZE - 1 - i] *= tables::HALF_WINDOW[i];
    }
}

/// `compute_band_energy`: per-band power with triangular band sharing.
pub(crate) fn band_energy(x: &[(f32, f32); FREQ_SIZE]) -> [f32; NB_BANDS] {
    let mut sum = [0.0f32; NB_BANDS + 2];
    for i in 0..NB_BANDS + 1 {
        let band_size = EBAND[i + 1] - EBAND[i];
        for j in 0..band_size {
            let bin = EBAND[i] + j;
            let frac = j as f32 / band_size as f32;
            let tmp = x[bin].0 * x[bin].0 + x[bin].1 * x[bin].1;
            sum[i] += (1.0 - frac) * tmp;
            sum[i + 1] += frac * tmp;
        }
    }
    sum[1] = (sum[0] + sum[1]) * 2.0 / 3.0;
    sum[NB_BANDS] = (sum[NB_BANDS] + sum[NB_BANDS + 1]) * 2.0 / 3.0;
    let mut out = [0.0f32; NB_BANDS];
    out.copy_from_slice(&sum[1..=NB_BANDS]);
    out
}

/// `compute_band_corr`: per-band X·P correlation.
fn band_corr(x: &[(f32, f32); FREQ_SIZE], p: &[(f32, f32); FREQ_SIZE]) -> [f32; NB_BANDS] {
    let mut sum = [0.0f32; NB_BANDS + 2];
    for i in 0..NB_BANDS + 1 {
        let band_size = EBAND[i + 1] - EBAND[i];
        for j in 0..band_size {
            let bin = EBAND[i] + j;
            let frac = j as f32 / band_size as f32;
            let tmp = x[bin].0 * p[bin].0 + x[bin].1 * p[bin].1;
            sum[i] += (1.0 - frac) * tmp;
            sum[i + 1] += frac * tmp;
        }
    }
    sum[1] = (sum[0] + sum[1]) * 2.0 / 3.0;
    sum[NB_BANDS] = (sum[NB_BANDS] + sum[NB_BANDS + 1]) * 2.0 / 3.0;
    let mut out = [0.0f32; NB_BANDS];
    out.copy_from_slice(&sum[1..=NB_BANDS]);
    out
}

/// `interp_band_gain`: spreads the per-band gains over the spectrum.
pub(crate) fn interp_band_gain(g: &mut [f32; FREQ_SIZE], band: &[f32; NB_BANDS]) {
    g.iter_mut().for_each(|value| *value = 0.0);
    for i in 1..NB_BANDS {
        let band_size = EBAND[i + 1] - EBAND[i];
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            g[EBAND[i] + j] = (1.0 - frac) * band[i - 1] + frac * band[i];
        }
    }
    for value in g.iter_mut().take(EBAND[1]) {
        *value = band[0];
    }
    for value in g.iter_mut().take(EBAND[NB_BANDS + 1]).skip(EBAND[NB_BANDS]) {
        *value = band[NB_BANDS - 1];
    }
}

fn dct(out: &mut [f32; NB_BANDS], input: &[f32; NB_BANDS]) {
    for (i, value) in out.iter_mut().enumerate() {
        let mut sum = 0.0f32;
        for (j, input) in input.iter().enumerate() {
            sum += input * tables::DCT_TABLE[j * NB_BANDS + i];
        }
        *value = sum * (2.0f32 / 22.0).sqrt();
    }
}

/// `rnn_pitch_filter`: pitch-comb enhancement of the delayed spectrum before
/// the gains are applied.
fn pitch_filter(
    x: &mut [(f32, f32); FREQ_SIZE],
    p: &[(f32, f32); FREQ_SIZE],
    ex: &[f32; NB_BANDS],
    ep: &[f32; NB_BANDS],
    exp_corr: &[f32; NB_BANDS],
    g: &[f32; NB_BANDS],
) {
    let mut r = [0.0f32; NB_BANDS];
    let mut rf = [0.0f32; FREQ_SIZE];
    let mut norm = [0.0f32; NB_BANDS];
    let mut normf = [0.0f32; FREQ_SIZE];
    for i in 0..NB_BANDS {
        if exp_corr[i] > g[i] {
            r[i] = 1.0;
        } else {
            let exp2 = exp_corr[i] * exp_corr[i];
            let g2 = g[i] * g[i];
            r[i] = exp2 * (1.0 - g2) / (0.001 + g2 * (1.0 - exp2));
            r[i] = r[i].min(1.0).max(0.0).sqrt();
        }
        r[i] *= (ex[i] / (1e-8 + ep[i])).sqrt();
    }
    interp_band_gain(&mut rf, &r);
    for i in 0..FREQ_SIZE {
        x[i].0 += rf[i] * p[i].0;
        x[i].1 += rf[i] * p[i].1;
    }
    let new_e = band_energy(x);
    for i in 0..NB_BANDS {
        norm[i] = (ex[i] / (1e-8 + new_e[i])).sqrt();
    }
    interp_band_gain(&mut normf, &norm);
    for i in 0..FREQ_SIZE {
        x[i].0 *= normf[i];
        x[i].1 *= normf[i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple deterministic white noise generator.
    struct Noise {
        state: u64,
    }
    impl Noise {
        fn next_f32(&mut self) -> f32 {
            // xorshift64*
            self.state ^= self.state >> 12;
            self.state ^= self.state << 25;
            self.state ^= self.state >> 27;
            let value = self.state.wrapping_mul(0x2545F4914F6CDD1D);
            ((value >> 40) as f32 / 16777216.0) - 1.0
        }
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn the_embedded_model_loads_and_the_frame_size_matches_upstream() {
        let denoiser = Denoiser::new();
        assert!(denoiser.is_some(), "the embedded RNNoise model must parse");
        assert_eq!(Denoiser::frame_size(), 480);
    }

    #[test]
    fn stationary_noise_is_suppressed() {
        let mut denoiser = Denoiser::new().unwrap();
        let mut noise = Noise { state: 0x9E3779B97F4A7C15 };
        // Warm up the network on noise so its gates settle.
        let mut frames = Vec::new();
        for _ in 0..60 {
            let mut frame = [0.0f32; 480];
            for sample in frame.iter_mut() {
                *sample = noise.next_f32() * 0.2;
            }
            frames.push(frame);
        }
        let input_rms = rms(&frames[40].clone());
        let mut vad_prob = 0.0;
        for frame in frames.iter_mut().take(60) {
            vad_prob = denoiser.process(frame);
        }
        let output_rms = rms(&frames[59]);
        assert!(
            output_rms < input_rms * 0.5,
            "noise not attenuated: {output_rms} vs {input_rms} (vad {vad_prob})"
        );
        assert!(vad_prob < 0.5, "noise should not read as voice: {vad_prob}");
    }

    #[test]
    fn processing_is_deterministic() {
        let run = || {
            let mut denoiser = Denoiser::new().unwrap();
            let mut noise = Noise { state: 42 };
            let mut frame = [0.0f32; 480];
            for sample in frame.iter_mut() {
                *sample = noise.next_f32() * 0.1;
            }
            for _ in 0..30 {
                denoiser.process(&mut frame);
            }
            frame
        };
        let first = run();
        let second = run();
        assert_eq!(first, second);
    }

    #[test]
    fn silence_stays_silent() {
        let mut denoiser = Denoiser::new().unwrap();
        let mut frame = [0.0f32; 480];
        for _ in 0..20 {
            denoiser.process(&mut frame);
        }
        assert!(frame.iter().all(|sample| sample.abs() < 1e-4));
    }
}
