mod support;

use ota_generator::control::{Control, Halt};
use ota_generator::evaluate::Problem;
use ota_generator::gpu_layout::GRID;
use ota_generator::search::open_session;
use ota_generator::mapper::Mapping;
use ota_generator::memory::Images;
use ota_generator::program::{Kind, Op, Span};
use ota_generator::{SearchConfig, build_payload_with, check::check_payload};
use ota_payload::{error::PayloadError, layout, profile::EMBEDDED, rsa::parse_pubkey_pem};

const PRIVATE: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/private.pem");
const PUBLIC: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/public.pem");

fn ok<T>(result: Result<T, PayloadError>) -> T {
    result.unwrap_or_else(|e| panic!("{}", e.msg))
}

fn done<T>(result: Result<T, Halt>) -> T {
    match result {
        Ok(v) => v,
        Err(Halt::Failed(e)) => panic!("{}", e.msg),
        Err(Halt::Deadline) => panic!("unexpected deadline"),
    }
}

fn span(start: u64, len: u64) -> Span {
    Span { start, len }
}

#[test]
fn generated_payloads_grow_and_shrink_including_empty_images() {
    let config = SearchConfig {
        device: support::device(),
        max_generations: Some(4),
        ..SearchConfig::quick()
    };
    for (old_len, new_len) in [(67, 257), (257, 67), (0, 129), (129, 0)] {
        let old: Vec<u8> = (0..old_len).map(|i| (i % 251) as u8).collect();
        let mut new = old[..old_len.min(new_len)].to_vec();
        new.resize(new_len, 0);
        if !new.is_empty() {
            new[0] = 99;
        }
        let payload = ok(build_payload_with(
            &old, &new, b"root", PRIVATE, 16, &EMBEDDED, &config,
        ));
        ok(check_payload(
            &payload,
            ok(parse_pubkey_pem(PUBLIC)),
            &old,
            &new,
            &EMBEDDED,
        ));
        let parsed = ok(layout::parse_unverified(&payload, &EMBEDDED)).into_parsed();
        let part = &parsed.manifest.partitions[0];
        assert_eq!(
            part.old_partition_info.as_ref().unwrap().size,
            Some(old_len as u64)
        );
        assert_eq!(
            part.new_partition_info.as_ref().unwrap().size,
            Some(new_len as u64)
        );
    }
}

#[test]
fn mapping_requires_writes_for_growth_even_when_the_new_bytes_are_zero() {
    let old = vec![0; 3];
    let new = vec![0; 19];
    let mapping = Mapping::new(
        Images {
            old: &old,
            new: &new,
        },
        4,
    );
    let ctl = Control::unbounded();
    assert_eq!(
        done(mapping.map(&[], &ctl, true)).mismatches,
        vec![span(3, 16)]
    );
    let mapped = done(mapping.map(&[Op::new(Kind::Sparse, vec![span(0, 19)])], &ctl, true));
    assert!(mapped.mismatches.is_empty());
    let mut copy = Op::new(Kind::TargetCopy, vec![span(0, 4)]);
    copy.src = vec![span(8, 4)];
    assert!(mapping.map(&[copy.clone()], &ctl, true).is_err());
    let mapped = done(mapping.map(&[Op::new(Kind::Zero, vec![span(8, 4)]), copy], &ctl, true));
    assert!(!mapped.mismatches.is_empty());
}

#[test]
fn mapping_source_copy_bounds_are_independent_from_new_image_bounds() {
    let ctl = Control::unbounded();
    let old: Vec<u8> = (0..32).collect();
    let new = old[24..32].to_vec();
    let mut copy = Op::new(Kind::SourceCopy, vec![span(0, 8)]);
    copy.src = vec![span(24, 8)];
    let mapping = Mapping::new(
        Images {
            old: &old,
            new: &new,
        },
        4,
    );
    assert!(done(mapping.map(&[copy], &ctl, true)).mismatches.is_empty());
    let mapping = Mapping::new(
        Images {
            old: &new,
            new: &old,
        },
        4,
    );
    let mut copy = Op::new(Kind::SourceCopy, vec![span(24, 8)]);
    copy.src = vec![span(8, 8)];
    assert!(mapping.map(&[copy], &ctl, true).is_err());
}

fn classes_on_the_host(old: &[u8], new: &[u8]) -> Vec<u8> {
    let cells = new.len().div_ceil(GRID);
    return (0..cells)
        .map(|g| {
            let a = g * GRID;
            let b = std::cmp::min(a + GRID, new.len());
            let equal = b <= old.len() && old[a..b] == new[a..b];
            let zero = new[a..b].iter().all(|x| *x == 0);
            (equal as u8) | ((zero as u8) << 1)
        })
        .collect();
}

fn session_for<'a>(old: &'a [u8], new: &'a [u8]) -> ota_generator::gpu::Session {
    let problem = ok(Problem::new(old, new, 16, "A/B", &EMBEDDED, 256));
    let config = SearchConfig {
        device: support::device(),
        ..SearchConfig::quick()
    };
    return ok(open_session(&problem, &config));
}

#[test]
fn gpu_classification_uses_the_new_grid_and_never_keeps_growth() {
    let cases: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (vec![0; 3], vec![0; 9]),
        (vec![0; 9], vec![0; 3]),
        (vec![0; GRID + 5], vec![0; 3 * GRID + 9]),
        ((0..2000u32).map(|k| (k % 251) as u8).collect(), {
            let mut v: Vec<u8> = (0..2000u32).map(|k| (k % 251) as u8).collect();
            v[700..900].fill(0);
            v[1500] ^= 1;
            v
        }),
        (vec![], vec![0; 700]),
        (vec![7; 700], vec![]),
    ];
    for (old, new) in &cases {
        let mut session = session_for(old, new);
        ok(session.prepare());
        let got = ok(session.classes());
        let want = classes_on_the_host(old, new);
        assert_eq!(got, want, "old {} new {}", old.len(), new.len());
        let changed = ok(session.changed_cells());
        let expected: Vec<u32> = (0..want.len() as u32).filter(|g| want[*g as usize] & 1 == 0).collect();
        assert_eq!(changed, expected, "the changed cells, in order, for old {} new {}", old.len(), new.len());
    }
}
