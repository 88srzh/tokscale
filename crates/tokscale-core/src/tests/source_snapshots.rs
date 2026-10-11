use super::*;
use crate::{bucket_tz::BucketTimezone, SourceUsage};
use std::time::Instant;

fn message(client: &str, session: &str, date: &str, model: &str, tokens: i64) -> UnifiedMessage {
    let mut message = UnifiedMessage::new(
        client,
        model,
        "provider",
        session,
        0,
        TokenBreakdown {
            input: tokens,
            ..Default::default()
        },
        tokens as f64 / 100.0,
    );
    message.date = date.into();
    message
}

fn snapshot_graph(options: &ReportOptions, messages: Vec<UnifiedMessage>) -> crate::GraphResult {
    let mut sink = GraphSink::new(
        Some(options),
        None,
        GraphPricingRequirement::LenientSourceSnapshots,
    );
    sink.enable_source_snapshots(SourceUsage {
        version: 1,
        event_identity_version: 1,
        captured_at: "2026-03-03T00:00:00Z".into(),
        full_history: options.since.is_none() && options.until.is_none() && options.year.is_none(),
        replacement_clients: crate::source_replacement_clients(options.clients.as_deref()),
        partial_clients: Vec::new(),
        bucket_timezone: "Etc/UTC".into(),
        day_key: "2026-03-03".into(),
        sessions: Vec::new(),
        rows_without_identity: Default::default(),
    });
    for message in messages {
        sink.accept(message);
    }
    sink.finish(
        Instant::now(),
        &BucketTimezone::from_pinned_name(Some("Etc/UTC")),
    )
    .unwrap()
}

#[test]
fn source_snapshots_fold_every_day_model_and_client_partition() {
    let graph = snapshot_graph(
        &ReportOptions::default(),
        vec![
            message("opencode", "shared", "2026-03-02", "model-a", 10),
            message("opencode", "shared", "2026-03-01", "model-a", 20),
            message("opencode", "shared", "2026-03-01", "model-b", 30),
            message("codex", "shared", "2026-03-01", "model-a", 40),
            message("opencode", "other", "2026-03-01", "model-a", 5),
        ],
    );
    let source = graph.source_usage.unwrap();
    assert_eq!(source.sessions.len(), 3);
    let session = source
        .sessions
        .iter()
        .find(|s| s.client == "opencode" && s.session_id == "shared")
        .unwrap();
    assert_eq!(
        session
            .days
            .iter()
            .map(|d| d.date.as_str())
            .collect::<Vec<_>>(),
        ["2026-03-01", "2026-03-02"]
    );
    assert_eq!(session.days[0].totals.tokens, 50);
    assert_eq!(session.days[0].totals.messages, 2);
    assert_eq!(session.days[0].clients.len(), 2);
    assert_eq!(session.days[0].totals.cost, 0.5);
    assert_eq!(
        source
            .sessions
            .iter()
            .flat_map(|s| &s.days)
            .map(|d| d.totals.tokens)
            .sum::<i64>(),
        graph.summary.total_tokens
    );
    assert_eq!(graph.summary.total_tokens, 105);
}

#[test]
fn source_snapshots_corrections_replace_whole_session_days() {
    let old = snapshot_graph(
        &ReportOptions::default(),
        vec![
            message("opencode", "same", "2026-03-01", "old-model", 100),
            message("opencode", "same", "2026-03-02", "old-model", 50),
        ],
    )
    .source_usage
    .unwrap();
    let corrected = snapshot_graph(
        &ReportOptions::default(),
        vec![message("opencode", "same", "2026-03-02", "new-model", 7)],
    )
    .source_usage
    .unwrap();
    // A downstream map replaces the entire (client, session) snapshot; merging
    // day/model maxima here would resurrect both the old day and old model.
    let mut archive: HashMap<_, _> = old
        .sessions
        .into_iter()
        .map(|s| ((s.client.clone(), s.session_id.clone()), s.days))
        .collect();
    for session in corrected.sessions {
        archive.insert((session.client, session.session_id), session.days);
    }
    let days = &archive[&("opencode".into(), "same".into())];
    assert_eq!(days.len(), 1);
    assert_eq!(days[0].date, "2026-03-02");
    assert_eq!(days[0].totals.tokens, 7);
    assert_eq!(days[0].clients[0].model_id, "new-model");
}

