use svchorizon_engine::{Application, OpusDecoder, OpusEncoder};

#[test]
fn opus_voip_round_trip_20ms_frame() {
    let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip).unwrap();
    let input: Vec<i16> = (0..960)
        .map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 12_000.0) as i16)
        .collect();
    let mut encoded = [0u8; 1276];
    let length = encoder.encode_i16(&input, 960, &mut encoded).unwrap();
    assert!(length > 1 && length <= 1275);

    let mut decoder = OpusDecoder::new(48_000, 1).unwrap();
    let mut output = [0.0f32; 960];
    let samples = decoder.decode(&encoded[..length], 960, &mut output).unwrap();
    assert_eq!(samples, 960);
    assert!(output.iter().any(|sample| sample.abs() > 0.01));
}

#[test]
fn opus_supports_all_server_codec_modes() {
    for application in [Application::Voip, Application::Audio, Application::RestrictedLowDelay] {
        assert!(OpusEncoder::new(48_000, 1, application).is_ok());
    }
}
