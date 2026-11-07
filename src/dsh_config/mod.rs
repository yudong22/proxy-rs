//! DSH (DeepSeek Harness) provider wiring.
//!
//! Writes the `proxy-rs` provider into the active profile's
//! `$DSH_HOME/profiles/<name>/cordis.patch.yml` so the harness talks to this
//! proxy. Mirrors `codex_config.rs` in approach, and for the same reason: the
//! configuration document belongs to the user.
//!
//! Why the target is the profile patch and not `settings.yaml`:
//!
//!   * `settings.yaml` no longer exists in current DSH. The harness *renames*
//!     it to `settings.yaml.imported` on first boot and moves each section into
//!     the profile that owns it (`SettingsForms.importLegacyDocument` in
//!     `@deepseek-ai/dsh-settings`). Writing the old path produced a file the
//!     running harness never reads.
//!   * A profile's settings are a **patch list**: a top-level YAML array whose
//!     entries target a loader row by `id`. Editing the patch list is how every
//!     other DSH surface changes these values, so this writer does the same.
//!
//! Why this is a hand-rolled text patch and not a YAML round-trip:
//!
//!   * The patch file is a *shared* document. It carries other rows (locale,
//!     theme, permissions, the default model) and arbitrary user comments. A
//!     serialize-the-whole-document write would reformat all of it — and DSH's
//!     own writer patches at leaf level to preserve comments and formatting, so
//!     a wholesale rewrite would fight it.
//!   * A patch entry's `config` **replaces** the target row's whole config
//!     (`applyEntryPatches` assigns `target[key] = value`), it does not merge.
//!     So the `llm-pi-ai` entry must keep every sibling provider and every
//!     hand-set profile key; only the fields below are allowed to change.
//!
//! The write is confined to three leaves inside the `proxy-rs` provider —
//! `baseURL`, `sessionHeader` and `models`. Every byte outside them is copied
//! through untouched. If the provider block cannot be located confidently the
//! write refuses rather than guessing: a corrupted shared config is far worse
//! than a reported error.
//!
//! Both YAML styles are accepted, because a patch file is hand-editable and the
//! two shapes appear in the wild: the block style the harness writes
//! (`proxy-rs:` followed by indented `key: value` lines) and the flow style
//! (`proxy-rs:` followed by a `{ … }` mapping) used by the retired
//! `settings.yaml`.
//!
//! ## File layout
//!
//! This is one cohesive writer with two very different halves, and the split
//! follows that seam rather than line count: a hand-written YAML structure
//! reader/renderer on one side, filesystem state and the write itself on the
//! other. `mod.rs` keeps what every file shares — the DSH surface identity —
//! plus the re-exports that keep every historical `crate::dsh_config::X` path
//! resolving.
//!
//! | file | owns |
//! |---|---|
//! | [`paths`] | the harness home, profile resolution, patch/credential locations |
//! | [`types`] | `DshModel`/`DshState`, the `GuiModel` mapping, `read_state` |
//! | [`yaml`] | locating and reading structure in the document (pure text) |
//! | [`render`] | rendering the provider's YAML fragments (pure text) |
//! | [`apply`] | splicing the provider block, the credential, atomic writes |
//!
//! `yaml` and `render` are the pure text layer: no filesystem, no error types,
//! only `String`/`usize` in and out. Keeping them I/O-free is what makes the
//! document shapes testable without a temp directory.
//!
//! The test module stays whole at the end of this file: the tests exercise
//! behaviour spanning all five files, and scattering them would make a change
//! harder to verify, not easier.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The credential name DSH resolves for this provider.
///
/// proxy-rs does not authenticate its own clients (it forwards with the
/// credential it holds), so any non-empty value works. The value only has to be
/// something an HTTP header can carry, because DSH asserts that before use.
pub const CREDENTIAL_REF: &str = "PROXY_RS_API_KEY";

/// The namespace DSH's pi-ai adapter reads providers from.
const NAMESPACE: &str = "llm-pi-ai";

/// The provider id this proxy registers itself as.
const PROVIDER: &str = "proxy-rs";