#[test]
fn source_snapshots_keep_out_of_window_ids_and_zero_usage_but_diagnose_identity_gaps() {
    let options = ReportOptions {
        since: Some("2026-03-02".into()),
        ..Default::default()
    };
    let graph = snapshot_graph(
        &options,
        vec![
            message("opencode", "outside", "2026-03-01", "model", 9),
            message("opencode", "zero", "2026-03-02", "", 0),
            message("opencode", "", "2026-03-02", "model", 3),
            message("opencode", "unknown", "2026-03-02", "model", 4),
            message("codex", "  ", "2026-03-01", "model", 5),
        ],
    );
    let source = graph.source_usage.unwrap();
    assert!(!source.full_history);
    assert_eq!(source.sessions.len(), 2);
    assert!(source.sessions[0].days.is_empty());
    assert_eq!(source.sessions[0].session_id, "outside");
    let zero = &source.sessions[1].days[0];
    assert_eq!(zero.totals.tokens, 0);
    assert_eq!(zero.totals.messages, 1);
    assert_eq!(zero.clients[0].model_id, "");
    assert_eq!(source.rows_without_identity["opencode"], 2);
    assert_eq!(source.rows_without_identity["codex"], 1);
    assert_eq!(graph.summary.total_tokens, 7);
    assert_eq!(graph.contributions[0].totals.messages, 3);
}

#[test]
fn source_snapshots_cross_pricing_batches_and_are_absent_without_opt_in() {
    let messages = (0..GRAPH_SINK_BATCH + 1)
        .map(|_| message("opencode", "same", "2026-03-01", "model", 1))
        .collect();
    let graph = snapshot_graph(&ReportOptions::default(), messages);
    assert_eq!(
        graph.source_usage.unwrap().sessions[0].days[0]
            .totals
            .tokens,
        (GRAPH_SINK_BATCH + 1) as i64
    );
    for requirement in [
        GraphPricingRequirement::Lenient,
        GraphPricingRequirement::LenientSourceOnly,
        GraphPricingRequirement::Submission,
    ] {
        let mut row = message("opencode", "same", "2026-03-01", "", 0);
        row.mark_provider_reported_cost();
        let pricing = pricing::PricingService::new(
            HashMap::from([(
                "fixture".into(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(0.0),
                    ..Default::default()
                },
            )]),
            HashMap::new(),
        );
        let graph = build_graph_from_messages(
            vec![row],
            Some(&pricing),
            requirement,
            Instant::now(),
            &BucketTimezone::Local,
        )
        .unwrap();
        assert!(graph.source_usage.is_none());
        assert!(serde_json::to_value(graph)
            .unwrap()
            .get("sourceUsage")
            .is_none());
    }
}

fn keyed_message(
    client: &str,
    session: &str,
    date: &str,
    model: &str,
    tokens: i64,
    key: &str,
) -> UnifiedMessage {
    let mut row = message(client, session, date, model, tokens);
    row.dedup_key = Some(key.into());
    row
}

