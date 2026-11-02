//! Pitch analysis: `pitch.c` and the `celt_lpc.c` helpers, float path.

use super::{PITCH_FRAME_SIZE, PITCH_MAX_PERIOD};

/// `rnn_pitch_downsample` (pitch.c), float path: decimate by 2, then fit a
/// 4-tap LPC over the decimated signal and apply the zero-adding pre-emphasis
/// FIR5, so the pitch search sees the whitened signal it was trained on.
pub(crate) fn downsample(x: &[f32], out: &mut [f32]) {
    let len = x.len();
    for i in 1..len / 2 {
        out[i] = 0.25 * (x[2 * i - 1] + x[2 * i + 1]) + 0.5 * x[2 * i];
    }
    out[0] = 0.25 * x[1] + 0.5 * x[0];
    let n = len / 2;
    // rnn_autocorr with no window and lag 4.
    let mut ac = [0.0f32; 5];
    autocorr(out, &mut ac, 4, n);
    // Noise floor -40 dB, then lag windowing.
    ac[0] *= 1.0001;
    for (i, value) in ac.iter_mut().enumerate().skip(1) {
        *value -= *value * (0.008 * i as f32) * (0.008 * i as f32);
    }
    // rnn_lpc, then the 0.9^i damping of the coefficients.
    let mut lpc_coefficients = [0.0f32; 4];
    lpc(&mut lpc_coefficients, &ac);
    let mut damping = 1.0f32;
    for coefficient in lpc_coefficients.iter_mut() {
        damping *= 0.9;
        *coefficient *= damping;
    }
    // Add a zero (the FIR5 numerator) and filter in place.
    let c1 = 0.8f32;
    let num = [
        lpc_coefficients[0] + c1,
        lpc_coefficients[1] + c1 * lpc_coefficients[0],
        lpc_coefficients[2] + c1 * lpc_coefficients[1],
        lpc_coefficients[3] + c1 * lpc_coefficients[2],
        c1 * lpc_coefficients[3],
    ];
    let source: Vec<f32> = out[..n].to_vec();
    let mut mem = [0.0f32; 5];
    fir5(&source, &num, out, n, &mut mem);
}

/// `celt_inner_prod` (pitch.h).
fn inner_prod(x: &[f32], y: &[f32], n: usize) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..n {
        sum += x[i] * y[i];
    }
    sum
}

/// `rnn_pitch_xcorr` (pitch.c): `xcorr[i] = <x, y[i..]>`.
fn pitch_xcorr(x: &[f32], y: &[f32], xcorr: &mut [f32], len: usize, max_pitch: usize) {
    for (i, slot) in xcorr.iter_mut().take(max_pitch).enumerate() {
        *slot = inner_prod(x, &y[i..], len);
    }
}

/// `rnn_autocorr` (celt_lpc.c), no window, into `ac[0..=lag]`.
fn autocorr(x: &[f32], ac: &mut [f32], lag: usize, n: usize) {
    let fast_n = n - lag;
    pitch_xcorr(x, x, ac, fast_n, lag + 1);
    for k in 0..=lag {
        let mut d = 0.0f32;
        for i in k + fast_n..n {
            d += x[i] * x[i - k];
        }
        ac[k] += d;
    }
}

/// `rnn_lpc` (celt_lpc.c): Levinson-Durbin over the autocorrelation.
fn lpc(out: &mut [f32], ac: &[f32]) {
    let p = out.len();
    out.iter_mut().for_each(|value| *value = 0.0);
    let mut error = ac[0];
    if ac[0] != 0.0 {
        for i in 0..p {
            let mut rr = 0.0f32;
            for j in 0..i {
                rr += out[j] * ac[i - j];
            }
            rr += ac[i + 1] * 0.125;
            let r = -(rr * 8.0) / error;
            out[i] = r * 0.125;
            for j in 0..(i + 1) / 2 {
                let tmp1 = out[j];
                let tmp2 = out[i - 1 - j];
                out[j] = tmp1 + r * tmp2;
                out[i - 1 - j] = tmp2 + r * tmp1;
            }
            error -= r * r * error;
            if error < 0.001 * ac[0] {
                break;
            }
        }
    }
}

