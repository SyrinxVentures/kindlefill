//! End-to-end tests of the engine against `mtp-rs`'s virtual MTP device.
//!
//! The virtual device is backed by a real directory and reports free space as
//! capacity minus actual bytes on disk, so the measure-write-remeasure loop is
//! exercised for real — including the per-file overhead the plan tests only model.
//! This is as close to a Kindle as we get without one on the cable.
//!
//! What it does *not* prove: that a Kindle's `GetStorageInfo` updates promptly after
//! an upload. A device that caches free space would pass here and hang forever in the
//! real world. That is what `kindlefill bench` is for.

use kindlefill_core::{engine, plan::MIB, Window};
use mtp_rs::{MtpDevice, NewObjectInfo, ObjectHandle, Storage, VirtualDeviceConfig,
             VirtualStorageConfig};
use std::path::Path;
use std::time::Duration;

const CAPACITY: u64 = 64 * MIB;

/// These tests exercise the default folder name; `validate_dir_name` and the
/// alternate-name path are covered by unit tests in engine.rs.
const DIR: &str = engine::DEFAULT_FILL_DIR;

async fn open(backing: &Path) -> MtpDevice {
    MtpDevice::builder()
        .open_virtual(VirtualDeviceConfig {
            manufacturer: "Amazon".into(),
            model: "Virtual Kindle".into(),
            serial: "TEST0001".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: CAPACITY,
                backing_dir: backing.to_path_buf(),
                read_only: false,
            }],
            watch_backing_dirs: false,
            event_poll_interval: Duration::ZERO,
            ..Default::default()
        })
        .await
        .expect("virtual device should open")
}

async fn storage_of(device: &MtpDevice) -> Storage {
    device
        .storages()
        .await
        .expect("storages")
        .into_iter()
        .next()
        .expect("one storage")
}

/// Small enough to keep the test fast, still wider than `Window::MIN_WIDTH`.
fn window() -> Window {
    Window::new(8 * MIB, 16 * MIB).unwrap()
}

async fn free_space(storage: &mut Storage) -> u64 {
    storage.refresh().await.expect("refresh");
    storage.info().free_space
}

#[tokio::test]
async fn fill_lands_inside_the_window() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;
    let w = window();

    let outcome = engine::fill(&mut storage, w, DIR, |_| {}).await.expect("fill");

    match outcome {
        engine::Outcome::InWindow { free } => {
            assert!(w.contains(free), "landed at {free}, outside the window");
        }
        other => panic!("expected InWindow, got {other:?}"),
    }
    // And the device agrees, independently of what fill reported.
    assert!(w.contains(free_space(&mut storage).await));
}

#[tokio::test]
async fn fill_creates_the_fill_disk_folder_and_reports_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    let mut written = 0usize;
    engine::fill(&mut storage, window(), DIR, |event| {
        if matches!(event, engine::Event::Wrote { .. }) {
            written += 1;
        }
    })
    .await
    .expect("fill");

    assert!(written > 0, "should have written at least one object");
    let dir = engine::find_fill_dir(&storage, DIR)
        .await
        .expect("lookup")
        .expect("fill_disk should exist");
    assert_eq!(dir.filename, DIR);
    assert_eq!(
        engine::list_fillers(&storage, dir.handle).await.unwrap().len(),
        written
    );
    assert!(tmp.path().join(DIR).is_dir(), "should exist on disk");
}

#[tokio::test]
async fn clean_reclaims_everything_fill_consumed() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    let before = free_space(&mut storage).await;
    engine::fill(&mut storage, window(), DIR, |_| {}).await.expect("fill");
    assert!(free_space(&mut storage).await < before);

    engine::clean(&mut storage, DIR, |_| {}).await.expect("clean");

    assert_eq!(free_space(&mut storage).await, before, "should fully reclaim");
    assert!(
        engine::find_fill_dir(&storage, DIR).await.unwrap().is_none(),
        "empty fill_disk should be removed too"
    );
}

/// A progress bar is only useful if the numbers behind it are trustworthy. These are
/// the properties a UI depends on and can't easily check for itself.
#[tokio::test]
async fn progress_reporting_is_bounded_monotonic_and_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;
    let w = window();

    let before = free_space(&mut storage).await;
    let mut announced_total = None;
    let mut samples: Vec<kindlefill_core::FillProgress> = Vec::new();
    let mut objects = 0usize;

    engine::fill(&mut storage, w, DIR, |event| match event {
        engine::Event::Started { total, .. } => announced_total = Some(total),
        engine::Event::Progress(p) => samples.push(p),
        engine::Event::Wrote { .. } => objects += 1,
        _ => {}
    })
    .await
    .expect("fill");

    // The advertised job size is what the loop actually set out to move.
    assert_eq!(announced_total, Some(before - w.aim()));

    // Every object reports at least once, so the bar never sits silent through a write.
    assert!(
        samples.len() >= objects,
        "{} progress events for {objects} objects",
        samples.len()
    );

    for p in &samples {
        assert_eq!(p.total, announced_total.unwrap(), "total must not drift");
        assert!(
            (0.0..=1.0).contains(&p.fraction()),
            "fraction {} out of range",
            p.fraction()
        );
        assert!(p.percent().is_finite(), "percent must never be NaN");
        assert!(p.eta.is_none() || p.eta.unwrap() <= std::time::Duration::from_secs(99 * 3600));
    }

    // Progress only ever moves forward.
    for pair in samples.windows(2) {
        assert!(
            pair[1].done >= pair[0].done,
            "progress went backwards: {} then {}",
            pair[0].done,
            pair[1].done
        );
    }
}

