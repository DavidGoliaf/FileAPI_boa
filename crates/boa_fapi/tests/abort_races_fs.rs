//! M7 filesystem race: mutation after capability import.
//!
//! Split out of `abort_races.rs` so the `fs`-gated test compiles without
//! unused-import/variable warnings in either feature configuration: this
//! file only exists with the `fs` feature (Unix asserts live revalidation,
//! non-Unix asserts point-in-time copy semantics).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(feature = "fs")]

#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(unix)]
use boa_engine::js_string;
#[cfg(unix)]
use boa_engine::{Context, Source};
#[cfg(unix)]
use boa_fapi::HostFileOptions;
#[cfg(unix)]
use boa_fapi::{Clock, FileApiExtension};

#[cfg(unix)]
#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

#[cfg(unix)]
impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

#[cfg(unix)]
const FIXED_TIME: i64 = 1_700_000_000_000;

#[cfg(unix)]
fn setup() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

#[cfg(unix)]
fn assert_eval(context: &mut Context, source: &str) {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    assert!(
        value.to_boolean(),
        "JS assertion failed: {source} (got {value:?})"
    );
}

#[cfg(unix)]
fn eval_side_effect(context: &mut Context, source: &str) {
    context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
}

#[cfg(unix)]
fn drain(context: &mut Context, handle: &boa_fapi::FileApiHandle) {
    for _ in 0..16 {
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        if settled == 0 && !handle.has_pending_io() {
            context.run_jobs().expect("run_jobs");
            if !handle.has_pending_io() {
                break;
            }
        }
    }
}

#[test]
fn filesystem_mutation_after_creation() {
    let registry = boa_fapi_fs::FsRegistry::new();
    #[cfg(unix)]
    {
        let (mut context, handle) = setup();
        static RACE_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = RACE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("boa-fapi-race-{id}.bin"));
        std::fs::write(&path, b"stable").expect("write");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open");
        let resource = registry.register(file).expect("register");
        let adapter = Arc::new(
            boa_fapi_fs::HostFileSource::new(&registry, &resource, None).expect("adapter"),
        );
        let object = handle
            .file_from_resource(
                &registry,
                adapter,
                "race.txt",
                HostFileOptions::default(),
                &mut context,
            )
            .expect("import");
        context
            .register_global_property(
                js_string!("raceFile"),
                object,
                boa_engine::property::Attribute::all(),
            )
            .expect("publish");
        assert_eval(&mut context, "raceFile.size === 6");
        std::fs::write(&path, b"mutated-longer").expect("mutate");
        eval_side_effect(
            &mut context,
            "globalThis.raceVerdict = 'pending'; \
             raceFile.text().then(v => { globalThis.raceVerdict = v; }, e => { globalThis.raceVerdict = e.name; });",
        );
        drain(&mut context, &handle);
        let verdict = context
            .eval(Source::from_bytes("globalThis.raceVerdict"))
            .expect("verdict")
            .as_string()
            .expect("string")
            .to_std_string_escaped();
        // Mutation surfaces as a mapped DOMException, never partial bytes.
        assert!(
            verdict.as_str() == "NotReadableError" || verdict.as_str() == "NotFoundError",
            "unexpected verdict {verdict:?}"
        );
        std::fs::remove_file(&path).ok();
    }
    #[cfg(not(unix))]
    {
        let mut path = std::env::temp_dir();
        path.push("boa-fapi-race-copy.bin");
        std::fs::write(&path, b"stable").expect("write");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open");
        let resource = registry.register(file).expect("register");
        let bytes = boa_fapi_fs::open_copy_on_import(&registry, &resource, u64::MAX).expect("copy");
        assert_eq!(&bytes[..], b"stable");
        std::fs::remove_file(&path).ok();
    }
}
