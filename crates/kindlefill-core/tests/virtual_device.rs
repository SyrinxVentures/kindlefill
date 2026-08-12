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
use mtp_rs::{
    MtpDevice, NewObjectInfo, ObjectHandle, Storage, VirtualDeviceConfig, VirtualStorageConfig,
};
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

    let outcome = engine::fill(&mut storage, w, DIR, |_| {})
        .await
        .expect("fill");

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
        engine::list_fillers(&storage, dir.handle)
            .await
            .unwrap()
            .len(),
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
    engine::fill(&mut storage, window(), DIR, |_| {})
        .await
        .expect("fill");
    assert!(free_space(&mut storage).await < before);

    engine::clean(&mut storage, DIR, |_| {})
        .await
        .expect("clean");

    assert_eq!(
        free_space(&mut storage).await,
        before,
        "should fully reclaim"
    );
    assert!(
        engine::find_fill_dir(&storage, DIR)
            .await
            .unwrap()
            .is_none(),
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

    engine::fill(&mut storage, w, DIR, |_| {})
        .await
        .expect("first fill");
    let dir = engine::find_fill_dir(&storage, DIR).await.unwrap().unwrap();
    let first_pass = engine::list_fillers(&storage, dir.handle).await.unwrap();

    // Simulate an interrupted run by removing the last object.
    let last = first_pass.last().expect("at least one filler");
    let names_before: Vec<_> = first_pass.iter().map(|f| f.filename.clone()).collect();
    storage.delete(last.handle).await.expect("delete");
    assert!(!w.contains(free_space(&mut storage).await));

    engine::fill(&mut storage, w, DIR, |_| {})
        .await
        .expect("second fill");

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
    assert_eq!(
        unique.len(),
        after.len(),
        "filenames must not collide: {after:?}"
    );
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
    assert_eq!(
        free_space(&mut storage).await,
        before,
        "must not have written"
    );
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
    assert!(
        mid < before,
        "should have made real progress before stopping"
    );
    assert!(!w.contains(mid), "should have stopped short of the target");

    // The decisive property: a plain re-run finishes the job.
    let outcome = engine::fill(&mut storage, w, DIR, |_| {})
        .await
        .expect("resume");
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

    engine::fill(&mut storage, window(), DIR, |_| {})
        .await
        .expect("fill");
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

    engine::clean(&mut storage, DIR, |_| {})
        .await
        .expect("clean");

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
    assert_eq!(
        remaining, expected,
        "clean deleted something that wasn't ours"
    );
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
        // Empty `elsewhere` is the load-bearing half: it is what distinguishes "the
        // space isn't ours" from "your filler is under another name", which the
        // sibling test below covers.
        Err(engine::FillError::AlreadyBelowWindow { elsewhere, .. }) => {
            assert!(
                elsewhere.is_empty(),
                "none of this space is ours: {elsewhere:?}"
            );
        }
        other => panic!("expected AlreadyBelowWindow, got {other:?}"),
    }
    assert!(
        engine::find_fill_dir(&storage, DIR)
            .await
            .unwrap()
            .is_none(),
        "must not leave an empty fill_disk behind after refusing"
    );
}

/// Refusing is right; refusing *silently* is not. Nothing remembers the folder name
/// between launches, so the default is exactly what a relaunched app asks about while
/// the filler sits under the name the user picked — and "nothing to fill" would leave
/// them believing the space was never ours to return.
#[tokio::test]
async fn refusing_below_the_window_says_where_the_filler_actually_is() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    engine::fill(&mut storage, window(), "somewhere_else", |_| {})
        .await
        .expect("fill");

    // A window above where the fill just left free space, asked about the *default*
    // folder — the shape of a relaunch that has forgotten the chosen name.
    let higher = Window::new(20 * MIB, 30 * MIB).unwrap();
    match engine::fill(&mut storage, higher, DIR, |_| {}).await {
        Err(e @ engine::FillError::AlreadyBelowWindow { .. }) => {
            let engine::FillError::AlreadyBelowWindow { ref elsewhere, .. } = e else {
                unreachable!()
            };
            assert_eq!(elsewhere.len(), 1, "{elsewhere:?}");
            assert_eq!(elsewhere[0].name, "somewhere_else");
            assert!(elsewhere[0].bytes > 0);
            // Both front ends render this error through `Display`, so the folder has
            // to survive the trip into the string — not just sit on the struct.
            let shown = e.to_string();
            assert!(
                shown.contains("somewhere_else"),
                "the message has to name the folder: {shown}"
            );
        }
        other => panic!("expected AlreadyBelowWindow, got {other:?}"),
    }
}