/// An interrupted fill must top up, not start over or collide on filenames.
#[tokio::test]
async fn fill_resumes_from_an_existing_partial_fill() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;
    let w = window();

    engine::fill(&mut storage, w, DIR, |_| {}).await.expect("first fill");
    let dir = engine::find_fill_dir(&storage, DIR).await.unwrap().unwrap();
    let first_pass = engine::list_fillers(&storage, dir.handle).await.unwrap();

    // Simulate an interrupted run by removing the last object.
    let last = first_pass.last().expect("at least one filler");
    let names_before: Vec<_> = first_pass.iter().map(|f| f.filename.clone()).collect();
    storage.delete(last.handle).await.expect("delete");
    assert!(!w.contains(free_space(&mut storage).await));

    engine::fill(&mut storage, w, DIR, |_| {}).await.expect("second fill");

    assert!(w.contains(free_space(&mut storage).await));
    let after: Vec<_> = engine::list_fillers(&storage, dir.handle)
        .await
        .unwrap()
        .iter()
        .map(|f| f.filename.clone())
        .collect();
    let mut unique = after.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), after.len(), "filenames must not collide: {after:?}");
    // The surviving objects from the first pass were not rewritten.
    for name in names_before.iter().take(names_before.len() - 1) {
        assert!(after.contains(name), "{name} should have been kept");
    }
}

#[tokio::test]
async fn a_precancelled_token_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    let before = free_space(&mut storage).await;
    let cancel = mtp_rs::CancelToken::new();
    cancel.cancel();

    let outcome = engine::fill_with_cancel(&mut storage, window(), DIR, Some(&cancel), |_| {})
        .await
        .expect("cancelling is not an error");

    assert!(matches!(outcome, engine::Outcome::Cancelled { .. }));
    assert_eq!(free_space(&mut storage).await, before, "must not have written");
}

/// Stopping a 17-minute fill has to leave the device in a state you can pick back up
/// from — not half-broken, and not needing a clean before you can retry.
#[tokio::test]
async fn cancelling_mid_fill_leaves_a_resumable_state() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;
    let w = window();

    let before = free_space(&mut storage).await;
    let cancel = mtp_rs::CancelToken::new();
    let mut written = 0;

    let outcome = engine::fill_with_cancel(&mut storage, w, DIR, Some(&cancel), |event| {
        if matches!(event, engine::Event::Wrote { .. }) {
            written += 1;
            if written == 1 {
                cancel.cancel();
            }
        }
    })
    .await
    .expect("cancelling is not an error");

    assert!(matches!(outcome, engine::Outcome::Cancelled { .. }));
    let mid = free_space(&mut storage).await;
    assert!(mid < before, "should have made real progress before stopping");
    assert!(!w.contains(mid), "should have stopped short of the target");

    // The decisive property: a plain re-run finishes the job.
    let outcome = engine::fill(&mut storage, w, DIR, |_| {}).await.expect("resume");
    assert!(matches!(outcome, engine::Outcome::InWindow { .. }));
    assert!(w.contains(free_space(&mut storage).await));

    // And no filler was orphaned or duplicated along the way.
    let dir = engine::find_fill_dir(&storage, DIR).await.unwrap().unwrap();
    let names: Vec<_> = engine::list_fillers(&storage, dir.handle)
        .await
        .unwrap()
        .iter()
        .map(|f| f.filename.clone())
        .collect();
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "duplicate filler: {names:?}");
}

/// `clean` must never delete something the user put there.
#[tokio::test]
async fn clean_leaves_foreign_files_and_their_folder_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    engine::fill(&mut storage, window(), DIR, |_| {}).await.expect("fill");
    let dir = engine::find_fill_dir(&storage, DIR).await.unwrap().unwrap();

    // Things someone dropped into fill_disk. The last three are the ones that matter:
    // they sit inside our own naming space and differ only in the part `clean` has to
    // read correctly. An earlier filter tested prefix and suffix alone and would have
    // deleted all three.
    let foreign = [
        "mybook.azw3",
        "fill_notes.bin",
        "fill_12.bin",
        "fill_00000007.bin",
    ];
    for name in foreign {
        storage
            .upload(
                Some(dir.handle),
                NewObjectInfo::file(name, 1024),
                kindlefill_core::ZeroStream::new(1024),
            )
            .await
            .unwrap_or_else(|_| panic!("upload {name}"));
    }

    engine::clean(&mut storage, DIR, |_| {}).await.expect("clean");

    let dir = engine::find_fill_dir(&storage, DIR)
        .await
        .unwrap()
        .expect("folder must survive because it still holds files");
    let mut remaining: Vec<_> = storage
        .list_objects(Some(dir.handle))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.filename)
        .collect();
    remaining.sort();
    let mut expected: Vec<String> = foreign.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(remaining, expected, "clean deleted something that wasn't ours");
}

#[tokio::test]
async fn refuses_to_fill_a_device_already_below_the_window() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    // Consume nearly everything so free space sits under the window's lower bound.
    let bulk = CAPACITY - 4 * MIB;
    storage
        .upload(
            Some(ObjectHandle::ROOT),
            NewObjectInfo::file("bulk.bin", bulk),
            kindlefill_core::ZeroStream::new(bulk),
        )
        .await
        .expect("upload bulk");

    match engine::fill(&mut storage, window(), DIR, |_| {}).await {
        Err(engine::FillError::AlreadyBelowWindow { .. }) => {}
        other => panic!("expected AlreadyBelowWindow, got {other:?}"),
    }
    assert!(
        engine::find_fill_dir(&storage, DIR).await.unwrap().is_none(),
        "must not leave an empty fill_disk behind after refusing"
    );
}