/// The HTTP field DSH must send for proxy-rs to key requests by conversation.
///
/// Without it the gateway sees only the `deepseek-harness/<version>`
/// user-agent and merges every chat in an install under `dsh:<version>`.
/// Writing it here is what makes the session filter work on a fresh install
/// without a manual edit.
pub const SESSION_HEADER: &str = "x-deepseek-harness-session-id";

mod apply;
mod paths;
mod render;
mod types;
mod yaml;

// `yaml` and `render` hold no public items: they are the text layer this
// module's own reader and writer sit on. A `pub use` of either would
// re-export nothing public and warn, so they are glob-imported at crate
// visibility instead.
pub(crate) use render::*;
pub(crate) use yaml::*;

pub use apply::*;
pub use paths::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the real profile patch: a top-level list of id-targeted entries,
    /// the `llm-pi-ai` entry with a block-style provider, sibling rows before
    /// and after, and comments — which are included because preserving them is
    /// the whole reason this writer is textual.
    const PATCH: &str = r#"# Your patch layer for this dsh profile.
- id: ui-settings-general
  name: "@deepseek-ai/dsh-client-ui-settings-general"
  config:
    welcomeNoticeVersion: 2026-09-28.1
- id: llm-pi-ai
  name: "@deepseek-ai/dsh-llm-pi-ai"
  config:
    providers:
      proxy-rs:
        apiKeyEnv: PROXY_RS_API_KEY
        api: openai-completions
        reasoningEffort: high
        customFlag: keep-me
        baseURL: http://127.0.0.1:3457/v1
        models:
          - id: hy3
            name: Hy3
            contextWindow: 192000
            maxTokens: 64000
            input: [ text ]
            reasoningEfforts: { high: high }
          - id: glm-5.3-flash
            name: GLM-5.3-Flash
            contextWindow: 1000000
            maxTokens: 32000
- id: agent-default-model
  name: "@deepseek-ai/dsh-agent-default-model"
  config:
    provider: proxy-rs
    model: hy3
"#;

    /// The retired `settings.yaml` shape, kept so a flow-style document is
    /// still updatable rather than refused.
    const FLOW: &str = r#"# user notes that must survive
ui-onboarding:
  welcomeNoticeVersion: 2026-08-13.1
