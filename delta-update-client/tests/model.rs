mod support;

use delta_update_generator::builder::Spec;
use delta_update_generator::builder::build;
use support::model::*;
use support::*;

const DEVICE: usize = 40_000;
const CASES: u64 = 48;

const PRIVATE_PEM: &[u8] = PRIVATE;

fn program(seed: u64) -> (usize, Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut rng = Rng::new(seed);
    let chunk = [512usize, 1000, 4096][rng.below(3) as usize];
    let old = rng.bytes(DEVICE);
    let count = 1 + rng.below(5) as usize;
    let (ops, want) = random_program(&mut rng, DEVICE, chunk, &old, &old, count);
    let payload = build(&Spec {
        chunk: chunk,
        partition: DEVICE as u64,
        old_hash: None,
        ops: ops,
        sign: Some(PRIVATE_PEM),
    })
    .map_err(|e| e.msg)
    .unwrap();
    return (chunk, old, want, payload);
}

#[test]
fn random_programs_match_the_model_on_every_path() {
    for seed in 1..=CASES {
        let (chunk, old, want, payload) = program(seed);
        let image = Old::new(old.clone());

        let mut dev = Disk::new(old.clone());
        Session::new()
            .buffered_with_image(&payload, &mut dev, &image)
            .unwrap_or_else(|e| panic!("seed {seed}: buffered refused: {}", e.msg));
        assert!(
            dev.data == want,
            "seed {seed}: buffered diverged from the model"
        );
        let writes = dev.writes;

        let mut dev = Disk::new(old.clone());
        Session::new()
            .streamed_with_image(&payload, 777, &mut dev, &image)
            .unwrap_or_else(|e| panic!("seed {seed}: streamed refused: {}", e.msg));
        assert!(
            dev.data == want,
            "seed {seed}: streamed diverged from the model"
        );

        for streamed in [false, true] {
            let fail_at = (seed as usize * 7919) % writes.max(1);
            let mut session = Session::new();
            let mut dev = Flaky::new(old.clone(), fail_at);
            let first = if streamed {
                session.streamed_with_image(&payload, 613, &mut dev, &image)
            } else {
                session.buffered_with_image(&payload, &mut dev, &image)
            };
            assert!(
                first.is_err(),
                "seed {seed}: interruption at write {fail_at} did not stop the run"
            );
            dev.fail_at = None;
            let resumed = if streamed {
                session.streamed_with_image(&payload, 613, &mut dev, &image)
            } else {
                session.buffered_with_image(&payload, &mut dev, &image)
            };
            resumed.unwrap_or_else(|e| {
                panic!(
                    "seed {seed}: resume (streamed={streamed}) refused: {}",
                    e.msg
                )
            });
            assert!(
                dev.disk.data == want,
                "seed {seed}: resume (streamed={streamed}) diverged from the model after a cut at write {fail_at} (chunk {chunk})"
            );
        }
    }
}
