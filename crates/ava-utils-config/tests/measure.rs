use ava_utils_config::envelope::Envelope;
use ava_utils_config::messages::{FILES_CHANGED, FilesChanged};
use rmp_serde::{from_slice, to_vec_named};
use std::time::Instant;

/// Generate 50,000 paths of ~60 bytes each
fn generate_paths(count: usize, _avg_len: usize) -> Vec<String> {
    let base = "/very/long/base/path/that/is/about/sixty/characters/long/";
    (0..count).map(|i| format!("{}{:08}", base, i)).collect()
}

/// Median of a sorted slice
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let len = values.len();
    if len.is_multiple_of(2) {
        (values[len / 2 - 1] + values[len / 2]) / 2.0
    } else {
        values[len / 2]
    }
}

#[test]
#[ignore]
fn measure_files_changed_encoding_cost() {
    const RUNS: usize = 20;
    const PATH_COUNT: usize = 50_000;
    const AVG_PATH_LEN: usize = 60;

    let paths = generate_paths(PATH_COUNT, AVG_PATH_LEN);

    // 1. Named boxes via Envelope (current approach)
    let batch = FilesChanged {
        changed: paths.clone(),
        removed: vec![],
        ..Default::default()
    };

    // Warm-up
    let env = Envelope::event(FILES_CHANGED, &batch).unwrap();
    let _ = env.encode().unwrap();

    let mut encode_times = Vec::with_capacity(RUNS);
    let mut decode_times = Vec::with_capacity(RUNS);
    let mut encoded_sizes = Vec::with_capacity(RUNS);

    for _ in 0..RUNS {
        let env = Envelope::event(FILES_CHANGED, &batch).unwrap();

        let start = Instant::now();
        let encoded = env.encode().unwrap();
        encode_times.push(start.elapsed().as_secs_f64() * 1000.0); // ms

        let start = Instant::now();
        let decoded = Envelope::decode(&encoded).unwrap();
        let _back: FilesChanged = decoded.body_as().unwrap();
        decode_times.push(start.elapsed().as_secs_f64() * 1000.0); // ms

        encoded_sizes.push(encoded.len());
    }

    let median_encode = median(&mut encode_times);
    let median_decode = median(&mut decode_times);
    let avg_size: usize = encoded_sizes.iter().sum::<usize>() / encoded_sizes.len();

    // 2. Plain array of strings (no Envelope, no named boxes)
    let plain_paths = paths.clone();

    let mut plain_encode_times = Vec::with_capacity(RUNS);
    let mut plain_decode_times = Vec::with_capacity(RUNS);
    let mut plain_sizes = Vec::with_capacity(RUNS);

    for _ in 0..RUNS {
        let start = Instant::now();
        let encoded = to_vec_named(&plain_paths).unwrap();
        plain_encode_times.push(start.elapsed().as_secs_f64() * 1000.0);

        let start = Instant::now();
        let _decoded: Vec<String> = from_slice(&encoded).unwrap();
        plain_decode_times.push(start.elapsed().as_secs_f64() * 1000.0);

        plain_sizes.push(encoded.len());
    }

    let plain_median_encode = median(&mut plain_encode_times);
    let plain_median_decode = median(&mut plain_decode_times);
    let plain_avg_size: usize = plain_sizes.iter().sum::<usize>() / plain_sizes.len();

    // Print results
    eprintln!("\n=== Measured cost: FilesChanged with 50,000 paths (~60 bytes each) ===");
    eprintln!("Profile: debug (cargo test default)");
    eprintln!("Runs: {} (median reported)", RUNS);
    eprintln!();
    eprintln!("Named boxes (Envelope::encode):");
    eprintln!("  Encode: {:.2} ms", median_encode);
    eprintln!("  Decode: {:.2} ms", median_decode);
    eprintln!(
        "  Size:   {} bytes ({:.2} MiB)",
        avg_size,
        avg_size as f64 / 1_048_576.0
    );
    eprintln!();
    eprintln!("Plain array of strings (rmp_serde::to_vec_named):");
    eprintln!("  Encode: {:.2} ms", plain_median_encode);
    eprintln!("  Decode: {:.2} ms", plain_median_decode);
    eprintln!(
        "  Size:   {} bytes ({:.2} MiB)",
        plain_avg_size,
        plain_avg_size as f64 / 1_048_576.0
    );
    eprintln!();
    eprintln!("Overhead of named boxes:");
    eprintln!(
        "  Encode: +{:.1}% ({:.2} ms)",
        (median_encode - plain_median_encode) / plain_median_encode * 100.0,
        median_encode - plain_median_encode
    );
    eprintln!(
        "  Decode: +{:.1}% ({:.2} ms)",
        (median_decode - plain_median_decode) / plain_median_decode * 100.0,
        median_decode - plain_median_decode
    );
    eprintln!(
        "  Size:   +{:.1}% ({} bytes)",
        (avg_size - plain_avg_size) as f64 / plain_avg_size as f64 * 100.0,
        avg_size - plain_avg_size
    );
}