#[test]
fn source_snapshots_event_groups_preserve_all_daily_accounting_and_unkeyed_residual() {
    let mut detailed = keyed_message("opencode", "same", "2026-03-01", "a", 7, "event-a");
    detailed.tokens.output = 3;
    detailed.tokens.cache_read = 5;
    detailed.tokens.cache_write = 11;
    detailed.tokens.cache_write_1h = 2;
    detailed.tokens.reasoning = 13;
    detailed.message_count = 2;
    let graph = snapshot_graph(
        &ReportOptions::default(),
        vec![
            detailed,
            keyed_message("opencode", "same", "2026-03-02", "b", 2, "event-a"),
            keyed_message("opencode", "same", "2026-03-01", "c", 4, "event-b"),
            message("opencode", "same", "2026-03-02", "", 0),
            keyed_message("opencode", "same", "2026-03-02", "b", 6, "  "),
        ],
    );
    let source = graph.source_usage.unwrap();
    assert_eq!(source.event_identity_version, 1);
    let session = &source.sessions[0];
    assert_eq!(session.events.len(), 3);
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.id.is_none())
            .count(),
        1
    );
    for day in &session.days {
        let event_days: Vec<_> = session
            .events
            .iter()
            .flat_map(|event| &event.days)
            .filter(|event_day| event_day.date == day.date)
            .collect();
        assert_eq!(
            event_days.iter().map(|day| day.totals.tokens).sum::<i64>(),
            day.totals.tokens
        );
        assert_eq!(
            event_days
                .iter()
                .map(|day| day.totals.messages)
                .sum::<i32>(),
            day.totals.messages
        );
        assert!(
            (event_days.iter().map(|day| day.totals.cost).sum::<f64>() - day.totals.cost).abs()
                < 1e-12
        );
        let mut tokens = TokenBreakdown::default();
        for event_day in &event_days {
            tokens.input += event_day.token_breakdown.input;
            tokens.output += event_day.token_breakdown.output;
            tokens.cache_read += event_day.token_breakdown.cache_read;
            tokens.cache_write += event_day.token_breakdown.cache_write;
            tokens.cache_write_1h += event_day.token_breakdown.cache_write_1h;
            tokens.reasoning += event_day.token_breakdown.reasoning;
        }
        assert_eq!(tokens, day.token_breakdown);
        for model in &day.clients {
            let groups: Vec<_> = event_days
                .iter()
                .flat_map(|day| &day.clients)
                .filter(|group| {
                    group.client == model.client
                        && group.model_id == model.model_id
                        && group.provider_id == model.provider_id
                })
                .collect();
            assert_eq!(
                groups.iter().map(|group| group.tokens.total()).sum::<i64>(),
                model.tokens.total()
            );
            assert_eq!(
                groups.iter().map(|group| group.messages).sum::<i32>(),
                model.messages
            );
            assert!(
                (groups.iter().map(|group| group.cost).sum::<f64>() - model.cost).abs() < 1e-12
            );
        }
    }
    let serialized = serde_json::to_string(&source).unwrap();
    assert!(!serialized.contains("event-a"));
    assert!(!serialized.contains("event-b"));
    for event in session.events.iter().filter(|event| event.id.is_some()) {
        let id = event.id.as_deref().unwrap();
        assert_eq!(id.len(), 71);
        assert!(id.starts_with("sha256:"));
        assert!(id[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
    }
}

#[test]
fn source_snapshots_event_claims_follow_winner_migration_and_keep_partial_overlap() {
    let old = snapshot_graph(
        &ReportOptions::default(),
        vec![
            keyed_message("opencode", "old", "2026-03-01", "old-model", 100, "shared"),
            keyed_message(
                "opencode",
                "old",
                "2026-03-02",
                "retained-model",
                50,
                "orphan",
            ),
        ],
    )
    .source_usage
    .unwrap();
    let current = snapshot_graph(
        &ReportOptions::default(),
        vec![keyed_message(
            "opencode",
            "new",
            "2026-03-03",
            "new-model",
            7,
            "shared",
        )],
    )
    .source_usage
    .unwrap();
    assert_eq!(old.sessions[0].events.len(), 2);
    let claimed: HashSet<_> = current
        .sessions
        .iter()
        .flat_map(|session| &session.events)
        .filter_map(|event| event.id.as_ref())
        .collect();
    assert!(claimed.contains(
        &"sha256:2283d654a3e5d8ef486897c248d1a6390f02c830a5ff500637ce48feeaacaec2".to_string()
    ));
    let retained: Vec<_> = old.sessions[0]
        .events
        .iter()
        .filter(|event| event.id.as_ref().is_some_and(|id| !claimed.contains(id)))
        .collect();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].days[0].clients[0].model_id, "retained-model");
    let union_tokens: i64 = retained
        .iter()
        .flat_map(|event| &event.days)
        .chain(current.sessions.iter().flat_map(|session| &session.days))
        .map(|day| day.totals.tokens)
        .sum();
    assert_eq!(union_tokens, 57);

    let old_native = snapshot_graph(
        &ReportOptions::default(),
        vec![keyed_message(
            "antigravity",
            "legacy-session",
            "2026-03-01",
            "model",
            9,
            "response",
        )],
    )
    .source_usage
    .unwrap();
    for client in ["antigravity-cli", "antigravity-extension"] {
        let new_native = snapshot_graph(
            &ReportOptions::default(),
            vec![keyed_message(
                client,
                "native-session",
                "2026-03-02",
                "corrected",
                3,
                "response",
            )],
        )
        .source_usage
        .unwrap();
        assert_eq!(
            old_native.sessions[0].events[0].id,
            new_native.sessions[0].events[0].id
        );
    }
    let independent = snapshot_graph(
        &ReportOptions::default(),
        vec![keyed_message(
            "opencode",
            "other",
            "2026-03-01",
            "model",
            9,
            "response",
        )],
    )
    .source_usage
    .unwrap();
    assert_ne!(
        old_native.sessions[0].events[0].id,
        independent.sessions[0].events[0].id
    );
}