async fn put(storage: &Storage, parent: ObjectHandle, name: &str, size: u64) {
    storage
        .upload(
            Some(parent),
            NewObjectInfo::file(name, size),
            kindlefill_core::ZeroStream::new(size),
        )
        .await
        .unwrap_or_else(|_| panic!("upload {name}"));
}

async fn root_names(storage: &Storage) -> Vec<String> {
    let mut names: Vec<String> = storage
        .list_objects(Some(ObjectHandle::ROOT))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.filename)
        .collect();
    names.sort();
    names
}

/// A staged firmware image is the only device content this tool deletes that it did not
/// write, so the three gates on that deletion are the ones worth exercising against a
/// real filesystem rather than trusting to a doc comment.
#[tokio::test]
async fn deleting_staged_updates_removes_only_named_root_level_images() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    // Two real update images, plus three files that must survive: one that only looks
    // like an update, one book, and one genuine-shaped image nested inside a folder
    // rather than at the root.
    put(
        &storage,
        ObjectHandle::ROOT,
        "update_kindle_pw5_5.18.1.bin",
        4096,
    )
    .await;
    put(
        &storage,
        ObjectHandle::ROOT,
        "update_kindle_other.bin",
        4096,
    )
    .await;
    put(&storage, ObjectHandle::ROOT, "updates_notes.bin", 4096).await;
    put(&storage, ObjectHandle::ROOT, "mybook.azw3", 4096).await;
    let sub = storage
        .create_folder(Some(ObjectHandle::ROOT), "documents")
        .await
        .expect("create folder");
    put(&storage, sub, "update_kindle_nested.bin", 4096).await;

    let staged: Vec<String> = engine::list_staged_updates(&storage)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.filename)
        .collect();
    assert_eq!(
        staged,
        vec!["update_kindle_other.bin", "update_kindle_pw5_5.18.1.bin"],
        "only root-level update_*.bin should be reported"
    );

    // The name list is a request, not an instruction: two of these three are not
    // root-level staged images and must be ignored rather than deleted.
    let asked = vec![
        "update_kindle_pw5_5.18.1.bin".to_string(),
        "mybook.azw3".to_string(),
        "update_kindle_nested.bin".to_string(),
    ];
    let removed = engine::delete_staged_updates(&mut storage, &asked, |_| {})
        .await
        .expect("delete");

    assert_eq!(
        removed
            .iter()
            .map(|o| o.filename.as_str())
            .collect::<Vec<_>>(),
        vec!["update_kindle_pw5_5.18.1.bin"],
        "a name that isn't a root-level staged image must delete nothing"
    );
    assert_eq!(
        root_names(&storage).await,
        vec![
            "documents",
            "mybook.azw3",
            "update_kindle_other.bin",
            "updates_notes.bin",
        ]
    );
    // The nested lookalike is still where it was.
    assert_eq!(
        storage.list_objects(Some(sub)).await.unwrap().len(),
        1,
        "a folder's contents are out of reach entirely"
    );
}

/// `list_fillers` decides what `clean` deletes and `list_foreign` decides what the UI
/// warns about. They read the same folder through the same predicate, and this pins
/// them as exact complements — the original bug was two views of "is this ours"
/// drifting apart, and splitting the question across two functions reintroduces the
/// opportunity.
#[tokio::test]
async fn filler_and_foreign_partition_the_folder_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    engine::fill(&mut storage, window(), DIR, |_| {})
        .await
        .expect("fill");
    let dir = engine::find_fill_dir(&storage, DIR).await.unwrap().unwrap();
    for name in [
        "mybook.azw3",
        "fill_notes.bin",
        "fill_12.bin",
        "fill_00000007.bin",
    ] {
        put(&storage, dir.handle, name, 1024).await;
    }
    storage
        .create_folder(Some(dir.handle), "a_folder")
        .await
        .expect("create nested folder");

    let all = storage.list_objects(Some(dir.handle)).await.unwrap().len();
    let fillers = engine::list_fillers(&storage, dir.handle).await.unwrap();
    let foreign = engine::list_foreign(&storage, dir.handle).await.unwrap();

    assert_eq!(
        fillers.len() + foreign.len(),
        all,
        "must partition, not overlap or drop"
    );
    assert_eq!(foreign.len(), 5, "4 files plus the folder");
    for f in &fillers {
        assert!(f.is_file(), "a folder must never be counted as filler");
        assert!(
            !foreign.iter().any(|o| o.handle == f.handle),
            "sets must be disjoint"
        );
    }
}

