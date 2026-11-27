use super::support::*;
use crate::application::{
    SavedSessionSummary, SessionCandidate, SessionListing, SessionSelectorError,
    resolve_session_selector_from_listings,
};
use crate::session::ConversationId;
use crate::store::StoreError;

fn listing_ok(id: &str, title: Option<&str>, model: &str, updated_at_ms: u64) -> SessionListing {
    let mut def = definition();
    def.model = model.to_owned();
    SessionListing {
        conversation_id: ConversationId::from(id),
        record: Ok(SavedSessionSummary {
            definition: def,
            revision: 1,
            block_count: 1,
            title: title.map(str::to_owned),
            created_at_ms: None,
            updated_at_ms,
        }),
        active: Ok(false),
        local: None,
    }
}

fn listing_corrupt(id: &str) -> SessionListing {
    SessionListing {
        conversation_id: ConversationId::from(id),
        record: Err(Arc::new(StoreError::InvalidDatabase("corrupt row".into()))),
        active: Ok(false),
        local: None,
    }
}

#[test]
fn pure_selector_rejects_empty_and_whitespace() {
    let listings = vec![listing_ok(
        "01936b12-0000-7000-8000-000000000001",
        None,
        "m",
        10,
    )];
    for raw in ["", " ", "\t\n  "] {
        let err = resolve_session_selector_from_listings(raw, &listings).unwrap_err();
        assert!(matches!(err, SessionSelectorError::EmptySelector));
        assert_eq!(err.to_string(), "session selector must not be empty");
    }
}

#[test]
fn pure_selector_prefers_exact_match_over_latest_and_prefixes() {
    let listings = vec![
        listing_ok("abcd", Some("exact prefix"), "m1", 10),
        listing_ok("abcdef12", Some("longer"), "m1", 500),
        listing_ok("latest", Some("literal latest"), "m1", 20),
        listing_ok("zzzzzzzz", Some("newest"), "m1", 999),
    ];

    assert_eq!(
        resolve_session_selector_from_listings("abcd", &listings).unwrap(),
        ConversationId::from("abcd")
    );
    assert_eq!(
        resolve_session_selector_from_listings("latest", &listings).unwrap(),
        ConversationId::from("latest")
    );
}

#[test]
fn pure_selector_latest_picks_newest_valid_and_tie_breaks_by_descending_id() {
    let empty: Vec<SessionListing> = Vec::new();
    let empty_err = resolve_session_selector_from_listings("latest", &empty).unwrap_err();
    assert!(matches!(
        empty_err,
        SessionSelectorError::NotFound { ref selector } if selector == "latest"
    ));

    let only_corrupt = vec![listing_corrupt("01936b12-corrupt")];
    let corrupt_err = resolve_session_selector_from_listings("latest", &only_corrupt).unwrap_err();
    assert!(matches!(
        corrupt_err,
        SessionSelectorError::NotFound { ref selector } if selector == "latest"
    ));

    let listings = vec![
        listing_corrupt("ffffffff-corrupt"),
        listing_ok("aaaa1111", None, "m1", 100),
        listing_ok("aaaa2222", None, "m1", 200),
        listing_ok("bbbb1111", None, "m1", 200),
    ];
    // Equal updated_at_ms=200 tie-breaks by descending conversation_id ("bbbb1111" > "aaaa2222").
    assert_eq!(
        resolve_session_selector_from_listings("latest", &listings).unwrap(),
        ConversationId::from("bbbb1111")
    );

    // Uppercase "LATEST" is NOT treated as the "latest" alias.
    let upper_err = resolve_session_selector_from_listings("LATEST", &listings).unwrap_err();
    assert!(matches!(
        upper_err,
        SessionSelectorError::NotFound { ref selector } if selector == "LATEST"
    ));
}

#[test]
fn pure_selector_prefix_is_case_insensitive_and_trims_whitespace() {
    let listings = vec![
        listing_ok("01936B12-aaaa-7000-8000-000000000001", None, "m1", 100),
        listing_ok("01936c99-bbbb-7000-8000-000000000002", None, "m1", 200),
    ];

    let resolved = resolve_session_selector_from_listings("  01936b12  ", &listings).unwrap();
    assert_eq!(
        resolved,
        ConversationId::from("01936B12-aaaa-7000-8000-000000000001")
    );
}

