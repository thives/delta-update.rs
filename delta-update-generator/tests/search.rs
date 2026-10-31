mod support;

use std::time::Instant;

use delta_update_generator::DevicePreference;
use delta_update_generator::SearchConfig;
use delta_update_generator::build_payload_with;
use delta_update_generator::evaluate::Problem;
use delta_update_generator::search;
use delta_update_payload::layout;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::profile::HOST;
use delta_update_payload::profile::ResourceProfile;

const PRIV: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");

fn success<T>(result: Result<T, delta_update_payload::error::PayloadError>) -> T {
    return result.unwrap_or_else(|e| panic!("{}", e.msg));
}

fn problem_for<'a>(old: &'a [u8], new: &'a [u8], chunk: usize) -> Problem<'a> {
    return success(Problem::new(old, new, chunk, "A/B", &EMBEDDED, 256));
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 24) as u8);
    }
    return out;
}

fn by_generations(seed: u64, gens: u64) -> SearchConfig {
    return SearchConfig {
        release_version: 0,
        max_search_seconds: 120.0,
        stagnation_seconds: 3600.0,
        min_improvement_bytes: 1,
        seed: seed,
        max_generations: Some(gens),
        device: support::device(),
        readable_target: true,
    };
}

fn quick() -> SearchConfig {
    return SearchConfig {
        release_version: 0,
        max_search_seconds: 3.0,
        stagnation_seconds: 0.5,
        min_improvement_bytes: 1,
        seed: 1,
        max_generations: None,
        device: support::device(),
        readable_target: true,
    };
}

#[test]
fn a_relocated_region_is_found_as_a_copy_and_shrinks_the_payload() {
    let old = noise(8192, 1);
    let mut new = old.clone();
    let moved: Vec<u8> = old[13..13 + 3000].to_vec();
    new[4001..4001 + 3000].copy_from_slice(&moved);
    let payload = success(build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIV,
        64,
        &EMBEDDED,
        &by_generations(2, 40),
    ));
    assert!(
        payload.len() < 8192 - 2000,
        "payload is {} bytes, expected a copy to save at least 2000",
        payload.len()
    );
}

#[test]
fn the_search_reports_its_raw_score_apart_from_the_size_of_the_payload_it_builds() {
    let old = noise(5000, 4);
    let mut new = old.clone();
    new[300..900].fill(0);
    new[2000..2200].copy_from_slice(&old[100..300]);
    let config = by_generations(3, 25);
    let built = success(delta_update_generator::build_report(
        &old, &new, b"A/B", PRIV, 64, &EMBEDDED, &config,
    ));

    let problem = problem_for(&old, &new, 64);
    let found = success(search::run(&problem, &config));
    let best = found.finalists[0].raw_size;
    assert_eq!(
        built.raw_fitness,
        Some(best),
        "the raw score is what the search optimized"
    );
    let admitted = built.raw_admitted.unwrap_or(u64::MAX);
    assert!(
        built.payload.len() as u64 <= admitted,
        "{} bytes against a raw {}",
        built.payload.len(),
        admitted
    );
    assert!(
        admitted <= best || built.candidates > 1,
        "the raw admitted size {} against the score {}",
        admitted,
        best
    );
}

#[test]
fn a_seed_and_a_generation_cap_reproduce_the_same_bytes() {
    let old = noise(3000, 8);
    let mut new = old.clone();
    new[1000..1700].fill(7);
    let a = success(build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIV,
        32,
        &EMBEDDED,
        &by_generations(5, 12),
    ));
    let b = success(build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIV,
        32,
        &EMBEDDED,
        &by_generations(5, 12),
    ));
    assert_eq!(a, b);
}

#[test]
fn the_forced_software_device_builds_a_payload_that_verifies() {
    let old = noise(2048, 9);
    let mut new = old.clone();
    new[17..401].fill(0x5a);
    let payload = success(build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIV,
        16,
        &HOST,
        &quick(),
    ));
    success(layout::parse_unverified(&payload, &HOST));
}

#[test]
fn the_clock_bounds_the_search_time() {
    let old = noise(4096, 10);
    let mut new = old.clone();
    new[5..3000].fill(3);
    let config = SearchConfig {
        release_version: 0,
        max_search_seconds: 0.5,
        stagnation_seconds: 3600.0,
        min_improvement_bytes: 1,
        seed: 1,
        max_generations: None,
        device: support::device(),
        readable_target: true,
    };
    let started = Instant::now();
    let _ = success(build_payload_with(
        &old, &new, b"A/B", PRIV, 64, &EMBEDDED, &config,
    ));
    let took = started.elapsed().as_secs_f64();
    assert!(
        took < 30.0,
        "the search ran for {:.2} s against a 0.5 s limit",
        took
    );
}