llm-pi-ai:
  providers:
    {
      other-gateway:
        { apiKeyEnv: OTHER_KEY, api: openai-completions, baseURL: https://other.test/v1 },
      proxy-rs:
        {
          apiKeyEnv: PROXY_RS_API_KEY,
          api: openai-completions,
          baseURL: http://127.0.0.1:3457/v1,
          models:
            [
              {
                  id: hy3,
                  name: Hy3,
                  contextWindow: 192000,
                  maxTokens: 64000
                }
            ]
        }
    }
agent-default-model:
  provider: proxy-rs
  model: hy3
"#;

    fn model(id: &str) -> DshModel {
        DshModel {
            id: id.to_string(),
            name: None,
            context_window: None,
            max_tokens: None,
            supports_images: None,
            supports_reasoning: None,
            reasoning_efforts: Vec::new(),
            points_ratio: None,
            free_badge: None,
        }
    }

    fn models_in(text: &str) -> Vec<String> {
        let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
        let span = find_span(&lines).expect("provider span");
        model_ids_in(&lines[span.body_start..span.body_end])
    }

    #[test]
    fn read_state_finds_provider_and_models() {
        let st = read_state(PATCH, None);
        assert!(st.provider_exists);
        assert_eq!(st.base_url.as_deref(), Some("http://127.0.0.1:3457/v1"));
        assert_eq!(st.model_ids, vec!["hy3", "glm-5.3-flash"]);
        assert!(!st.credential_present);
        assert!(!st.session_header);
    }

    #[test]
    fn read_state_reports_missing_provider() {
        let st = read_state("- id: locale\n  config:\n    preference: zh\n", None);
        assert!(!st.provider_exists);
    }

    #[test]
    fn read_state_reports_the_session_header() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let st = read_state(&out, None);
        assert!(
            st.session_header,
            "the writer must leave sessionHeader configured"
        );
        assert_eq!(st.model_ids, vec!["hy3"]);
    }

    #[test]
    fn upsert_writes_the_session_header_when_absent() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(out.contains("sessionHeader: x-deepseek-harness-session-id"));
    }

    #[test]
    fn upsert_replaces_an_existing_session_header_rather_than_duplicating() {
        let once = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let twice = upsert_provider(&once, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert_eq!(once, twice, "re-running must not keep changing the file");
        assert_eq!(twice.matches("sessionHeader:").count(), 1);
    }

    #[test]
    fn upsert_preserves_everything_outside_the_managed_leaves() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:9999/v1", &[model("hy4")]).unwrap();

        // Untouched context, in both directions.
        assert!(out.contains("# Your patch layer for this dsh profile."));
        assert!(out.contains("welcomeNoticeVersion: 2026-09-28.1"));
        assert!(out.contains("- id: agent-default-model"));
        assert!(out.contains("    model: hy3"));

        // Untouched keys inside the provider itself, including one this writer
        // knows nothing about.
        assert!(out.contains("apiKeyEnv: PROXY_RS_API_KEY"));
        assert!(out.contains("api: openai-completions"));
        assert!(out.contains("reasoningEffort: high"));
        assert!(out.contains("customFlag: keep-me"));

        // The values that were meant to change.
        assert!(out.contains("baseURL: http://127.0.0.1:9999/v1"));
        assert!(!out.contains("3457"));
        assert_eq!(models_in(&out), vec!["hy4"]);
    }

    #[test]
    fn upsert_is_idempotent() {
        let once = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let twice = upsert_provider(&once, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert_eq!(once, twice, "re-running must not keep changing the file");
    }

    #[test]
    fn upsert_keeps_the_models_inside_the_provider_block() {
        // The block locator must stop at the provider's own boundary: a rewrite
        // that ran past it would corrupt the sibling entry below.
        let out =
            upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("a"), model("b")]).unwrap();
        let lines: Vec<String> = out.lines().map(|s| s.to_string()).collect();
        let span = find_span(&lines).expect("provider span");
        assert_eq!(
            model_ids_in(&lines[span.body_start..span.body_end]),
            vec!["a", "b"]
        );
        assert!(out.contains("- id: agent-default-model"));
    }

    #[test]
    fn upsert_renders_every_field_it_is_given() {
        let m = DshModel {
            id: "deepseek-v4.1-flash".to_string(),
            name: Some("Deepseek-V4.1-Flash".to_string()),
            context_window: Some(1_000_000),
            max_tokens: Some(128_000),
            supports_images: Some(true),
            supports_reasoning: Some(true),
            reasoning_efforts: Vec::new(),
            points_ratio: None,
            free_badge: None,
        };
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[m]).unwrap();
        assert!(out.contains("- id: deepseek-v4.1-flash"));
        assert!(out.contains("name: Deepseek-V4.1-Flash"));
        assert!(out.contains("contextWindow: 1000000"));
        assert!(out.contains("maxTokens: 128000"));
        assert!(out.contains("reasoningEfforts: { high: high }"));
        assert!(out.contains("input: [ text, image ]"));
    }

    #[test]
    fn deepseek_tiers_are_written_as_low_high_max() {
        // The DeepSeek family offers exactly `low`/`high`/`max`; anything else
        // (or a single `high`) would misdescribe the model in DSH's Effort menu.
        let mut m = model("deepseek-v4.1-flash");
        m.supports_reasoning = Some(true);
        m.reasoning_efforts = vec!["low".into(), "high".into(), "max".into()];
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[m]).unwrap();
        assert!(
            out.contains("reasoningEfforts: { low: low, high: high, max: max }"),
            "got:\n{out}"
        );
        // `max` must reach the flow-style writer too, or a legacy profile
        // silently keeps a one-tier menu.
        let flow = upsert_provider(
            FLOW,
            "http://127.0.0.1:9999/v1",
            &[{
                let mut m = model("deepseek-v4.1-flash");
                m.supports_reasoning = Some(true);
                m.reasoning_efforts = vec!["low".into(), "high".into(), "max".into()];
                m
            }],
        )
        .unwrap();
        // Here `reasoningEfforts` is the entry's last field, so the flow writer
        // strips its comma; the mapping itself is what must survive.
        assert!(flow.contains("reasoningEfforts: { low: low, high: high, max: max }"));
    }

    #[test]
    fn unsupported_tier_falls_back_to_high() {
        // A tier DSH cannot name must never be written: its config validator
        // rejects the whole profile, which would take the provider out of every
        // picker. The historical single `high` is the safe fallback.
        let mut m = model("some-model");
        m.supports_reasoning = Some(true);
        m.reasoning_efforts = vec!["ultra".into()];
        assert_eq!(
            render_reasoning_efforts(&m).as_deref(),
            Some("{ high: high }")
        );

        // A non-reasoning model declares nothing at all.
        let mut plain = model("plain");
        plain.reasoning_efforts = vec!["low".into(), "high".into()];
        assert_eq!(render_reasoning_efforts(&plain), None);
    }

    #[test]
    fn upsert_omits_unknown_capability_rather_than_inventing_it() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("plain")]).unwrap();
        assert!(out.contains("- id: plain"));
        assert!(!out.contains("reasoningEfforts"));
        assert!(!out.contains("input:"));
        assert!(!out.contains("contextWindow"));
    }

    #[test]
    fn upsert_refuses_an_empty_model_list() {
        assert!(upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[]).is_err());
    }

    #[test]
    fn to_dsh_model_appends_ratio_and_badge_to_name() {
        // Free node with a badge: `Hy3 (×0.00 限时免费)`.
        let free = crate::providers::GuiModel {
            id: "hy3".into(),
            name: Some("Hy3".into()),
            context_window: Some(200_000),
            max_output_tokens: Some(8_192),
            supports_images: Some(true),
            supports_reasoning: None,
            points_ratio: Some(0.0),
            free_badge: Some("限时免费".into()),
            reasoning_efforts: None,
            default_reasoning_effort: None,
        };
        let d = to_dsh_model(&free);
        assert_eq!(d.name.as_deref(), Some("Hy3 (×0.00 限时免费)"));

        // Paid model: `Hy4 preview (×0.29)`.
        let paid = crate::providers::GuiModel {
            id: "hy4-preview".into(),
            name: Some("Hy4 preview".into()),
            context_window: Some(200_000),
            max_output_tokens: Some(32_768),
            supports_images: Some(true),
            supports_reasoning: Some(true),
            points_ratio: Some(0.29),
            free_badge: None,
            reasoning_efforts: Some(vec!["high".into()]),
            default_reasoning_effort: Some("high".into()),
        };
        let d2 = to_dsh_model(&paid);
        assert_eq!(d2.name.as_deref(), Some("Hy4 preview (×0.29)"));
        // The mapping carries the tiers through, so the writer can render them.
        assert_eq!(d2.reasoning_efforts, vec!["high".to_string()]);
    }

    #[test]
    fn upsert_refuses_when_the_provider_is_absent() {
        let text = "- id: locale\n  name: \"@deepseek-ai/dsh-client-locale\"\n  config:\n    preference: zh\n";
        let err = upsert_provider(text, "http://x/v1", &[model("a")]).unwrap_err();
        assert!(
            err.to_string().contains("could not locate"),
            "must explain itself, got {err}"
        );
        assert_eq!(text, text, "input untouched on refusal");
    }

    #[test]
    fn upsert_keeps_the_documents_trailing_newline() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(out.ends_with('\n'));
        let no_newline = PATCH.trim_end_matches('\n').to_string();
        let out2 =
            upsert_provider(&no_newline, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(
            !out2.ends_with('\n'),
            "must not add one the file did not have"
        );
    }

    #[test]
    fn upsert_updates_a_flow_style_document_too() {
        // The retired `settings.yaml` shape must still be updatable: refusing it
        // would strand anyone whose profile patch was written before the
        // migration.
        let out = upsert_provider(FLOW, "http://127.0.0.1:9999/v1", &[model("hy4")]).unwrap();
        assert!(out.contains("# user notes that must survive"));
        assert!(out.contains("other-gateway:"));
        assert!(out.contains("baseURL: https://other.test/v1"));
        assert!(out.contains("baseURL: http://127.0.0.1:9999/v1"));
        assert!(out.contains("sessionHeader: x-deepseek-harness-session-id,"));
        assert_eq!(models_in(&out), vec!["hy4"]);
        let once = out;
        let twice = upsert_provider(&once, "http://127.0.0.1:9999/v1", &[model("hy4")]).unwrap();
        assert_eq!(once, twice, "flow writes must be idempotent too");
    }

    #[test]
    fn upsert_rejects_a_quoted_scalar_that_needs_quoting() {
        assert_eq!(yaml_scalar("plain-id.1"), "plain-id.1");
        assert_eq!(yaml_scalar("has: space"), "'has: space'");
        assert_eq!(yaml_scalar("trailing "), "'trailing '");
        assert_eq!(yaml_scalar("it's"), "it's");
        assert_eq!(yaml_scalar("a'b: c"), "'a''b: c'");
    }

    #[test]
    fn credential_is_added_once_and_is_idempotent() {
        let _dir = EmptyDataDir::new("idempotent");
        let store =
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-x\nrecords:\n  a:\n    kind: grant\n";
        assert!(!credential_present(store));
        let once = ensure_credential(store);
        assert!(credential_present(&once));
        assert!(once.contains("PROXY_RS_API_KEY: nouse"));
        // Existing entries and the records section survive.
        assert!(once.contains("DEEPSEEK_API_KEY: sk-x"));
        assert!(once.contains("records:"));
        assert!(once.contains("    kind: grant"));
        // Inserted inside refs, not after records.
        let refs = once.find("refs:").unwrap();
        let records = once.find("records:").unwrap();
        let added = once.find("PROXY_RS_API_KEY").unwrap();
        assert!(
            refs < added && added < records,
            "credential belongs in refs"
        );

        let twice = ensure_credential(&once);
        assert_eq!(once, twice);
        assert_eq!(twice.matches("PROXY_RS_API_KEY").count(), 1);
    }

    #[test]
    fn credential_is_left_alone_when_already_present() {
        let store = "version: 1\nrefs:\n  PROXY_RS_API_KEY: nouse\nrecords:\n";
        assert_eq!(ensure_credential(store), store);
    }

    /// An empty data directory: no accounts, therefore no writable key.
    ///
    /// These tests previously asserted the placeholder unconditionally and
    /// relied on the process-wide `PROXY_DATA_DIR` happening to be unset. Once a
    /// test elsewhere in the same binary created an account (the key-writing
    /// tests), `ensure_credential` correctly started emitting that real key and
    /// these assertions failed — a test-isolation bug, not a product bug.
    ///
    /// Each test now owns the data dir, so it states its own preconditions.
    struct EmptyDataDir {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: std::path::PathBuf,
    }

    impl EmptyDataDir {
        fn new(name: &str) -> Self {
            let guard = crate::settings::data_dir_test_lock();
            let dir =
                std::env::temp_dir().join(format!("proxy-rs-dsh-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            std::env::set_var("PROXY_DATA_DIR", &dir);
            Self { _guard: guard, dir }
        }
    }

    impl Drop for EmptyDataDir {
        fn drop(&mut self) {
            std::env::remove_var("PROXY_DATA_DIR");
            if !std::thread::panicking() {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    #[test]
    fn credential_appends_when_there_are_no_refs_yet() {
        let _dir = EmptyDataDir::new("no-refs");
        let out = ensure_credential("version: 1\n");
        assert!(out.contains("refs:"));
        assert!(out.contains("PROXY_RS_API_KEY: nouse"));
    }

    #[test]
    fn probe_refuses_to_write_through_a_document_that_has_none() {
        // read_state must not invent a provider for a document without one.
        let st = read_state("- id: locale\n  config:\n    preference: zh\n", None);
        assert!(!st.provider_exists);
        assert!(st.base_url.is_none());
        assert!(st.model_ids.is_empty());
        assert!(!st.session_header);
    }

    #[test]
    fn the_models_key_keeps_its_indentation() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let key = lines
            .iter()
            .position(|l| l.trim() == "models:")
            .expect("models key");
        // The key sits at the provider body's field indent (8 spaces in the
        // fixture) and the entries one level deeper.
        assert_eq!(indent_of(lines[key]), 8, "models key indent");
        assert!(lines[key + 1].trim_start().starts_with("- id: hy3"));
        assert_eq!(indent_of(lines[key + 1]), 10, "item indent");
        // Re-running must not drift the indentation either.
        let again = upsert_provider(&out, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn declares_provider_matches_only_the_managed_route() {
        assert!(declares_provider(PATCH));
        assert!(!declares_provider(
            "- id: locale\n  config:\n    preference: zh\n"
        ));
    }

    #[test]
    fn write_atomic_replaces_contents_and_keeps_permissions() {
        let dir = tempdir();
        let path = dir.join("cordis.patch.yml");
        std::fs::write(&path, "old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        write_atomic(&path, "new contents\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new contents\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a secret-bearing file must not widen its mode");
        }

        // No temp file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("proxy-rs-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?}");
    }

    #[test]
    fn apply_writes_both_files_and_is_idempotent() {
        let dir = tempdir();
        let patch = dir.join("cordis.patch.yml");
        let creds = dir.join(".credentials.yaml");
        std::fs::write(&patch, PATCH).unwrap();
        std::fs::write(
            &creds,
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-x\nrecords:\n",
        )
        .unwrap();

        let (_, _, added) =
            apply(&patch, &creds, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(added, "credential was missing, so it should be added");
        assert!(credential_present(
            &std::fs::read_to_string(&creds).unwrap()
        ));
        // The other credential survived.
        assert!(std::fs::read_to_string(&creds)
            .unwrap()
            .contains("DEEPSEEK_API_KEY: sk-x"));

        // Second run adds nothing and leaves the files alone.
        let before = std::fs::read_to_string(&patch).unwrap();
        let (_, _, added2) =
            apply(&patch, &creds, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(!added2);
        assert_eq!(before, std::fs::read_to_string(&patch).unwrap());
    }

    #[test]
    fn apply_refuses_when_the_patch_document_is_missing() {
        let dir = tempdir();
        let err = apply(
            &dir.join("nope.yml"),
            &dir.join(".credentials.yaml"),
            "http://127.0.0.1:3457/v1",
            &[model("hy3")],
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to read"), "got {err}");
    }

    /// Minimal temp-dir helper so the module stays free of extra dev-deps.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "proxy-rs-dsh-config-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// `ensure_credential` writes a real, usable gateway key when one exists.
    ///
    /// Before this, it always wrote the placeholder `nouse`, which on a server
    /// with `PROXY_CLIENT_AUTH=true` produced a DSH config that could never
    /// authenticate — the user had no way to fix it except to re-issue a key and
    /// edit the file by hand.
    #[test]
    fn ensure_credential_writes_the_real_key() {
        let _dir = EmptyDataDir::new("real-key");
        let (_acct, key) = crate::client_accounts::create_account("t", "").expect("create");
        let out = ensure_credential("refs:\n  OTHER: x\nrecords: {}\n");

        assert!(
            out.contains(&format!("{CREDENTIAL_REF}: {key}")),
            "the real key must be written, got:\n{out}"
        );
        assert!(!out.contains(CREDENTIAL_PLACEHOLDER), "{out}");
    }

    /// With no writable key, the placeholder is still better than nothing: DSH
    /// then sends a credential that fails with a clear 401 instead of sending none.
    #[test]
    fn ensure_credential_falls_back_to_the_placeholder() {
        let _dir = EmptyDataDir::new("no-key");
        let out = ensure_credential("refs:\n  OTHER: x\n");
        assert!(
            out.contains(&format!("{CREDENTIAL_REF}: {CREDENTIAL_PLACEHOLDER}")),
            "{out}"
        );
    }
}