/// `celt_fir5` (pitch.c): the 5-tap FIR the pitch pre-filter applies.
fn fir5(x: &[f32], num: &[f32; 5], y: &mut [f32], n: usize, mem: &mut [f32; 5]) {
    for i in 0..n {
        let sum = x[i]
            + num[0] * mem[0]
            + num[1] * mem[1]
            + num[2] * mem[2]
            + num[3] * mem[3]
            + num[4] * mem[4];
        mem[4] = mem[3];
        mem[3] = mem[2];
        mem[2] = mem[1];
        mem[1] = mem[0];
        mem[0] = x[i];
        y[i] = sum;
    }
}

/// `find_best_pitch` (pitch.c), float path.
fn find_best_pitch(xcorr: &[f32], y: &[f32], len: usize, max_pitch: usize, best: &mut [usize; 2]) {
    let mut syy = 1.0f32;
    let mut best_num = [-1.0f32; 2];
    let mut best_den = [0.0f32; 2];
    best[0] = 0;
    best[1] = 1;
    for value in y.iter().take(len) {
        syy += value * value;
    }
    for i in 0..max_pitch {
        if xcorr[i] > 0.0 {
            let xcorr16 = xcorr[i] * 1e-12;
            let num = xcorr16 * xcorr16;
            if num * best_den[1] > best_num[1] * syy {
                if num * best_den[0] > best_num[0] * syy {
                    best_num[1] = best_num[0];
                    best_den[1] = best_den[0];
                    best[1] = best[0];
                    best_num[0] = num;
                    best_den[0] = syy;
                    best[0] = i;
                } else {
                    best_num[1] = num;
                    best_den[1] = syy;
                    best[1] = i;
                }
            }
        }
        syy += y[i + len] * y[i + len] - y[i] * y[i];
        syy = syy.max(1.0);
    }
}

/// `rnn_pitch_search` (pitch.c). `x_lp` is the current frame's decimated
/// signal (the caller passes the offset window), `y` the whole decimated
/// history; the returned lag is in decimated units.
pub(crate) fn search(x_lp: &[f32], y: &[f32], len: usize, max_pitch: usize, pitch: &mut usize) {
    let lag = len + max_pitch;
    let mut best_pitch = [0usize; 2];
    // Downsample by 2 again (the C arrays are `PITCH_FRAME_SIZE>>2` and
    // `(PITCH_FRAME_SIZE+PITCH_MAX_PERIOD)>>2` long).
    const X_LP4_LEN: usize = PITCH_FRAME_SIZE / 4;
    const Y_LP4_LEN: usize = (PITCH_FRAME_SIZE + PITCH_MAX_PERIOD) / 4;
    let quarter_len = len / 4;
    let quarter_lag = lag / 4;
    let mut x_lp4 = [0.0f32; X_LP4_LEN];
    let mut y_lp4 = [0.0f32; Y_LP4_LEN];
    for (j, value) in x_lp4.iter_mut().take(quarter_len).enumerate() {
        *value = x_lp[2 * j];
    }
    for (j, value) in y_lp4.iter_mut().take(quarter_lag).enumerate() {
        *value = y[2 * j];
    }
    // Coarse search with 4x decimation.
    let mut xcorr = [0.0f32; PITCH_MAX_PERIOD / 2];
    pitch_xcorr(&x_lp4, &y_lp4, &mut xcorr, quarter_len, max_pitch / 4);
    find_best_pitch(&xcorr, &y_lp4, quarter_len, max_pitch / 4, &mut best_pitch);
    // Finer search with 2x decimation.
    for i in 0..max_pitch / 2 {
        if (i as isize - 2 * best_pitch[0] as isize).abs() > 2
            && (i as isize - 2 * best_pitch[1] as isize).abs() > 2
        {
            xcorr[i] = 0.0;
            continue;
        }
        let sum = inner_prod(x_lp, &y[i..], len / 2);
        xcorr[i] = sum.max(-1.0);
    }
    find_best_pitch(&xcorr, y, len / 2, max_pitch / 2, &mut best_pitch);
    // Refine by pseudo-interpolation.
    let offset: isize = if best_pitch[0] > 0 && best_pitch[0] < (max_pitch / 2) - 1 {
        let a = xcorr[best_pitch[0] - 1];
        let b = xcorr[best_pitch[0]];
        let c = xcorr[best_pitch[0] + 1];
        if (c - a) > 0.7 * (b - a) {
            1
        } else if (a - c) > 0.7 * (b - c) {
            -1
        } else {
            0
        }
    } else {
        0
    };
    *pitch = ((2 * best_pitch[0]) as isize - offset).max(0) as usize;
}

fn compute_pitch_gain(xy: f32, xx: f32, yy: f32) -> f32 {
    xy / (1.0 + xx * yy).sqrt()
}

