//! JSON Schema export. `cargo xtask schemas` writes these to `schemas/` and `--check` diffs
//! them in CI, so the committed schemas always match the code.

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema};
use serde_json::Value;

use crate::event::Event;
use crate::record::Entry;
use crate::reply::HookReply;
use crate::rpc::Rpc;

/// One exported document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exported {
    /// File name under `schemas/`.
    pub file_name: &'static str,
    /// The schema (draft 2020-12).
    pub schema: Value,
}

/// The four contract documents, in a fixed order. The journal document is rooted at [`Entry`],
/// not at `Record`: the payload on disk is the stamped record.
#[must_use]
pub fn schemas() -> Vec<Exported> {
    vec![
        export::<Event>("event.schema.json"),
        export::<HookReply>("hook-reply.schema.json"),
        export_rpc(),
        export::<Entry>("journal-record.schema.json"),
    ]
}

fn insert_rpc_definition<T: JsonSchema>(doc: &mut Exported, name: &str) {
    let mut schema = schema_for::<T>().to_value();
    if let Some(definitions) = schema.as_object_mut().unwrap().remove("$defs") {
        doc.schema["$defs"]
            .as_object_mut()
            .unwrap()
            .extend(definitions.as_object().unwrap().clone());
    }
    schema.as_object_mut().unwrap().remove("$schema");
    schema.as_object_mut().unwrap().remove("$comment");
    doc.schema["$defs"]
        .as_object_mut()
        .unwrap()
        .insert(name.to_owned(), schema);
}

fn insert_floor_views(doc: &mut Exported) {
    insert_rpc_definition::<crate::projection::FloorProjectionView<'_>>(doc, "FloorProjectionView");
    insert_rpc_definition::<crate::projection::FloorDeltaView<'_>>(doc, "FloorDeltaView");
}

fn export_rpc() -> Exported {
    let mut doc = export::<Rpc>("rpc.schema.json");
    insert_rpc_definition::<crate::rpc::team::RosterAgentParams>(&mut doc, "RosterAgentParams");
    insert_rpc_definition::<crate::rpc::closing::ClosingSchema>(&mut doc, "ClosingSchema");
    insert_floor_views(&mut doc);
    let mut doctor = schema_for::<crate::rpc::system::DoctorJournalResult>().to_value();
    let definitions = doctor.as_object_mut().unwrap().remove("$defs").unwrap();
    let defs = doc.schema["$defs"].as_object_mut().unwrap();
    defs.extend(definitions.as_object().unwrap().clone());
    doctor.as_object_mut().unwrap().remove("$schema");
    doctor.as_object_mut().unwrap().remove("$comment");
    let mut mailbox = schema_for::<crate::rpc::system::DoctorMailboxResult>().to_value();
    let definitions = mailbox.as_object_mut().unwrap().remove("$defs").unwrap();
    defs.extend(definitions.as_object().unwrap().clone());
    doctor["properties"]
        .as_object_mut()
        .unwrap()
        .extend(mailbox["properties"].as_object().unwrap().clone());
    defs.insert("DoctorResult".to_owned(), doctor);
    defs.insert(
        "JournalStoppedCode".to_owned(),
        serde_json::json!({
            "type": "integer", "const": crate::rpc::JOURNAL_STOPPED_CODE,
            "description": "The journal stopped accepting writes; data is JournalStoppedData.",
        }),
    );
    defs.get_mut("ErrorData").unwrap()["properties"]["journal_stopped"] = serde_json::json!({
        "$ref": "#/$defs/JournalStoppedData",
    });
    let mut stopped = schema_for::<crate::rpc::floor::FloorJournalStopped>().to_value();
    stopped.as_object_mut().unwrap().remove("$schema");
    stopped.as_object_mut().unwrap().remove("$comment");
    defs.insert("FloorJournalStopped".to_owned(), stopped);
    defs.insert("FloorFinishedToday".to_owned(), floor_finished_today());
    doc.schema["properties"]["error_code"] = serde_json::json!({
        "anyOf": [ { "$ref": "#/$defs/ErrorCode" }, { "$ref": "#/$defs/JournalStoppedCode" } ],
    });
    let mut messages = schema_for::<crate::MessageView>().to_value();
    let definitions = messages.as_object_mut().unwrap().remove("$defs").unwrap();
    messages.as_object_mut().unwrap().remove("$schema");
    messages.as_object_mut().unwrap().remove("$comment");
    let defs = doc.schema["$defs"].as_object_mut().unwrap();
    defs.extend(definitions.as_object().unwrap().clone());
    messages["properties"]["thread"] = serde_json::json!({"$ref":"#/$defs/ThreadId"});
    defs.insert("MessageView".into(), messages);
    let mut threads = schema_for::<crate::ThreadView>().to_value();
    let definitions = threads.as_object_mut().unwrap().remove("$defs").unwrap();
    threads.as_object_mut().unwrap().remove("$schema");
    threads.as_object_mut().unwrap().remove("$comment");
    threads["properties"]["members"]["uniqueItems"] = serde_json::json!(true);
    defs.extend(definitions.as_object().unwrap().clone());
    defs.insert("ThreadView".into(), threads);
    let window = serde_json::json!({
        "type": "array", "maxItems": crate::MAX_MESSAGES,
        "items": { "$ref": "#/$defs/MessageView" },
        "description": "Complete replacement window, at most 65,536 serialized-array bytes.",
    });
    for (name, kind, key, view, required) in [
        (
            "FloorMessagesSnapshot",
            "floor.snapshot",
            "projection",
            "FloorProjectionView",
            vec![
                "type",
                "projection",
                "messages",
                "messages_lsn",
                "threads",
                "threads_lsn",
            ],
        ),
        (
            "FloorMessagesDelta",
            "floor.delta",
            "delta",
            "FloorDeltaView",
            vec!["type", "delta", "messages_lsn", "threads_lsn"],
        ),
    ] {
        defs.insert(
            name.into(),
            serde_json::json!({
                "type": "object", "required": required,
                "dependentRequired": { "messages": ["threads"], "threads": ["messages"] },
                "properties": {
                    "type": { "type": "string", "const": kind },
                    key: { "$ref": format!("#/$defs/{view}") },
                    "messages": window,
                    "threads": { "type":"array", "maxItems":crate::MAX_THREADS,
                        "items":{"$ref":"#/$defs/ThreadView"},
                        "description":"Complete live thread graph, at most 65,536 serialized-array bytes." },
                    "threads_lsn": { "anyOf": [ { "const": "0" }, { "type": "string", "pattern": crate::messages::POSITIVE_ORDER_PATTERN } ] },
                    "journal_stopped": { "$ref": "#/$defs/FloorJournalStopped" },
                    "finished_today": { "$ref": "#/$defs/FloorFinishedToday" },
                    "messages_lsn": { "anyOf": [ { "const": "0" }, { "type": "string", "pattern": crate::messages::POSITIVE_ORDER_PATTERN } ] },
                },
            }),
        );
    }
    doc
}