#[test]
fn folder_names_that_would_escape_the_root_are_refused() {
    for ok in ["fill_disk", "my filler", "Fill-Disk_2", "a.b"] {
        assert!(
            engine::validate_dir_name(ok).is_ok(),
            "{ok} should be allowed"
        );
    }
    for bad in [
        "", "..", ".", "a/b", "a\\b", "a:b", " lead", "trail ", "a\u{0}b", "a*b",
    ] {
        assert!(
            engine::validate_dir_name(bad).is_err(),
            "{bad:?} must be refused"
        );
    }
}

/// Overwriting a folder is the one path that deletes content the tool didn't write, so
/// what it reaches — and what it doesn't — is worth pinning against a real filesystem.
#[tokio::test]
async fn overwriting_empties_the_folder_and_nothing_outside_it() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    // Something at the root that must survive: the purge is scoped to one named folder.
    put(&storage, ObjectHandle::ROOT, "root_book.azw3", 1024).await;

    engine::fill(&mut storage, window(), DIR, |_| {})
        .await
        .expect("fill");
    let dir = engine::find_fill_dir(&storage, DIR).await.unwrap().unwrap();
    put(&storage, dir.handle, "mybook.azw3", 1024).await;
    // Nested, so the depth-first walk is actually exercised — MTP won't delete a
    // folder that still has children.
    let nested = storage
        .create_folder(Some(dir.handle), "notes")
        .await
        .expect("create nested");
    put(&storage, nested, "deep.txt", 512).await;

    let before = free_space(&mut storage).await;
    let removed = engine::purge_fill_dir(&mut storage, DIR, |_| {})
        .await
        .expect("purge");

    assert!(
        removed >= 4,
        "should have removed filler, book, folder and its child"
    );
    let dir = engine::find_fill_dir(&storage, DIR)
        .await
        .unwrap()
        .expect("the folder itself stays — fill is about to use it");
    assert!(
        storage
            .list_objects(Some(dir.handle))
            .await
            .unwrap()
            .is_empty(),
        "folder must be empty afterwards"
    );
    assert!(
        free_space(&mut storage).await > before,
        "space must come back"
    );
    assert!(
        root_names(&storage)
            .await
            .contains(&"root_book.azw3".to_string()),
        "content outside the folder must be untouched"
    );
}

#[tokio::test]
async fn overwriting_a_folder_that_does_not_exist_is_a_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;
    put(&storage, ObjectHandle::ROOT, "root_book.azw3", 1024).await;

    assert_eq!(
        engine::purge_fill_dir(&mut storage, DIR, |_| {})
            .await
            .expect("purge"),
        0
    );
    assert_eq!(root_names(&storage).await, vec!["root_book.azw3"]);
}

/// The bug a real device found: nothing persists the folder name, so a relaunch looks
/// at the default and reports nothing while the filler sits under the name the user
/// chose. Discovery has to find it wherever it is — and must not mistake a folder of
/// books for one of ours.
#[tokio::test]
async fn filler_is_found_whatever_the_folder_is_called() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    // Filler under a name that isn't the default.
    engine::fill(&mut storage, window(), "fill_disk2112", |_| {})
        .await
        .expect("fill");

    // A decoy: a root folder full of things that are not ours.
    let books = storage
        .create_folder(Some(ObjectHandle::ROOT), "documents")
        .await
        .expect("create documents");
    put(&storage, books, "mybook.azw3", 4096).await;
    put(&storage, books, "fill_notes.bin", 4096).await;

    let found = engine::find_filler_folders(&storage).await.expect("scan");
    assert_eq!(
        found.len(),
        1,
        "should find exactly the filler folder: {found:?}"
    );
    assert_eq!(found[0].name, "fill_disk2112");
    assert!(found[0].files > 0 && found[0].bytes > 0);

    // And the default name genuinely holds nothing, which is what made this invisible.
    assert!(engine::find_fill_dir(&storage, DIR)
        .await
        .unwrap()
        .is_none());
}