#[test]
fn pure_selector_ambiguous_caps_at_five_candidates_and_formats_clearly() {
    let listings = vec![
        listing_ok(
            "ab120001-0000-0000-0000-000000000001",
            Some("first"),
            "m-old",
            10,
        ),
        listing_ok("ab120002-0000-0000-0000-000000000002", None, "m-2", 20),
        listing_ok(
            "ab120003-0000-0000-0000-000000000003",
            Some("three\nline"),
            "m-3",
            30,
        ),
        listing_ok(
            "ab120004-0000-0000-0000-000000000004",
            Some("four"),
            "m-4",
            50,
        ),
        listing_ok(
            "ab120005-0000-0000-0000-000000000005",
            Some("five"),
            "m-5",
            50,
        ),
        listing_ok(
            "ab120006-0000-0000-0000-000000000006",
            Some("six"),
            "m-6",
            60,
        ),
        listing_ok(
            "ab120007-0000-0000-0000-000000000007",
            Some("seven"),
            "m-7",
            70,
        ),
    ];

    let err = resolve_session_selector_from_listings("AB12", &listings).unwrap_err();
    match &err {
        SessionSelectorError::Ambiguous {
            selector,
            total_matches,
            candidates,
        } => {
            assert_eq!(selector, "AB12");
            assert_eq!(*total_matches, 7);
            assert_eq!(candidates.len(), 5);
            // Sorted by Reverse(updated_at_ms), then conversation_id ascending on tie (50 -> 0004 before 0005).
            assert_eq!(
                candidates,
                &vec![
                    SessionCandidate {
                        conversation_id: ConversationId::from(
                            "ab120007-0000-0000-0000-000000000007"
                        ),
                        short_id: "ab120007".into(),
                        title: Some("seven".into()),
                        model: "m-7".into(),
                        updated_at_ms: 70,
                    },
                    SessionCandidate {
                        conversation_id: ConversationId::from(
                            "ab120006-0000-0000-0000-000000000006"
                        ),
                        short_id: "ab120006".into(),
                        title: Some("six".into()),
                        model: "m-6".into(),
                        updated_at_ms: 60,
                    },
                    SessionCandidate {
                        conversation_id: ConversationId::from(
                            "ab120004-0000-0000-0000-000000000004"
                        ),
                        short_id: "ab120004".into(),
                        title: Some("four".into()),
                        model: "m-4".into(),
                        updated_at_ms: 50,
                    },
                    SessionCandidate {
                        conversation_id: ConversationId::from(
                            "ab120005-0000-0000-0000-000000000005"
                        ),
                        short_id: "ab120005".into(),
                        title: Some("five".into()),
                        model: "m-5".into(),
                        updated_at_ms: 50,
                    },
                    SessionCandidate {
                        conversation_id: ConversationId::from(
                            "ab120003-0000-0000-0000-000000000003"
                        ),
                        short_id: "ab120003".into(),
                        title: Some("three\nline".into()),
                        model: "m-3".into(),
                        updated_at_ms: 30,
                    },
                ]
            );
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }

    let msg = err.to_string();
    assert!(msg.contains("7 matches"));
    assert!(msg.contains("ab120007 (m-7) \"seven\""));
    assert!(msg.contains("three\\nline"));
    assert!(msg.contains("... and 2 more (total 7 matches)"));
}

#[test]
fn pure_selector_matches_corrupt_by_exact_or_prefix_and_ranks_valid_first() {
    let listings = vec![
        listing_corrupt("dead0001-0000-0000-0000-000000000001"),
        listing_ok(
            "dead0002-0000-0000-0000-000000000002",
            Some("ok-zero"),
            "m-ok",
            0,
        ),
        listing_corrupt("cafe0001-0000-0000-0000-000000000001"),
    ];

    // Corrupt listing is still resolvable by exact ID or unique prefix (for repair/delete).
    assert_eq!(
        resolve_session_selector_from_listings("cafe0001-0000-0000-0000-000000000001", &listings)
            .unwrap(),
        ConversationId::from("cafe0001-0000-0000-0000-000000000001")
    );
    assert_eq!(
        resolve_session_selector_from_listings("CAFE", &listings).unwrap(),
        ConversationId::from("cafe0001-0000-0000-0000-000000000001")
    );

    // Ambiguous prefix mixing valid (even updated_at_ms=0) and corrupt sorts valid first.
    let err = resolve_session_selector_from_listings("dead", &listings).unwrap_err();
    match &err {
        SessionSelectorError::Ambiguous {
            total_matches,
            candidates,
            ..
        } => {
            assert_eq!(*total_matches, 2);
            assert_eq!(candidates.len(), 2);
            assert_eq!(
                candidates[0].conversation_id,
                ConversationId::from("dead0002-0000-0000-0000-000000000002")
            );
            assert_eq!(candidates[0].model, "m-ok");
            assert_eq!(
                candidates[1].conversation_id,
                ConversationId::from("dead0001-0000-0000-0000-000000000001")
            );
            assert_eq!(candidates[1].model, "(corrupt)");
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    assert!(err.to_string().contains("dead0001 ((corrupt))"));

    // NotFound escapes control characters and newlines consistently with Ambiguous.
    let not_found = resolve_session_selector_from_listings("bad\n\x1b", &listings).unwrap_err();
    assert_eq!(not_found.to_string(), "session not found: bad\\n\\x1b");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_resolve_session_selector_against_sqlite_store() {
    // Transient host without store: empty selector still returns EmptySelector first;
    // non-empty selector returns StoreUnavailable.
    let no_store_host = SessionHost::new(None);
    assert!(matches!(
        no_store_host.resolve_session_selector("   ").await,
        Err(SessionSelectorError::EmptySelector)
    ));
    assert!(matches!(
        no_store_host.resolve_session_selector("latest").await,
        Err(SessionSelectorError::StoreUnavailable)
    ));

    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().into()));
    normal_fixture(&host);

    // Empty store -> latest returns NotFound.
    assert!(matches!(
        host.resolve_session_selector("latest").await,
        Err(SessionSelectorError::NotFound { ref selector }) if selector == "latest"
    ));

    let s1 = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id1 = s1.target().conversation_id().clone();
    host.close(&s1.target(), ClosePolicy::Save).await.unwrap();

    tokio::time::sleep(Duration::from_millis(5)).await;

    let s2 = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id2 = s2.target().conversation_id().clone();
    s2.rename(Some("Second Session".into())).unwrap();
    s2.wait_saved().await.unwrap();
    host.close(&s2.target(), ClosePolicy::Save).await.unwrap();

    // "latest" resolves to s2 (more recently updated).
    assert_eq!(host.resolve_session_selector("latest").await.unwrap(), id2);

    // Exact match resolves to id1.
    assert_eq!(
        host.resolve_session_selector(id1.as_str()).await.unwrap(),
        id1
    );

    // Find a unique prefix for id1 vs id2 (UUIDv7 created >= 5ms apart differ within first 12 hex chars).
    let mut unique_len = 1;
    while id1.as_str()[..unique_len] == id2.as_str()[..unique_len] {
        unique_len += 1;
    }
    let unique_prefix = id1.as_str()[..unique_len].to_ascii_uppercase();
    assert_eq!(
        host.resolve_session_selector(&unique_prefix).await.unwrap(),
        id1
    );

    // Shared prefix ("019" or first char) -> Ambiguous when both share it.
    if id1.as_str().as_bytes()[0] == id2.as_str().as_bytes()[0] {
        let shared = &id1.as_str()[..1];
        let err = host.resolve_session_selector(shared).await.unwrap_err();
        assert!(matches!(
            err,
            SessionSelectorError::Ambiguous {
                total_matches: 2,
                ..
            }
        ));
    }

    // Non-matching prefix -> NotFound.
    assert!(matches!(
        host.resolve_session_selector("zzzz-no-match").await,
        Err(SessionSelectorError::NotFound { ref selector }) if selector == "zzzz-no-match"
    ));
}