/// The finished-goals sibling's definition, without the document wrapper.
fn floor_finished_today() -> Value {
    let mut finished = schema_for::<crate::rpc::floor::FloorFinishedToday>().to_value();
    finished.as_object_mut().unwrap().remove("$schema");
    finished.as_object_mut().unwrap().remove("$comment");
    finished
}

/// A root schema for `T` with the contract's settings.
#[must_use]
pub fn schema_for<T: JsonSchema>() -> Schema {
    let mut schema = SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>();
    schema.insert(
        "$comment".to_owned(),
        Value::String(format!(
            "Generated by `cargo xtask schemas` from dudes-proto; contract v{}. Do not edit.",
            crate::CONTRACT_VERSION
        )),
    );
    schema
}

fn export<T: JsonSchema>(file_name: &'static str) -> Exported {
    Exported {
        file_name,
        schema: schema_for::<T>().to_value(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four files of the `schemas/` tree, each a draft 2020-12 document
    /// with a title, and every `$ref` resolving inside the document.
    #[test]
    fn four_documents_with_resolving_refs() {
        let docs = schemas();
        let names: Vec<_> = docs.iter().map(|d| d.file_name).collect();
        assert_eq!(
            names,
            [
                "event.schema.json",
                "hook-reply.schema.json",
                "rpc.schema.json",
                "journal-record.schema.json"
            ]
        );
        for doc in &docs {
            assert_eq!(
                doc.schema["$schema"],
                "https://json-schema.org/draft/2020-12/schema"
            );
            assert!(
                doc.schema["title"].is_string(),
                "{}: no title",
                doc.file_name
            );
            let defs = doc
                .schema
                .get("$defs")
                .cloned()
                .unwrap_or(Value::Object(serde_json::Map::default()));
            check_refs(&doc.schema, &defs, doc.file_name);
            // Two Rust types with one bare name would get a numeric suffix (`Params2`), which
            // moves whenever a type is added. Names must be unique by construction.
            for name in defs.as_object().unwrap().keys() {
                assert!(
                    !name.ends_with(|c: char| c.is_ascii_digit()),
                    "{}: $defs/{name} is a disambiguated name; rename the type",
                    doc.file_name
                );
            }
        }
    }

    fn check_refs(value: &Value, defs: &Value, file: &str) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(reference)) = map.get("$ref") {
                    let name = reference
                        .strip_prefix("#/$defs/")
                        .unwrap_or_else(|| panic!("{file}: non-local $ref {reference}"));
                    assert!(
                        defs.get(name).is_some(),
                        "{file}: dangling $ref {reference}"
                    );
                }
                map.values().for_each(|v| check_refs(v, defs, file));
            }
            Value::Array(items) => items.iter().for_each(|v| check_refs(v, defs, file)),
            _ => {}
        }
    }

    /// The committed file under `schemas/`, as a client would read it (not the in-memory
    /// generator: `cargo xtask schemas --check` keeps the two equal).
    fn exported_file(file_name: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../schemas")
            .join(file_name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        serde_json::from_str(&text).unwrap()
    }

    /// Assert that `instance` validates against `schema`, printing every error.
    fn assert_valid(schema: &Value, instance: &Value, what: &str) {
        let validator = jsonschema::validator_for(schema)
            .unwrap_or_else(|err| panic!("{what}: schema does not compile: {err}"));
        let errors: Vec<String> = validator
            .iter_errors(instance)
            .map(|err| format!("{} at {}", err, err.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "{what} does not match its schema:\n{}\n{instance:#}",
            errors.join("\n")
        );
    }

    /// A schema for one `$defs` entry of `doc`: a `$ref` to it with the document's `$defs`
    /// alongside, so in-document references keep resolving.
    fn def_of(doc: &Value, name: &str) -> Value {
        assert!(doc["$defs"].get(name).is_some(), "{name} is not in $defs");
        serde_json::json!({
            "$schema": doc["$schema"],
            "$ref": format!("#/$defs/{name}"),
            "$defs": doc["$defs"],
        })
    }

    #[test]
    fn thread_schema_accepts_only_the_bounded_membership_allowlist() {
        let rpc = exported_file("rpc.schema.json");
        let validator = jsonschema::validator_for(&def_of(&rpc, "ThreadView")).unwrap();
        let a = dudes_types::AgentId::from_parts(1, 1);
        let b = dudes_types::AgentId::from_parts(1, 2);
        let row =
            serde_json::json!({"id":dudes_types::ThreadId::from_parts(1, 1), "members":[a,b]});
        assert!(validator.is_valid(&row));
        for members in [
            serde_json::json!([]),
            serde_json::json!([a]),
            serde_json::json!([a, a]),
            serde_json::json!([a, b, a, b, a, b, a, b, a]),
        ] {
            let mut invalid = row.clone();
            invalid["members"] = members;
            assert!(!validator.is_valid(&invalid));
        }
        for key in ["subject", "body", "act", "state", "n", "why", "lsn"] {
            let mut invalid = row.clone();
            invalid[key] = serde_json::json!("private");
            assert!(!validator.is_valid(&invalid));
        }
        let messages = jsonschema::validator_for(&def_of(&rpc, "MessageView")).unwrap();
        let modern = crate::MessageView {
            id: dudes_types::MailId::from_parts(1, 1),
            from: a,
            to: b,
            at: dudes_types::Timestamp::UNIX_EPOCH,
            lsn: "1".into(),
            state: crate::MessageState::Durable,
            thread: Some(dudes_types::ThreadId::from_parts(1, 1)),
        };
        let mut value = serde_json::to_value(&modern).unwrap();
        assert!(messages.is_valid(&value));
        value["thread"] = serde_json::Value::Null;
        assert!(!messages.is_valid(&value));
    }

    #[test]
    fn messages_schema_validates_serialized_rows_and_replacement_frames() {
        let rpc = exported_file("rpc.schema.json");
        let row = crate::MessageView {
            id: dudes_types::MailId::from_parts(1, 1),
            from: dudes_types::AgentId::from_parts(1, 2),
            to: dudes_types::AgentId::from_parts(1, 3),
            at: dudes_types::Timestamp::from_unix_millis(-1),
            lsn: u64::MAX.to_string(),
            state: crate::MessageState::Bounced,
            thread: None,
        };
        let snapshot = serde_json::json!({
            "type":"floor.snapshot", "projection":crate::FloorProjection::default(),
            "messages":[row], "messages_lsn":u64::MAX.to_string(), "threads":[], "threads_lsn":u64::MAX.to_string(),
        });
        assert_valid(
            &def_of(&rpc, "FloorMessagesSnapshot"),
            &snapshot,
            "Messages snapshot",
        );
        let delta = serde_json::json!({
            "type":"floor.delta", "delta":crate::FloorDelta::default(), "messages_lsn":"0", "threads_lsn":"0",
        });
        let delta_schema = def_of(&rpc, "FloorMessagesDelta");
        assert_valid(&delta_schema, &delta, "unchanged Messages delta");
        let mut cleared = delta.clone();
        cleared["messages"] = serde_json::json!([]);
        cleared["threads"] = serde_json::json!([]);
        assert_valid(&delta_schema, &cleared, "cleared Messages delta");
        let validator = jsonschema::validator_for(&def_of(&rpc, "MessageView")).unwrap();
        let encoded = serde_json::to_value(&row).unwrap();
        for private in [
            "subject",
            "body",
            "act",
            "body_len",
            "body_path",
            "body_blake3",
            "why",
            "n",
            "diagnostic",
        ] {
            let mut disclosed = encoded.clone();
            disclosed[private] = serde_json::json!("private-sentinel");
            assert!(
                !validator.is_valid(&disclosed),
                "unexpected row field {private}"
            );
        }
        for invalid in ["0", "01", "-1", "1e2", "18446744073709551616"] {
            let mut bad_order = encoded.clone();
            bad_order["lsn"] = serde_json::json!(invalid);
            assert!(
                !validator.is_valid(&bad_order),
                "non-u64 creation order {invalid}"
            );
        }
        let delta_validator = jsonschema::validator_for(&delta_schema).unwrap();
        for invalid in ["01", "-1", "1e2", "18446744073709551616"] {
            let mut bad_cut = delta.clone();
            bad_cut["messages_lsn"] = serde_json::json!(invalid);
            assert!(
                !delta_validator.is_valid(&bad_cut),
                "non-u64 stream cut {invalid}"
            );
        }
        let mut oversized = snapshot.clone();
        oversized["messages"] = serde_json::json!(vec![row; 129]);
        assert!(
            !jsonschema::validator_for(&def_of(&rpc, "FloorMessagesSnapshot"))
                .unwrap()
                .is_valid(&oversized)
        );
    }

    /// What serde writes is what the exported file describes: an `Event`, serialised,
    /// validates against `event.schema.json`. These four tests are the only place the serde
    /// attributes and the schemars output are checked against each other.
    #[test]
    fn event_validates_against_the_exported_file() {
        use crate::record::samples;

        let schema = exported_file("event.schema.json");
        let event = crate::event::Event {
            agent: samples::agent(),
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            provenance: dudes_types::Provenance::Observed,
            kind: crate::event::EventKind::PreTool {
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            raw: Some(serde_json::json!({"hook_event_name": "PreToolUse"})),
        };
        assert_valid(&schema, &serde_json::to_value(&event).unwrap(), "Event");
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(!validator.is_valid(&serde_json::json!({
            "agent": "not-a-ulid", "at": 0, "provenance": "observed", "kind": "compact_start"
        })));
    }

    /// Every `HookReply` shape validates against `hook-reply.schema.json`.
    #[test]
    fn hook_reply_validates_against_the_exported_file() {
        let schema = exported_file("hook-reply.schema.json");
        for reply in [
            crate::reply::HookReply::ALLOW,
            crate::reply::HookReply::allow_with("roster"),
            crate::reply::HookReply {
                decision: crate::reply::HookDecision::Deny {
                    reason: "budget".into(),
                },
                context: None,
            },
            crate::reply::HookReply {
                decision: crate::reply::HookDecision::Block {
                    task: "next".into(),
                },
                context: Some("steer".into()),
            },
            crate::reply::HookReply {
                decision: crate::reply::HookDecision::Halt,
                context: None,
            },
        ] {
            assert_valid(&schema, &serde_json::to_value(&reply).unwrap(), "HookReply");
        }
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(!validator.is_valid(&serde_json::json!({"decision": "deny"})));
    }

    /// Every `Record` variant, inside the [`Entry`] envelope, validates against
    /// `journal-record.schema.json`, and shapes the schema forbids fail it — including a bare,
    /// unstamped `Record`, which is the control that the root really does require `at`
    /// Before the stamped entry became the journal root, the bare record was what validated.
    #[test]
    fn every_record_validates_against_the_exported_file() {
        use crate::record::samples;

        let schema = exported_file("journal-record.schema.json");
        let validator = jsonschema::validator_for(&schema).unwrap();
        let at = dudes_types::Timestamp::from_unix_millis(1_787_394_600_000);
        for &kind in crate::record::RecordKind::KNOWN {
            let entry = Entry {
                at,
                record: samples::record(kind),
            };
            assert_valid(
                &schema,
                &serde_json::to_value(&entry).unwrap(),
                &format!("Entry::{kind:?}"),
            );
            assert!(
                !validator.is_valid(&serde_json::to_value(samples::record(kind)).unwrap()),
                "{kind:?}: an unstamped Record must not validate against the Entry root"
            );
        }
        assert!(
            !validator.is_valid(&serde_json::json!({"at": 0, "t": "pause", "agent": "not-a-ulid"}))
        );
        assert!(!validator.is_valid(&serde_json::json!({"at": 0, "t": "no_such_record"})));
    }

    /// the two provider-run limits are optional, but a supplied value is strictly
    /// positive in both public documents. The constraint belongs to these fields rather than to
    /// [`dudes_types::Usd`]: zero remains a valid value for the shared budget type everywhere
    /// that the three-state ceiling contract uses it.
    #[test]
    fn provider_run_limits_are_positive_only_in_rpc_and_journal_schemas() {
        fn assert_optional_positive(schema: &Value, baseline: &Value, field: &str, document: &str) {
            let validator = jsonschema::validator_for(schema).unwrap();

            let mut absent = baseline.clone();
            absent.as_object_mut().unwrap().remove(field);
            assert!(
                validator.is_valid(&absent),
                "{document}: an absent `{field}` remains valid"
            );

            let mut null = baseline.clone();
            null[field] = Value::Null;
            assert!(
                validator.is_valid(&null),
                "{document}: a null `{field}` remains valid"
            );

            let mut positive = baseline.clone();
            positive[field] = Value::from(1);
            assert!(
                validator.is_valid(&positive),
                "{document}: the smallest positive `{field}` remains valid"
            );

            let mut zero = baseline.clone();
            zero[field] = Value::from(0);
            assert!(
                !validator.is_valid(&zero),
                "{document}: a supplied zero `{field}` must be rejected"
            );
        }

        let rpc = exported_file("rpc.schema.json");
        let hire = crate::rpc::agent::HireParams {
            character: None,
            accent: None,
            name: "Kim".into(),
            role: "backend engineer".into(),
            department: "engineering".into(),
            provider: dudes_types::Provider::Claude,
            cwd: "/work/repo".into(),
            budget: crate::AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: false,
            checkout: None,
            from: None,
            mcp_servers: Vec::new(),
        };
        let hire = serde_json::to_value(hire).unwrap();
        let hire_schema = def_of(&rpc, "HireParams");
        for field in ["max_turns", "max_budget"] {
            assert_optional_positive(&hire_schema, &hire, field, "rpc.schema.json/HireParams");
        }

        let journal = exported_file("journal-record.schema.json");
        let hired = crate::record::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            record: crate::record::samples::record(crate::record::RecordKind::AgentHired),
        };
        let hired = serde_json::to_value(hired).unwrap();
        for field in ["max_turns", "max_budget"] {
            assert_optional_positive(&journal, &hired, field, "journal-record.schema.json");
        }

        for (document, schema) in [
            ("rpc.schema.json", &rpc),
            ("journal-record.schema.json", &journal),
        ] {
            assert_eq!(
                schema["$defs"]["Usd"]["minimum"], 0,
                "{document}: the provider field must not narrow shared Usd"
            );
        }
    }

    /// The one `oneOf` branch whose tag property `key` holds `tag`. Fails on none and on more
    /// than one, so a renamed variant cannot make a shape assertion vacuously true.
    fn tagged<'a>(one_of: &'a Value, key: &str, tag: &str) -> &'a Value {
        let hits: Vec<&Value> = one_of
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["properties"][key]["const"] == tag)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one `{tag}` branch tagged by `{key}`"
        );
        hits[0]
    }

    /// The one `Record` branch whose `t` is `tag`.
    fn branch<'a>(one_of: &'a Value, tag: &str) -> &'a Value {
        tagged(one_of, "t", tag)
    }

    /// The command-evidence branch embedded in an exported contract document.
    fn command_evidence(doc: &Value) -> &Value {
        tagged(&doc["$defs"]["Evidence"]["oneOf"], "kind", "command")
    }

    /// A schema's `required` list, as strings.
    fn required(schema: &Value) -> Vec<&str> {
        schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect()
    }

    const HOOK_TOKEN_MINTED_DESCRIPTION: &str = concat!(
        "The supervisor minted this agent's token before the agent's first spawn of this daemon run —\n",
        "managed and attached alike. The token itself is never journaled, only its blake3; the\n",
        "plaintext does not survive a restart, so a restart mints again and every hash journaled\n",
        "for a live agent stays valid. What a presented token resolves to is decided by the\n",
        "peer-classification table, never restated here."
    );

    const TURN_SPAWNED_BOOT_DESCRIPTION: &str = concat!(
        "The boot identity of the machine the child started on, where the platform's start\n",
        "value is boot-relative (Linux: `/proc/sys/kernel/random/boot_id`); `None` where the\n",
        "start value is captured at creation (macOS, Windows). Opaque: compared for equality,\n",
        "never parsed. `None` asserts the writer's start value is wall-clock-anchored, and never\n",
        "\"boot-relative but unreadable\" — that case is a spawn failure. How the key is\n",
        "compared is decided by the liveness table, never restated here."
    );

    const KILL_DEFERRED_DESCRIPTION: &str = concat!(
        "A standing cause would have killed this turn's child, but the child's liveness is `Unknown`,\n",
        "so no signal was sent. Appended once per `(turn, standing cause)`\n",
        "transition, never once per retry — the retry loop is in memory, because a\n",
        "foreign boot identity is an `Unknown` that no action on this host resolves."
    );

    fn exact_description(value: &Value, path: &str, expected: &str) -> Result<(), String> {
        let actual = value
            .as_str()
            .ok_or_else(|| format!("{path}: description is not a string"))?;
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "{path}: description differs\nexpected: {expected:?}\n  actual: {actual:?}"
            ))
        }
    }

    #[test]
    fn protected_record_descriptions_are_exact_in_every_document() {
        let journal = exported_file("journal-record.schema.json");
        let rpc = exported_file("rpc.schema.json");

        for (file, entry) in [
            ("journal-record.schema.json", &journal),
            ("rpc.schema.json", &rpc["$defs"]["Entry"]),
        ] {
            for (path, value, expected) in [
                (
                    "HookTokenMinted.description",
                    &branch(&entry["oneOf"], "hook_token_minted")["description"],
                    HOOK_TOKEN_MINTED_DESCRIPTION,
                ),
                (
                    "TurnSpawned.properties.boot.description",
                    &branch(&entry["oneOf"], "turn_spawned")["properties"]["boot"]["description"],
                    TURN_SPAWNED_BOOT_DESCRIPTION,
                ),
                (
                    "KillDeferred.description",
                    &branch(&entry["oneOf"], "kill_deferred")["description"],
                    KILL_DEFERRED_DESCRIPTION,
                ),
            ] {
                exact_description(value, path, expected)
                    .unwrap_or_else(|err| panic!("{file}: {err}"));
            }
        }
    }

    #[test]
    fn degraded_hook_token_description_is_rejected() {
        let degraded = Value::String(HOOK_TOKEN_MINTED_DESCRIPTION.replace("agent's", "agent"));
        let error = exact_description(
            &degraded,
            "HookTokenMinted.description",
            HOOK_TOKEN_MINTED_DESCRIPTION,
        )
        .expect_err("the historically degraded possessives must fail exact comparison");
        assert!(
            error.contains("HookTokenMinted.description"),
            "the failure must name the protected description: {error}"
        );
    }

    /// The observed attestation is grow-only on the public contract: `program` remains optional in both
    /// exported documents, so records written before the field existed and no-spawn refusals keep
    /// validating. Exact required-set equality makes adding the field load-bearing rather than a
    /// serde-only promise.
    #[test]
    fn predicate_program_is_absent_from_the_schema_required_set() {
        for file in ["journal-record.schema.json", "rpc.schema.json"] {
            let doc = exported_file(file);
            let command = command_evidence(&doc);
            assert!(
                command["properties"]["program"].is_object(),
                "{file}: program is still an exported property"
            );
            assert_eq!(
                required(command),
                ["kind", "argv"],
                "{file}: program must remain optional"
            );
        }
    }

    /// `program` names what was spawned, so every no-spawn outcome leaves it
    /// absent — including a successful resolution that is then refused for canonical containment.
    /// The generated descriptions are pinned because those are the client-visible contract.
    #[test]
    fn predicate_program_doc_covers_the_in_tree_refusal() {
        for file in ["journal-record.schema.json", "rpc.schema.json"] {
            let doc = exported_file(file);
            let description = command_evidence(&doc)["properties"]["program"]["description"]
                .as_str()
                .expect("program has a description");
            assert!(
                description.contains("Absent whenever nothing ran"),
                "{file}: absence must cover every no-spawn outcome: {description}"
            );
            assert!(
                description.contains("in-tree refusal"),
                "{file}: the fourth absent case is named: {description}"
            );
        }
    }

    /// The `Record` shape both exported documents must agree on: every field added to the record
    /// is a property and none of them joined `required`, and the field it removed is gone.
    fn assert_record_shape(file: &str, one_of: &Value) {
        assert_eq!(
            one_of.as_array().unwrap().len(),
            crate::record::RecordKind::KNOWN.len(),
            "{file}: variant count"
        );

        let spawned = branch(one_of, "turn_spawned");
        assert!(
            spawned["properties"]["mcp_grant"].is_object(),
            "{file}: optional MCP grant missing"
        );
        assert!(
            spawned["properties"]["boot"].is_object(),
            "{file}: boot missing"
        );
        assert_eq!(
            required(spawned),
            ["t", "agent", "turn", "pid", "start_time"],
            "{file}: boot must not be required"
        );

        let attached = branch(one_of, "attached");
        assert!(
            attached["properties"]["hook_port"].is_object(),
            "{file}: hook_port missing"
        );
        assert_eq!(
            required(attached),
            ["t", "agent", "by"],
            "{file}: hook_port must not be required"
        );

        let minted = branch(one_of, "hook_token_minted");
        assert!(
            minted["properties"]["token_hash"].is_object(),
            "{file}: control — this is the mint"
        );
        assert!(
            minted["properties"].get("port").is_none(),
            "{file}: the mint kept its port"
        );
        assert_eq!(required(minted), ["t", "agent", "token_hash"], "{file}");

        let ended = branch(one_of, "turn_ended");
        assert!(
            ended["properties"]["outcome"].is_object(),
            "{file}: outcome missing"
        );
        assert_eq!(
            required(ended),
            ["t", "agent", "turn", "mode", "provenance"],
            "{file}: outcome must not be required"
        );

        branch(one_of, "kill_deferred");
    }

    /// `cargo xtask schemas --check` proves the committed file matches the generator; it cannot
    /// prove the shape is right — a field could regenerate straight into `required` and
    /// `--check` would still be clean. This asserts the shape, in **both** exported documents:
    /// [`Entry`] is embedded in the RPC document too (`journal.appended` carries the frame
    /// payload verbatim), so one payload change lands in two files. Each
    /// assertion is falsifiable because it names an expected value: [`branch`] fails on none and
    /// on more than one, and `required` is compared as an exact list rather than by membership,
    /// so a field that joins it fails here. `at` is asserted to be a property *and* the whole of
    /// `required` on `Entry` in each document, not just in the journal one — a stamp required on
    /// only one side is a contract that disagrees with itself. Each document also asserts
    /// `$defs.Record` is absent: the unstamped shape is not a contract item anywhere, so a
    /// re-flattening or a half-done migration that leaves it behind fails here rather than
    /// passing.
    #[test]
    fn optional_record_fields_are_in_properties_and_never_required() {
        let journal = exported_file("journal-record.schema.json");
        let rpc = exported_file("rpc.schema.json");

        assert_eq!(
            journal["title"], "Entry",
            "the journal document is rooted at Entry"
        );
        assert!(
            journal["$defs"].get("Record").is_none(),
            "flatten inlines the variants; a $defs.Record means the shape changed"
        );
        assert!(
            rpc["$defs"]["Entry"].is_object(),
            "`journal.appended` carries the stamped Entry"
        );
        assert!(
            rpc["$defs"].get("Record").is_none(),
            "the RPC document keeps no unstamped Record; something still refs the old shape"
        );

        // Every assertion below runs against both documents: the journal document is rooted at
        // `Entry`, the RPC document embeds the same type at `$defs.Entry`, and a stamp that is a
        // property in one and required in only one is a contract that disagrees with itself.
        for (file, entry) in [
            ("journal-record.schema.json", &journal),
            ("rpc.schema.json", &rpc["$defs"]["Entry"]),
        ] {
            assert!(
                entry["properties"]["at"].is_object(),
                "{file}: `at` is a property of Entry"
            );
            assert_eq!(required(entry), ["at"], "{file}: `at` is required on Entry");
            assert_record_shape(file, &entry["oneOf"]);
        }

        for (file, doc) in [
            ("journal-record.schema.json", &journal),
            ("rpc.schema.json", &rpc),
        ] {
            let acts: Vec<&str> = doc["$defs"]["MailAct"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["const"].as_str().unwrap())
                .collect();
            assert_eq!(
                acts,
                ["message", "request", "card", "ack"],
                "{file}: MailAct"
            );
        }
    }

    /// the additive `body` is a string-or-null property of the **mail
    /// record** in both exported documents and joins no `required` set.
    ///
    /// **The assertion is scoped to the mail record**:
    /// `rpc.schema.json` carries a second, unrelated `body` — `MailSendParams.body`, a required
    /// `String` — so a check that searched the document by name would pass on the wrong one.
    /// [`branch`] resolves the record by its `t` const and fails on none and on more than one, and
    /// the control at the end of this test asserts that the other `body` really is there and
    /// really is required.
    ///
    /// The exact `required` list and the three retained types are the runnable form of the add-only
    /// rule: a field
    /// removed, re-typed, or promoted into `required` fails here rather than in a client.
    #[test]
    fn the_mail_body_is_an_optional_string_in_both_documents() {
        let string_or_null = serde_json::json!(["string", "null"]);
        let journal = exported_file("journal-record.schema.json");
        let rpc = exported_file("rpc.schema.json");

        for (file, entry) in [
            ("journal-record.schema.json", &journal),
            ("rpc.schema.json", &rpc["$defs"]["Entry"]),
        ] {
            let mail = branch(&entry["oneOf"], "mail");
            assert_eq!(
                mail["properties"]["body"]["type"], string_or_null,
                "{file}: the inline body is a string-or-null property"
            );
            assert!(
                mail["properties"]["body"].get("maxLength").is_none(),
                "{file}: the 4 KiB inline cap is enforced by the emitter, so the schema carries no maxLength"
            );
            assert_eq!(
                required(mail),
                [
                    "t", "id", "from", "to", "act", "subject", "body_len", "state"
                ],
                "{file}: body must not be required, and no other field may join or leave"
            );
            assert_eq!(
                mail["properties"]["body_len"]["type"], "integer",
                "{file}: body_len keeps its name and type"
            );
            assert_eq!(
                mail["properties"]["body_path"]["type"], string_or_null,
                "{file}: body_path keeps its name and type"
            );
            assert_eq!(
                mail["properties"]["body_blake3"]["type"], string_or_null,
                "{file}: body_blake3 keeps its name and type"
            );
        }

        let send = &rpc["$defs"]["MailSendParams"];
        assert_eq!(
            send["properties"]["body"]["type"], "string",
            "control: the RPC document carries a second `body` of a different shape"
        );
        assert!(
            required(send).contains(&"body"),
            "control: and that one is required, which the record's inline body is not"
        );
    }

    /// A card record names the same mail-carried board route whether it creates or transitions
    /// the card, and delegates status authorship to the `BoardState` predicate instead of restating caller roles.
    #[test]
    fn card_record_descriptions_share_the_mail_route() {
        const MAIL_ROUTE: &str = "or a board action carried by mail `act: card`; status authorship is checked \
                                  where a `BoardState` goal predicate reads the card.";

        let journal = exported_file("journal-record.schema.json");
        let rpc = exported_file("rpc.schema.json");

        for (file, doc, entry) in [
            ("journal-record.schema.json", &journal, &journal),
            ("rpc.schema.json", &rpc, &rpc["$defs"]["Entry"]),
        ] {
            let card_acts = doc["$defs"]["MailAct"]["oneOf"]
                .as_array()
                .expect("MailAct is a oneOf")
                .iter()
                .filter(|act| act["const"] == "card")
                .count();
            assert_eq!(
                card_acts, 1,
                "control: {file} publishes exactly one mail `act: card`"
            );

            for (tag, direct_method, suffix) in [
                ("card_created", "`board.create`", ""),
                (
                    "card_transition",
                    "`board.transition`",
                    " Conditional: rejected when `expected_seq` is stale.",
                ),
            ] {
                let record = branch(&entry["oneOf"], tag);
                assert_eq!(
                    record["properties"]["t"]["const"], tag,
                    "control: {file} selected the `{tag}` branch"
                );
                let description = record["description"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{file}: `{tag}` has no description"));
                assert!(
                    description.contains(direct_method),
                    "control: {file}: `{tag}` names its direct method: {description}"
                );
                let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
                assert_eq!(
                    description,
                    format!("{direct_method} {MAIL_ROUTE}{suffix}"),
                    "{file}: `{tag}` must publish the shared mail route and cite its authority"
                );
            }
        }
    }

    /// The other half of the shape contract: the three authorship fields of the board status are
    /// **required** in every exported document that carries them.
    /// [`optional_record_fields_are_in_properties_and_never_required`] pins the fields that must
    /// stay *out* of `required`; nothing pinned the fields that must stay *in*, so their
    /// required-ness was whatever the generator happened to emit. Required-ness is the property
    /// rather than presence: a reader that may receive a card status with no author reads
    /// *absent* as *fine*, a hole arriving through the
    /// published contract instead of through the type. Each list is compared exactly, so a field
    /// that leaves `required` and a field that joins it both fail here.
    ///
    /// `cargo xtask schemas --check` sees neither: it holds the committed file equal to the
    /// generator, and a field that regenerates straight out of `required` moves both together.
    #[test]
    fn the_authorship_fields_are_required_in_every_document() {
        let journal = exported_file("journal-record.schema.json");
        let rpc = exported_file("rpc.schema.json");

        // `Record::CardCreated.by`: a card's first status has an author in the records at all.
        // Asserted in both documents — `Entry` is embedded in the RPC document as well
        // — because an author required on only one side is a contract that
        // disagrees with itself.
        for (file, entry) in [
            ("journal-record.schema.json", &journal),
            ("rpc.schema.json", &rpc["$defs"]["Entry"]),
        ] {
            let created = branch(&entry["oneOf"], "card_created");
            assert!(
                created["properties"]["by"].is_object(),
                "{file}: CardCreated.by is a property"
            );
            assert_eq!(
                required(created),
                ["t", "card", "title", "department", "by"],
                "{file}: CardCreated.by is required"
            );
        }

        // `Evidence::Card.by`: the author carried into the predicate result, without which the
        // evidence is not enough to be re-checked by a reader.
        for (file, doc) in [
            ("journal-record.schema.json", &journal),
            ("rpc.schema.json", &rpc),
        ] {
            let card = tagged(&doc["$defs"]["Evidence"]["oneOf"], "kind", "card");
            assert!(
                card["properties"]["by"].is_object(),
                "{file}: Evidence::Card.by is a property"
            );
            assert_eq!(
                required(card),
                ["kind", "card", "status", "by"],
                "{file}: Evidence::Card.by is required"
            );
        }

        // `CardView.status_by`: the projection's single answer, asserted in the one document
        // that publishes the projection. The absence below is the control that this is checked
        // where the type actually is, rather than skipped in a document it never reached.
        assert!(
            journal["$defs"].get("CardView").is_none(),
            "control: the journal document publishes no CardView"
        );
        let card_view = &rpc["$defs"]["CardView"];
        assert!(
            card_view["properties"]["status_by"].is_object(),
            "rpc.schema.json: CardView.status_by is a property"
        );
        assert_eq!(
            required(card_view),
            ["id", "title", "department", "status", "status_by", "seq"],
            "rpc.schema.json: CardView.status_by is required"
        );
    }

    /// A `Request`, its typed params, a `Response`, each of the three typed error payloads a
    /// `Response` carries, `hook.event` params and `journal.appended` params validate against
    /// their entries in `rpc.schema.json`. Serde shape and schema shape are each asserted
    /// elsewhere; this is the only place the *join* between them is, so a type whose
    /// serialisation and whose generated schema each look right but do not describe each other
    /// fails here.
    ///
    /// An error payload needs its own arm rather than riding on the frame: `RpcError::data` is
    /// an untyped `Value`, so a `Response` validates whatever it carries and the payload's join
    /// is asserted directly instead. `cargo xtask schemas --check` cannot stand in for any of
    /// this — it compares the generator against the committed file, and a serde/schemars
    /// divergence moves neither.
    #[allow(clippy::too_many_lines)] // one arm per wire shape; a checklist, not logic
    #[test]
    fn rpc_shapes_validate_against_the_exported_file() {
        use crate::record::samples;

        let schema = exported_file("rpc.schema.json");
        let request = crate::rpc::Request {
            jsonrpc: crate::rpc::JSONRPC.to_owned(),
            id: crate::rpc::RequestId::Number(7),
            method: crate::rpc::Method::GoalSet.name().to_owned(),
            params: Some(
                serde_json::to_value(crate::rpc::goal::GoalSetParams {
                    agent: samples::agent(),
                    text: "All tests green".into(),
                    predicate: crate::goal::Predicate::Command {
                        argv: vec!["npm".into(), "test".into()],
                        timeout_s: 600,
                    },
                    budget: crate::budget::GoalBudget {
                        usd: Some(dudes_types::Usd::from_dollars(15)),
                        tokens: None,
                    },
                    hop_cap: 40,
                    check: None,
                    may_already_pass: false,
                })
                .unwrap(),
            ),
            v: Some(crate::CONTRACT_VERSION),
            // Present rather than `None`, so the join asserted here covers the field a call
            // presents a credential on. A `None` would serialise away and validate the absent
            // case only, which is the case that cannot fail.
            cred: Some(crate::rpc::cred::OperatorCredential::new("C".repeat(43)).unwrap()),
        };
        let request_json = serde_json::to_value(&request).unwrap();
        assert_valid(&def_of(&schema, "Request"), &request_json, "Request");
        assert!(
            request_json["cred"].is_string(),
            "positive control: the credential reached the validated document"
        );
        assert_valid(
            &def_of(&schema, "GoalSetParams"),
            &request_json["params"],
            "goal.set params",
        );

        let hook = crate::rpc::hook::HookEventParams {
            token: crate::rpc::hook::HookToken::new("A".repeat(43)).unwrap(),
            raw: serde_json::json!({"hook_event_name": "Stop"}),
        };
        assert_valid(
            &def_of(&schema, "HookEventParams"),
            &serde_json::to_value(&hook).unwrap(),
            "hook.event params",
        );
        let validator = jsonschema::validator_for(&def_of(&schema, "HookEventParams")).unwrap();
        assert!(!validator.is_valid(&serde_json::json!({"token": "short", "raw": {}})));

        // The three typed error payloads. `RpcError::data` is an untyped `Value`, and the
        // exported document says so — `$defs/RpcError/properties/data` carries a description and
        // no `type`, no `$ref` — so validating the `Response` alone can see nothing inside it:
        // every payload passes, including one no client could read. The join therefore has to be
        // made on the payload itself, lifted back out of the serialised frame rather than written
        // a second time by hand, so what is validated is what the daemon would actually put on
        // the wire. Each frame below keeps its `Response` arm as well, because that arm passing
        // while the payload arm fires is what shows the payload arm is not the vacuous one.

        // The conflict frame.
        let response = crate::rpc::Response::err(
            crate::rpc::RequestId::Number(7),
            crate::rpc::RpcError {
                code: crate::rpc::ErrorCode::Conflict.code(),
                message: "stale seq".into(),
                data: Some(
                    serde_json::to_value(crate::rpc::ConflictData {
                        current_seq: dudes_types::Seq(4),
                    })
                    .unwrap(),
                ),
            },
        );
        let response_json = serde_json::to_value(&response).unwrap();
        assert_valid(&def_of(&schema, "Response"), &response_json, "Response");
        assert!(
            response_json["error"]["data"]["current_seq"].is_number(),
            "positive control: the conflicting seq reached the validated document"
        );
        assert_valid(
            &def_of(&schema, "ConflictData"),
            &response_json["error"]["data"],
            "ConflictData as the conflict frame carries it",
        );

        // The provider-error frame. Two fields rather than one, and one of them behind a `$ref`,
        // so a one-sided rename has two places here to hide.
        let provider_error = crate::rpc::Response::err(
            crate::rpc::RequestId::Number(8),
            crate::rpc::RpcError {
                code: crate::rpc::ErrorCode::ProviderError.code(),
                message: "the CLI exited 1".into(),
                data: Some(
                    serde_json::to_value(crate::rpc::ProviderErrorData {
                        provider: dudes_types::Provider::Claude,
                        detail: "no such session".into(),
                    })
                    .unwrap(),
                ),
            },
        );
        let provider_error_json = serde_json::to_value(&provider_error).unwrap();
        assert_valid(
            &def_of(&schema, "Response"),
            &provider_error_json,
            "Response carrying ProviderErrorData",
        );
        assert!(
            provider_error_json["error"]["data"]["provider"].is_string()
                && provider_error_json["error"]["data"]["detail"].is_string(),
            "positive control: both provider-error fields reached the validated document"
        );
        assert_valid(
            &def_of(&schema, "ProviderErrorData"),
            &provider_error_json["error"]["data"],
            "ProviderErrorData as the provider-error frame carries it",
        );

        // The refusal frame.
        let refused = crate::rpc::RefusedData {
            gate: crate::rpc::RefusalGate::Token,
        };
        let refusal = crate::rpc::Response::err(
            crate::rpc::RequestId::Number(9),
            crate::rpc::RpcError {
                code: crate::rpc::ErrorCode::Refused.code(),
                message: "no such minting".into(),
                data: Some(serde_json::to_value(refused).unwrap()),
            },
        );
        let refusal_json = serde_json::to_value(&refusal).unwrap();
        assert_valid(
            &def_of(&schema, "Response"),
            &refusal_json,
            "Response carrying RefusedData",
        );
        assert!(
            refusal_json["error"]["data"]["gate"].is_string(),
            "positive control: the gate reached the validated document"
        );
        assert_valid(
            &def_of(&schema, "RefusedData"),
            &refusal_json["error"]["data"],
            "RefusedData as the refusal frame carries it",
        );

        // Every gate the exported document declares must be one the daemon's own deserialiser
        // accepts and re-emits unchanged. What this catches is a *one-sided* move:
        // `#[schemars(rename = "…")]` changes the published word and leaves serde's alone, so the
        // document names a gate no client's payload could ever be read as. A plain
        // `#[serde(rename = "…")]` is not that case — schemars honours serde's rename, so both
        // sides move together and agree, and the word they agree on is guarded by the per-variant
        // spelling tests in `rpc`, not here. Driven from the document rather than from a list of
        // variants, so no count is pinned here and a new gate needs no edit.
        let gates: Vec<&str> = schema["$defs"]["RefusalGate"]["oneOf"]
            .as_array()
            .expect("$defs/RefusalGate is a oneOf of consts")
            .iter()
            .map(|b| b["const"].as_str().expect("each branch is a const string"))
            .collect();
        assert!(
            !gates.is_empty(),
            "control: the document declares at least one gate, so the loop below runs"
        );
        for gate in gates {
            let wire = serde_json::json!({ "gate": gate });
            let parsed: crate::rpc::RefusedData = serde_json::from_value(wire.clone())
                .unwrap_or_else(|err| {
                    panic!("rpc.schema.json publishes gate `{gate}`, RefusedData refuses it: {err}")
                });
            assert_eq!(
                serde_json::to_value(parsed).unwrap(),
                wire,
                "gate `{gate}` does not serialise back to the word the document publishes"
            );
        }

        let appended = crate::rpc::journal::JournalAppendedParams {
            lsn: dudes_types::Lsn(7),
            entry: crate::record::Entry {
                at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
                record: samples::record(crate::record::RecordKind::AgentHired),
            },
        };
        let appended_json = serde_json::to_value(&appended).unwrap();
        assert_valid(
            &def_of(&schema, "JournalAppendedParams"),
            &appended_json,
            "journal.appended params",
        );

        // The rejecting arm, performed on the real payload rather than a hand-written literal:
        // hoist `entry`'s keys to the params root and drop `entry`, which is exactly what a
        // `#[serde(flatten)]` on the field emits. schemars writes no `additionalProperties:
        // false`, so what the exported schema refuses here is the missing required `entry`, not
        // the stray `at` beside it; a stray key that arrives *without* displacing `entry` is
        // invisible to any validator and is caught in `rpc::journal`'s absence assertions
        // instead.
        let mut flattened = appended_json.clone();
        let entry = flattened.as_object_mut().unwrap().remove("entry").unwrap();
        for (key, value) in entry.as_object().unwrap() {
            flattened[key] = value.clone();
        }
        assert!(
            flattened["at"].is_number() && flattened.get("entry").is_none(),
            "control: the hoist put `at` at the params root and removed `entry`"
        );
        let validator =
            jsonschema::validator_for(&def_of(&schema, "JournalAppendedParams")).unwrap();
        assert!(
            !validator.is_valid(&flattened),
            "re-flattened `journal.appended` params must not validate:\n{flattened:#}"
        );
    }

    #[test]
    fn look_schema_requires_explicit_nullable_appearance() {
        let rpc = exported_file("rpc.schema.json");
        let schema = def_of(&rpc, "LookParams");
        let validator = jsonschema::validator_for(&schema).unwrap();
        let automatic = serde_json::json!({
            "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "role": "designer",
            "character": null, "accent": null,
        });
        assert!(
            validator.is_valid(&automatic),
            "explicit Automatic must validate"
        );
        for absent in ["character", "accent"] {
            let mut incomplete = automatic.clone();
            incomplete.as_object_mut().unwrap().remove(absent);
            assert!(
                !validator.is_valid(&incomplete),
                "missing {absent} must fail"
            );
        }
        let selected = serde_json::json!({
            "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "role": "designer",
            "character": "h", "accent": "sky",
        });
        assert!(
            validator.is_valid(&selected),
            "authored choices must validate"
        );
    }

    /// The RPC document carries every method and notification by wire name, so a generated
    /// client cannot miss one.
    #[test]
    fn rpc_schema_lists_every_method_and_notification() {
        let rpc = schema_for::<Rpc>().to_value();
        let methods = &rpc["$defs"]["Methods"]["properties"];
        for method in crate::rpc::Method::ALL {
            assert!(methods.get(method.name()).is_some(), "{method} missing");
        }
        let notifications = &rpc["$defs"]["Notifications"]["properties"];
        for kind in crate::rpc::NotificationKind::ALL {
            assert!(notifications.get(kind.name()).is_some(), "{kind} missing");
        }
    }

    #[test]
    fn roster_schema_separates_empty_business_input_from_agent_presentation() {
        let rpc = exported_file("rpc.schema.json");
        let agent_schema = def_of(&rpc, "RosterAgentParams");
        let agent = jsonschema::validator_for(&agent_schema).unwrap();
        let token = serde_json::json!({"token":"t".repeat(43)});
        assert!(agent.is_valid(&token));
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"token":"t".repeat(43),"agent":dudes_types::AgentId::NIL}),
        ] {
            assert!(!agent.is_valid(&invalid));
        }
        let empty_schema = def_of(&rpc, "Empty");
        let empty = jsonschema::validator_for(&empty_schema).unwrap();
        assert!(empty.is_valid(&serde_json::json!({})));
        assert!(!empty.is_valid(&token));
        assert_eq!(
            rpc["$defs"]["Methods"]["properties"]["team.roster"]["properties"]["params"]["$ref"],
            "#/$defs/Empty"
        );
        let properties = rpc["$defs"]["RosterEntry"]["properties"]
            .as_object()
            .unwrap();
        assert_eq!(
            properties
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "id",
                "name",
                "role",
                "department",
                "status",
                "held_card"
            ])
        );
    }
}