#[test]
fn source_snapshots_event_groups_cross_batch_boundaries() {
    let graph = snapshot_graph(
        &ReportOptions::default(),
        (0..GRAPH_SINK_BATCH + 3)
            .map(|index| {
                keyed_message(
                    "opencode",
                    "same",
                    "2026-03-01",
                    if index % 2 == 0 { "a" } else { "b" },
                    1,
                    "same-event",
                )
            })
            .collect(),
    );
    let session = &graph.source_usage.as_ref().unwrap().sessions[0];
    assert_eq!(session.events.len(), 1);
    assert_eq!(session.events[0].days[0].totals, session.days[0].totals);
    for model in &session.days[0].clients {
        assert_eq!(
            session.events[0].days[0]
                .clients
                .iter()
                .find(|candidate| candidate.model_id == model.model_id),
            Some(model)
        );
    }
}

#[test]
fn source_snapshots_synthetic_only_clients_are_partial_before_date_filtering() {
    for (selected, expected) in [
        (vec!["synthetic".into()], Vec::<String>::new()),
        (
            vec!["opencode".into(), "synthetic".into()],
            vec!["opencode".into()],
        ),
    ] {
        let options = ReportOptions {
            clients: Some(selected),
            since: Some("2030-01-01".into()),
            ..Default::default()
        };
        let source = snapshot_graph(
            &options,
            vec![
                keyed_message("opencode", "a", "2026-03-01", "model", 3, "a"),
                keyed_message("codex", "b", "2026-03-01", "model", 4, "b"),
            ],
        )
        .source_usage
        .unwrap();
        assert_eq!(source.replacement_clients, expected);
        assert!(source.partial_clients.contains(&"codex".into()));
        assert_eq!(
            source.partial_clients.contains(&"opencode".into()),
            source.replacement_clients.is_empty()
        );
        assert!(source
            .sessions
            .iter()
            .all(|session| session.days.is_empty()));
    }
    let options = ReportOptions {
        clients: Some(vec!["claude".into(), "gjc".into()]),
        ..Default::default()
    };
    let source = snapshot_graph(
        &options,
        vec![
            keyed_message("cc-mirror/test", "a", "2026-03-01", "model", 3, "a"),
            keyed_message("9Router", "b", "2026-03-01", "model", 4, "b"),
        ],
    )
    .source_usage
    .unwrap();
    assert!(source.partial_clients.is_empty());
    for (client, family) in [
        ("cc-mirror/test", "claude"),
        ("9router", "gjc"),
        ("9Router", "gjc"),
    ] {
        let alias = keyed_message(client, "alias-session", "2026-03-01", "a", 3, "shared");
        let parent = keyed_message(family, "parent-session", "2026-03-02", "b", 7, "shared");
        assert_eq!(
            crate::source_event_id(&alias),
            crate::source_event_id(&parent)
        );
    }
    assert_eq!(
        crate::source_replacement_clients(Some(&["9router".into(), "synthetic".into()])),
        vec!["9router"]
    );
}

#[test]
fn source_snapshots_explicit_owned_clients_have_scope_without_changing_defaults() {
    let default_clients = crate::source_replacement_clients(None);
    for client in crate::token_monitor::client_ids() {
        assert!(!default_clients.iter().any(|selected| selected == client));
        let source = snapshot_graph(
            &ReportOptions {
                clients: Some(vec![client.into()]),
                ..Default::default()
            },
            vec![keyed_message(
                client,
                "owned-session",
                "2026-03-01",
                "model",
                15,
                "owned",
            )],
        )
        .source_usage
        .unwrap();
        assert_eq!(source.replacement_clients, vec![client]);
        assert!(source.partial_clients.is_empty());
        assert_eq!(source.sessions[0].client, client);
    }
}

#[test]
fn source_snapshots_copilot_unknown_session_is_aggregate_only() {
    let graph = snapshot_graph(
        &ReportOptions::default(),
        vec![
            keyed_message(
                "copilot",
                "unknown-session",
                "2026-03-01",
                "model",
                11,
                "span",
            ),
            keyed_message(
                "copilot",
                "trace-fallback",
                "2026-03-01",
                "model",
                2,
                "trace:span",
            ),
            keyed_message(
                "opencode",
                "unknown-session",
                "2026-03-01",
                "model",
                3,
                "native-id",
            ),
        ],
    );
    assert_eq!(graph.summary.total_tokens, 16);
    let source = graph.source_usage.unwrap();
    assert_eq!(source.rows_without_identity["copilot"], 1);
    assert_eq!(source.sessions.len(), 2);
    assert!(!source
        .sessions
        .iter()
        .any(|session| session.client == "copilot" && session.session_id == "unknown-session"));
    assert!(source
        .sessions
        .iter()
        .any(|session| session.session_id == "trace-fallback"));
}
