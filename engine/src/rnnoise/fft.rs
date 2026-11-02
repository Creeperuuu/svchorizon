//! The 960-point transforms of denoise.c, on the engine's kiss_fft port.
//! Both upstream directions use the forward FFT (`rnn_fft`): the inverse
//! conjugate-extends the spectrum and reads the result reversed, undoing the
//! 1/960 scale the forward pass applies.

use super::{FREQ_SIZE, WINDOW_SIZE};
use crate::kiss_fft::{KissCpx, KissFftState};

/// `forward_transform` (denoise.c).
pub(crate) fn forward(
    fft: &KissFftState,
    input: &[f32; WINDOW_SIZE],
    out: &mut [(f32, f32); FREQ_SIZE],
) {
    let mut x: Vec<KissCpx> = Vec::with_capacity(WINDOW_SIZE);
    x.extend(input.iter().map(|value| KissCpx::new(*value, 0.0)));
    let mut y = vec![KissCpx::new(0.0, 0.0); WINDOW_SIZE];
    crate::kiss_fft::opus_fft(fft, &x, &mut y);
    for (slot, value) in out.iter_mut().zip(y.iter()) {
        *slot = (value.r, value.i);
    }
}

/// `inverse_transform` (denoise.c).
pub(crate) fn inverse(
    fft: &KissFftState,
    input: &[(f32, f32); FREQ_SIZE],
    out: &mut [f32; WINDOW_SIZE],
) {
    let mut x = vec![KissCpx::new(0.0, 0.0); WINDOW_SIZE];
    for (i, value) in input.iter().enumerate() {
        x[i] = KissCpx::new(value.0, value.1);
    }
    for i in FREQ_SIZE..WINDOW_SIZE {
        let mirror = &x[WINDOW_SIZE - i];
        x[i] = KissCpx::new(mirror.r, -mirror.i);
    }
    let mut y = vec![KissCpx::new(0.0, 0.0); WINDOW_SIZE];
    crate::kiss_fft::opus_fft(fft, &x, &mut y);
    out[0] = WINDOW_SIZE as f32 * y[0].r;
    for i in 1..WINDOW_SIZE {
        out[i] = WINDOW_SIZE as f32 * y[WINDOW_SIZE - i].r;
    }
}
