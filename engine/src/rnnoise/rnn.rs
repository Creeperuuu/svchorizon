//! The network: float `compute_linear` (dense and block-sparse), GRU, the
//! two-tap input convolutions and the exact activations of the official
//! build (`tanh_approx`, `sigmoid_approx` in vec.h).

use super::weights::{Layer, Model};
use super::{NB_BANDS, NB_FEATURES};

/// `tanh_approx` (vec.h): degree-2 rational approximation, clamped to ±1.
pub(crate) fn tanh_approx(x: f32) -> f32 {
    const N0: f32 = 952.52801514;
    const N1: f32 = 96.39235687;
    const N2: f32 = 0.60863042;
    const D0: f32 = 952.72399902;
    const D1: f32 = 413.36801147;
    const D2: f32 = 11.88600922;
    let x2 = x * x;
    let num = (N2).mul_add(x2, N1).mul_add(x2, N0);
    let den = (D2).mul_add(x2, D1).mul_add(x2, D0);
    (num * x / den).clamp(-1.0, 1.0)
}

/// `sigmoid_approx` (vec.h).
pub(crate) fn sigmoid_approx(x: f32) -> f32 {
    0.5 + 0.5 * tanh_approx(0.5 * x)
}

/// `compute_linear` (nnet_arch.h) float path: `out[i] = sum_j
/// weights[j * N + i] * in[j] + bias[i]` for the dense layout, or the
/// block-sparse `sparse_sgemv8x4` when the layer carries an index.
fn compute_linear(layer: &Layer, out: &mut [f32], input: &[f32]) {
    let rows = layer.nb_outputs;
    match layer.index {
        None => {
            let weights = layer.weights;
            for i in 0..rows {
                let mut sum = 0.0f32;
                for (j, value) in input.iter().take(layer.nb_inputs).enumerate() {
                    sum += weights[j * rows + i] * value;
                }
                out[i] = sum + layer.bias[i];
            }
        }
        Some(index) => {
            for value in out[..rows].iter_mut() {
                *value = 0.0;
            }
            // `sparse_sgemv8x4`: per 8-row group, the index names the 4-wide
            // input blocks that exist; each block's 32 weights are
            // row-major-in-block (8 rows x 4 columns).
            let weights = layer.weights;
            let mut idx = 0usize;
            let mut w = 0usize;
            let mut row_group = 0usize;
            while row_group < rows {
                let block_count = index[idx] as usize;
                idx += 1;
                for _ in 0..block_count {
                    let pos = index[idx] as usize;
                    idx += 1;
                    for c in 0..4 {
                        let xj = input[pos + c];
                        for r in 0..8 {
                            out[row_group + r] += weights[w + 8 * r + c] * xj;
                        }
                    }
                    w += 32;
                }
                row_group += 8;
            }
            for (value, bias) in out[..rows].iter_mut().zip(layer.bias.iter()) {
                *value += bias;
            }
        }
    }
}

/// `compute_generic_gru` (nnet.c), with the recurrent layer's diagonal term
/// (`linear->diag`) that the trained GRUs carry.
fn compute_gru(
    input_layer: &Layer,
    recurrent_layer: &Layer,
    state: &mut [f32],
    input: &[f32],
    zrh: &mut [f32],
    recur: &mut [f32],
) {
    let n = recurrent_layer.nb_inputs;
    compute_linear(input_layer, zrh, input);
    compute_linear(recurrent_layer, recur, state);
    if let Some(diag) = recurrent_layer.diag {
        for i in 0..n {
            recur[i] += diag[i] * state[i];
            recur[i + n] += diag[i + n] * state[i];
            recur[i + 2 * n] += diag[i + 2 * n] * state[i];
        }
    }
    for i in 0..2 * n {
        zrh[i] += recur[i];
    }
    for value in zrh[..2 * n].iter_mut() {
        *value = sigmoid_approx(*value);
    }
    for i in 0..n {
        zrh[2 * n + i] += recur[2 * n + i] * zrh[n + i];
    }
    for value in zrh[2 * n..3 * n].iter_mut() {
        *value = tanh_approx(*value);
    }
    for i in 0..n {
        let z = zrh[i];
        let h = zrh[2 * n + i];
        state[i] = z * state[i] + (1.0 - z) * h;
    }
}