#[test]
fn stagnation_restarts_the_population_before_ending_the_search() {
    let old = noise(2048, 12);
    let mut new = old.clone();
    new[100..140].fill(1);
    let config = SearchConfig {
        release_version: 0,
        max_search_seconds: 60.0,
        stagnation_seconds: 0.4,
        min_improvement_bytes: 1 << 20,
        seed: 2,
        max_generations: None,
        device: support::device(),
        readable_target: true,
    };
    let problem = problem_for(&old, &new, 32);
    let started = Instant::now();
    let found = success(search::run(&problem, &config));
    let took = started.elapsed().as_secs_f64();
    assert_eq!(found.stop, delta_update_generator::Stop::Stagnation);
    assert!(found.restarts >= 3, "the search stopped after {} restarts", found.restarts);
    assert!(
        found.generations >= u64::from(found.restarts),
        "{} generations for {} restarts: a restarted population did not get to run",
        found.generations,
        found.restarts
    );
    assert!(took < 10.0, "a stagnant search ran for {:.2} s", took);
}

#[test]
fn one_byte_images_and_images_shorter_than_a_chunk_build() {
    let profile: ResourceProfile = EMBEDDED;
    for len in [1usize, 2, 3, 5] {
        let old = noise(len, 20 + len as u64);
        let mut new = old.clone();
        new[len - 1] ^= 0x80;
        let payload = success(build_payload_with(
            &old,
            &new,
            b"A/B",
            PRIV,
            4096,
            &profile,
            &quick(),
        ));
        success(layout::parse_unverified(&payload, &profile));
    }
}

#[test]
fn an_odd_length_image_with_a_partial_final_chunk_builds() {
    let old = noise(4099, 31);
    let mut new = old.clone();
    new[4090..4099].fill(0);
    let payload = success(build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIV,
        4096,
        &EMBEDDED,
        &quick(),
    ));
    success(layout::parse_unverified(&payload, &EMBEDDED));
}

#[test]
fn the_automatic_device_builds_a_payload_that_verifies() {
    let old = noise(2048, 15);
    let mut new = old.clone();
    new[300..700].fill(0x21);
    let auto = SearchConfig {
        release_version: 0,
        device: DevicePreference::Auto,
        ..by_generations(6, 20)
    };
    let payload = success(build_payload_with(
        &old, &new, b"A/B", PRIV, 32, &EMBEDDED, &auto,
    ));
    success(layout::parse_unverified(&payload, &EMBEDDED));
}

#[test]
fn one_byte_chunks_build_and_pass_the_checks() {
    let old = noise(8192, 61);
    let mut new = old.clone();
    new[1000..3000].fill(0);
    new[5000..5600].copy_from_slice(&old[100..700]);
    for profile in [&EMBEDDED, &HOST] {
        let payload = success(build_payload_with(
            &old,
            &new,
            b"A/B",
            PRIV,
            1,
            profile,
            &quick(),
        ));
        success(layout::parse_unverified(&payload, profile));
    }
}

const RECORDED_LEN: usize = 1183;
const RECORDED_DIGEST: [u8; 8] = [0x47, 0xf2, 0x45, 0x3b, 0x83, 0xc7, 0x17, 0x51];

#[test]
fn a_fixed_generation_search_reproduces_its_recorded_payload() {
    let old = noise(1 << 16, 3);
    let mut new = old.clone();
    new[10_000..30_000].copy_from_slice(&old[40_000..60_000]);
    new[50_000..52_000].fill(0);
    new[20_000..20_100].copy_from_slice(&noise(100, 8));
    let built = success(delta_update_generator::build_report(
        &old,
        &new,
        b"A/B",
        PRIV,
        4096,
        &HOST,
        &by_generations(1, 40),
    ));
    let digest = delta_update_payload::sha256::hash_bytes(&built.payload);
    eprintln!(
        "payload {} bytes, digest {:02x?}",
        built.payload.len(),
        &digest[..8]
    );
    assert_eq!(built.payload.len(), RECORDED_LEN);
    assert_eq!(&digest[..8], &RECORDED_DIGEST[..]);
}
