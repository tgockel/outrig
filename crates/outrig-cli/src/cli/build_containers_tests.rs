//! Fixture-driven tests for the build-container sweep. No engine involved:
//! the join and the classifier are pure, which is the whole reason they are
//! separate from the two listings that feed them.

use std::time::{Duration, SystemTime};

use super::{BuildContainer, classify_build_containers, join_build_containers};

const WORKING: &str = r#"[
  {
    "id": "213c716fa504aa1f0f4c1b1b6f0f0b7b2f9a1c0d0e0f0a0b0c0d0e0f0a0b0c0d",
    "builder": true,
    "imageid": "69e8c386a4ef",
    "imagename": "localhost/outrig-cache:d8aa33",
    "containername": "outrig-cache-working-container"
  }
]"#;

const EXTERNAL: &str = r#"[
  {
    "Id": "213c716fa504aa1f0f4c1b1b6f0f0b7b2f9a1c0d0e0f0a0b0c0d0e0f0a0b0c0d",
    "Created": 1700000000,
    "Names": ["outrig-cache-working-container"]
  }
]"#;

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn an_empty_store_prints_null_and_joins_to_nothing() {
    let joined = join_build_containers(b"null", b"[]").expect("join");
    assert!(joined.is_empty());
}

#[test]
fn the_join_carries_the_id_from_buildah_and_the_time_from_podman() {
    let joined =
        join_build_containers(WORKING.as_bytes(), EXTERNAL.as_bytes()).expect("join");
    assert_eq!(joined.len(), 1);
    assert_eq!(joined[0].name, "outrig-cache-working-container");
    assert_eq!(joined[0].image, "localhost/outrig-cache:d8aa33");
    assert_eq!(joined[0].short_id(), "213c716fa504");
    assert_eq!(joined[0].created, Some(at(1_700_000_000)));
}

/// podman reports `Created` as an RFC 3339 string on some versions and as
/// epoch seconds on others, and a sweep that understood only one would
/// silently collect nothing on the other.
#[test]
fn an_rfc3339_creation_time_is_understood_too() {
    let external = EXTERNAL.replace("1700000000", "\"2023-11-14T22:13:20Z\"");
    let joined = join_build_containers(WORKING.as_bytes(), external.as_bytes()).expect("join");
    assert_eq!(joined[0].created, Some(at(1_700_000_000)));
}

/// buildah decides what is a working container; podman's listing only
/// contributes times. A row only podman knows about is not swept.
#[test]
fn a_row_only_podman_knows_about_is_ignored() {
    let joined = join_build_containers(b"null", EXTERNAL.as_bytes()).expect("join");
    assert!(joined.is_empty());
}

#[test]
fn a_container_with_no_external_row_has_no_creation_time() {
    let joined = join_build_containers(WORKING.as_bytes(), b"[]").expect("join");
    assert_eq!(joined[0].created, None);
}

#[test]
fn only_containers_past_the_cutoff_are_removable() {
    let now = at(1_000_000);
    let containers = vec![
        BuildContainer {
            id: "old".into(),
            name: "a".into(),
            image: "i".into(),
            created: Some(at(1_000_000 - 3600)),
        },
        BuildContainer {
            id: "young".into(),
            name: "b".into(),
            image: "i".into(),
            created: Some(at(1_000_000 - 5)),
        },
    ];
    let (removable, undatable) =
        classify_build_containers(containers, Duration::from_secs(60), now);
    assert_eq!(
        removable.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["old"]
    );
    assert!(undatable.is_empty());
}

/// The rule that keeps this sweep from reaching a build that is running right
/// now: an age it cannot establish is not an age past the cutoff.
#[test]
fn a_container_with_no_creation_time_is_reported_not_removed() {
    let containers = vec![BuildContainer {
        id: "unknown".into(),
        name: "a".into(),
        image: "i".into(),
        created: None,
    }];
    let (removable, undatable) =
        classify_build_containers(containers, Duration::from_secs(60), at(1_000_000));
    assert!(removable.is_empty());
    assert_eq!(
        undatable.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["unknown"]
    );
}

/// podman serializes an unset creation time as Go's zero `time.Time`. Clamping
/// that to the epoch would read "no idea when this was made" as "made in
/// 1970", which clears every cutoff -- turning the one container this sweep
/// must not touch into its most eligible target.
#[test]
fn gos_zero_time_is_an_unknown_age_not_an_ancient_one() {
    for unset in ["-62135596800", "\"0001-01-01T00:00:00Z\""] {
        let external = EXTERNAL.replace("1700000000", unset);
        let joined = join_build_containers(WORKING.as_bytes(), external.as_bytes())
            .unwrap_or_else(|e| panic!("join with {unset}: {e}"));
        assert_eq!(joined[0].created, None, "for {unset}");

        let (removable, undatable) = classify_build_containers(
            joined,
            Duration::from_secs(60),
            at(1_700_000_000),
        );
        assert!(
            removable.is_empty(),
            "an undated container cleared the cutoff for {unset}"
        );
        assert_eq!(undatable.len(), 1, "it should be reported instead");
    }
}