/// `compute_generic_conv1d` (nnet.c): the layer input is the
/// `nb_inputs - input_size` kept samples followed by the current input.
fn compute_conv1d(
    layer: &Layer,
    output: &mut [f32],
    mem: &mut [f32],
    input: &[f32],
    input_size: usize,
    conv_buffer: &mut [f32],
) {
    let total = layer.nb_inputs;
    let keep = total - input_size;
    conv_buffer[..keep].copy_from_slice(&mem[..keep]);
    conv_buffer[keep..total].copy_from_slice(&input[..input_size]);
    compute_linear(layer, output, &conv_buffer[..total]);
    mem[..keep].copy_from_slice(&conv_buffer[input_size..total]);
}

pub(crate) struct RnnState {
    conv1_state: Vec<f32>,
    conv2_state: Vec<f32>,
    gru1_state: Vec<f32>,
    gru2_state: Vec<f32>,
    gru3_state: Vec<f32>,
    conv_buffer: Vec<f32>,
    zrh: Vec<f32>,
    recur: Vec<f32>,
    tmp: Vec<f32>,
    tmp2: Vec<f32>,
}

impl RnnState {
    pub(crate) fn new() -> Self {
        Self {
            conv1_state: vec![0.0; 65 * 2],
            conv2_state: vec![0.0; 128 * 2],
            gru1_state: vec![0.0; 384],
            gru2_state: vec![0.0; 384],
            gru3_state: vec![0.0; 384],
            conv_buffer: vec![0.0; 384],
            zrh: vec![0.0; 3 * 384],
            recur: vec![0.0; 3 * 384],
            tmp: vec![0.0; 384],
            tmp2: vec![0.0; 384],
        }
    }
}

/// `compute_rnn` (rnn.c at the pinned revision): two convolutions, three
/// GRUs, then the band gains and the voice probability.
pub(crate) fn compute_rnn(
    model: &Model,
    rnn: &mut RnnState,
    gains: &mut [f32; NB_BANDS],
    vad: &mut f32,
    input: &[f32; NB_FEATURES],
) {
    compute_conv1d(
        &model.conv1, &mut rnn.tmp, &mut rnn.conv1_state, input, 65, &mut rnn.conv_buffer,
    );
    for value in rnn.tmp[..128].iter_mut() {
        *value = tanh_approx(*value);
    }
    let conv1_out: Vec<f32> = rnn.tmp[..128].to_vec();
    compute_conv1d(
        &model.conv2,
        &mut rnn.tmp2,
        &mut rnn.conv2_state,
        &conv1_out,
        128,
        &mut rnn.conv_buffer,
    );
    for value in rnn.tmp2[..384].iter_mut() {
        *value = tanh_approx(*value);
    }
    let conv2_out: Vec<f32> = rnn.tmp2[..384].to_vec();
    let (zrh, recur) = (&mut rnn.zrh, &mut rnn.recur);
    compute_gru(&model.gru1_input, &model.gru1_recurrent, &mut rnn.gru1_state, &conv2_out, zrh, recur);
    let gru1_out: Vec<f32> = rnn.gru1_state.clone();
    compute_gru(&model.gru2_input, &model.gru2_recurrent, &mut rnn.gru2_state, &gru1_out, zrh, recur);
    let gru2_out: Vec<f32> = rnn.gru2_state.clone();
    compute_gru(&model.gru3_input, &model.gru3_recurrent, &mut rnn.gru3_state, &gru2_out, zrh, recur);
    let gru3_out: Vec<f32> = rnn.gru3_state.clone();
    compute_linear(&model.dense_out, gains, &gru3_out);
    for value in gains.iter_mut() {
        *value = sigmoid_approx(*value);
    }
    let mut vad_out = [0.0f32; 1];
    compute_linear(&model.vad_dense, &mut vad_out, &gru3_out);
    *vad = sigmoid_approx(vad_out[0]);
}

/// `rnn_biquad` (denoise.c): `b` and `a` are the two-coefficient filters
/// without their leading 1.0.
pub(crate) fn biquad(y: &mut [f32], mem: &mut [f32; 2], x: &[f32], b: [f32; 2], a: [f32; 2]) {
    for (i, xi) in x.iter().enumerate() {
        let yi = x[i] + mem[0];
        mem[0] = mem[1] + b[0] * xi - a[0] * yi;
        mem[1] = b[1] * xi - a[1] * yi;
        y[i] = yi;
    }
}