const SECOND_CHECK: [usize; 16] = [0, 0, 3, 2, 3, 2, 5, 2, 3, 2, 3, 2, 5, 2, 3, 2];

/// `rnn_remove_doubling` (pitch.c): resolves octave errors after the search.
pub(crate) fn remove_doubling(
    x: &[f32],
    maxperiod: usize,
    minperiod: usize,
    n: usize,
    t0: &mut usize,
    prev_period: usize,
    prev_gain: f32,
) -> f32 {
    let minperiod0 = minperiod;
    let maxperiod = maxperiod / 2;
    let minperiod = minperiod / 2;
    *t0 /= 2;
    let prev_period = prev_period / 2;
    let n = n / 2;
    // `x` conceptually starts at `maxperiod`; the C pointer arithmetic is
    // expressed with an offset here.
    let offset = maxperiod;
    if *t0 >= maxperiod {
        *t0 = maxperiod - 1;
    }
    let at = |i: isize| x[(offset as isize + i) as usize];
    let mut t = *t0;
    let t0_value = *t0;
    let mut yy_lookup = [0.0f32; PITCH_MAX_PERIOD + 1];
    let mut xx = 0.0f32;
    let mut xy = 0.0f32;
    for i in 0..n {
        xx += at(i as isize) * at(i as isize);
        xy += at(i as isize) * at(i as isize - t0_value as isize);
    }
    yy_lookup[0] = xx;
    let mut yy = xx;
    for i in 1..=maxperiod {
        yy += at(-(i as isize)) * at(-(i as isize)) - at((n - i) as isize) * at((n - i) as isize);
        yy_lookup[i] = yy.max(0.0);
    }
    yy = yy_lookup[t0_value];
    let mut best_xy = xy;
    let mut best_yy = yy;
    let g0 = compute_pitch_gain(xy, xx, yy);
    let mut g = g0;
    // Look for any pitch at T/k.
    for k in 2..=15 {
        let t1 = (2 * t0_value + k) / (2 * k);
        if t1 < minperiod {
            break;
        }
        let t1b = if k == 2 {
            if t1 + t0_value > maxperiod {
                t0_value
            } else {
                t0_value + t1
            }
        } else {
            (2 * SECOND_CHECK[k] * t0_value + k) / (2 * k)
        };
        let mut xy1 = 0.0f32;
        let mut xy2 = 0.0f32;
        for i in 0..n {
            xy1 += at(i as isize) * at(i as isize - t1 as isize);
            xy2 += at(i as isize) * at(i as isize - t1b as isize);
        }
        let xy = 0.5 * (xy1 + xy2);
        let yy = 0.5 * (yy_lookup[t1] + yy_lookup[t1b]);
        let g1 = compute_pitch_gain(xy, xx, yy);
        let cont = if (t1 as isize - prev_period as isize).abs() <= 1 {
            prev_gain
        } else if (t1 as isize - prev_period as isize).abs() <= 2 && 5 * k * k < t0_value {
            0.5 * prev_gain
        } else {
            0.0
        };
        let mut thresh = (0.3f32).max(0.7 * g0 - cont);
        // Bias against very high pitch to avoid false positives.
        if t1 < 3 * minperiod {
            thresh = (0.4f32).max(0.85 * g0 - cont);
        } else if t1 < 2 * minperiod {
            thresh = (0.5f32).max(0.9 * g0 - cont);
        }
        if g1 > thresh {
            best_xy = xy;
            best_yy = yy;
            t = t1;
            g = g1;
        }
    }
    let best_xy = best_xy.max(0.0);
    let mut pg = if best_yy <= best_xy {
        1.0
    } else {
        best_xy / (best_yy + 1.0)
    };
    let mut xcorr = [0.0f32; 3];
    for (k, slot) in xcorr.iter_mut().enumerate() {
        *slot = inner_prod(
            &x[offset..offset + n],
            &x[offset - (t + k - 1)..],
            n,
        );
    }
    let offset_result: isize = if (xcorr[2] - xcorr[0]) > 0.7 * (xcorr[1] - xcorr[0]) {
        1
    } else if (xcorr[0] - xcorr[2]) > 0.7 * (xcorr[1] - xcorr[2]) {
        -1
    } else {
        0
    };
    if pg > g {
        pg = g;
    }
    *t0 = (2 * t as isize + offset_result).max(0) as usize;
    if *t0 < minperiod0 {
        *t0 = minperiod0;
    }
    pg
}