/// `clean` aimed at the wrong folder must not claim success. Reporting only free
/// space cannot distinguish "removed everything" from "there was nothing here".
#[tokio::test]
async fn cleaning_the_wrong_folder_reports_that_it_removed_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    engine::fill(&mut storage, window(), "somewhere_else", |_| {})
        .await
        .expect("fill");
    let before = free_space(&mut storage).await;

    let report = engine::clean(&mut storage, DIR, |_| {})
        .await
        .expect("clean");
    assert_eq!(report.removed, 0, "nothing in {DIR} to remove");
    assert_eq!(report.bytes, 0);
    assert_eq!(
        free_space(&mut storage).await,
        before,
        "must not have freed anything"
    );

    // The filler is still where it was, and still findable.
    let found = engine::find_filler_folders(&storage).await.unwrap();
    assert_eq!(found[0].name, "somewhere_else");

    // Aimed correctly, it reports what it did.
    let report = engine::clean(&mut storage, "somewhere_else", |_| {})
        .await
        .expect("clean");
    assert!(report.removed > 0 && report.bytes > 0, "{report:?}");
    assert!(free_space(&mut storage).await > before);
}

/// The gap a real device exposed: filled to 87 MB free, wanting 80, and the smallest
/// filler object is far wider than the window. Writing can only reduce free space, so
/// before this the only remedy was deleting everything and refilling — seventeen
/// minutes to gain a few megabytes. Fill now converges from below too.
#[tokio::test]
async fn fill_climbs_back_up_when_it_starts_below_the_window() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    // Land somewhere first, then ask for a window well *above* where we ended up.
    engine::fill(&mut storage, window(), DIR, |_| {})
        .await
        .expect("fill");
    let low = free_space(&mut storage).await;
    assert!(window().contains(low));

    let higher = Window::new(low + 8 * MIB, low + 16 * MIB).unwrap();
    let outcome = engine::fill(&mut storage, higher, DIR, |_| {})
        .await
        .expect("second fill");

    match outcome {
        engine::Outcome::InWindow { free } => {
            assert!(
                higher.contains(free),
                "landed at {free}, outside {higher:?}"
            )
        }
        other => panic!("expected InWindow, got {other:?}"),
    }
    // And the device agrees, independently of what fill reported.
    assert!(higher.contains(free_space(&mut storage).await));
}

/// Climbing up must delete filler and nothing else.
#[tokio::test]
async fn climbing_up_removes_only_filler_and_leaves_foreign_files_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    engine::fill(&mut storage, window(), DIR, |_| {})
        .await
        .expect("fill");
    let dir = engine::find_fill_dir(&storage, DIR).await.unwrap().unwrap();
    put(&storage, dir.handle, "mybook.azw3", 1024).await;
    put(&storage, dir.handle, "fill_notes.bin", 1024).await;
    put(&storage, ObjectHandle::ROOT, "root_book.azw3", 1024).await;

    let low = free_space(&mut storage).await;
    let higher = Window::new(low + 8 * MIB, low + 16 * MIB).unwrap();
    engine::fill(&mut storage, higher, DIR, |_| {})
        .await
        .expect("climb");

    let survivors: Vec<String> = engine::list_foreign(&storage, dir.handle)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.filename)
        .collect();
    assert!(
        survivors.contains(&"mybook.azw3".to_string()),
        "{survivors:?}"
    );
    assert!(
        survivors.contains(&"fill_notes.bin".to_string()),
        "{survivors:?}"
    );
    assert!(root_names(&storage)
        .await
        .contains(&"root_book.azw3".to_string()));
}

/// Below the window with no filler of ours is genuinely unfixable, and must still say
/// so rather than silently doing nothing.
#[tokio::test]
async fn below_the_window_with_nothing_of_ours_to_remove_is_still_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let device = open(tmp.path()).await;
    let mut storage = storage_of(&device).await;

    let bulk = CAPACITY - 4 * MIB;
    put(&storage, ObjectHandle::ROOT, "bulk.bin", bulk).await;

    match engine::fill(&mut storage, window(), DIR, |_| {}).await {
        Err(engine::FillError::AlreadyBelowWindow { .. }) => {}
        other => panic!("expected AlreadyBelowWindow, got {other:?}"),
    }
}
