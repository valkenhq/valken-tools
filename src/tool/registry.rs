use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use super::{Access, Actor, Cx, Tier, Tool, ToolError, ToolOutput, ToolSpec};
use crate::audit::{AuditEntry, AuditLog};
use crate::redact::Redactor;

/// Longest a tool may run: a wedged API server must not hold an MCP
/// session (which handles one call at a time) forever.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest audit TARGET / RESULT text; both can carry client-chosen strings.
const AUDIT_FIELD_CHARS: usize = 512;

/// What a model or MCP client is told about one tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema (an `object`) for the input.
    pub schema: Arc<Value>,
    pub access: Access,
    pub tier: Tier,
}

/// The outcome of one [`Registry::call`].
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub tool: String,
    pub target: String,
    /// Redacted output, or the error to hand back to the model.
    pub output: Result<ToolOutput, ToolError>,
    /// How many values redaction masked in the body.
    pub masked: usize,
    pub elapsed: Duration,
}

impl Call {
    /// The audit RESULT / tool-trail detail: `40 lines · 2 masked`, `ok`, or
    /// the error.
    pub fn summary(&self) -> String {
        let base = match &self.output {
            Ok(out) => out.note.clone().unwrap_or_else(|| "ok".to_owned()),
            Err(e) => e.to_string(),
        };
        match self.masked {
            0 => base,
            n => format!("{base} · {n} masked"),
        }
    }

    /// The text the model sees as the tool result.
    pub fn model_text(&self) -> String {
        match &self.output {
            Ok(out) => out.body.clone(),
            Err(e) => e.to_string(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.output.is_err()
    }
}

#[async_trait]
trait Erased: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn def(&self) -> &ToolDef;
    /// Parses and runs; returns the audit target alongside the result.
    async fn call(&self, cx: Cx<'_>, input: Value) -> (String, Result<ToolOutput, ToolError>);
}

struct Typed<T: Tool> {
    tool: T,
    def: ToolDef,
}

#[async_trait]
impl<T: Tool> Erased for Typed<T> {
    fn spec(&self) -> ToolSpec {
        self.tool.spec()
    }

    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn call(&self, cx: Cx<'_>, input: Value) -> (String, Result<ToolOutput, ToolError>) {
        // Models sometimes send `null` for a tool without parameters.
        let input = if input.is_null() {
            Value::Object(Default::default())
        } else {
            input
        };
        match serde_json::from_value::<T::Input>(input) {
            Ok(parsed) => {
                let target = self.tool.target(&parsed);
                (target, self.tool.run(cx, parsed).await)
            }
            Err(e) => ("—".to_owned(), Err(ToolError::Input(e.to_string()))),
        }
    }
}

/// The tool registry.  Build it once, share it as `Arc<Registry>`.
pub struct Registry {
    tools: Vec<Box<dyn Erased>>,
    index: HashMap<&'static str, usize>,
    redactor: RwLock<Arc<Redactor>>,
    audit: Option<Arc<AuditLog>>,
}

impl Registry {
    pub fn new(redactor: Redactor, audit: Option<Arc<AuditLog>>) -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            redactor: RwLock::new(Arc::new(redactor)),
            audit,
        }
    }

    /// Adds a tool.  Panics on a duplicate name: that's a programming error.
    pub fn register<T: Tool>(&mut self, tool: T) -> &mut Self {
        let spec = tool.spec();
        assert!(
            spec.name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_'),
            "tool name `{}` must be [a-z_]",
            spec.name
        );
        let def = ToolDef {
            name: spec.name,
            description: spec.description,
            schema: Arc::new(input_schema::<T::Input>()),
            access: spec.access,
            tier: spec.tier,
        };
        let prev = self.index.insert(spec.name, self.tools.len());
        assert!(prev.is_none(), "tool `{}` registered twice", spec.name);
        self.tools.push(Box::new(Typed { tool, def }));
        self
    }

    /// The redaction rules in force.  Settings changes swap them live.
    pub fn redactor(&self) -> Arc<Redactor> {
        self.redactor.read().expect("redactor lock").clone()
    }

    pub fn set_redactor(&self, redactor: Redactor) {
        *self.redactor.write().expect("redactor lock") = Arc::new(redactor);
    }

    pub fn audit(&self) -> Option<&Arc<AuditLog>> {
        self.audit.as_ref()
    }

    /// Tools `actor` may see.  PRO tools are listed only when entitled, so a
    /// model never plans around a tool it can't call.
    pub fn defs_for<'a>(
        &'a self,
        actor: &'a Actor,
        pro: bool,
    ) -> impl Iterator<Item = &'a ToolDef> + 'a {
        self.tools
            .iter()
            .map(|t| t.def())
            .filter(move |d| d.access.allows(actor) && (pro || d.tier == Tier::Free))
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Runs one tool call with every rule applied, and audits it.
    #[tracing::instrument(name = "tools.call", skip(self, input), fields(actor = %actor))]
    pub async fn call(&self, actor: &Actor, pro: bool, name: &str, input: Value) -> Call {
        let started = Instant::now();
        let redactor = self.redactor();
        let (target, output) = match self.index.get(name).map(|&i| &self.tools[i]) {
            None => ("—".to_owned(), Err(ToolError::Unknown(name.to_owned()))),
            Some(tool) => {
                let spec = tool.spec();
                if !spec.access.allows(actor) {
                    (
                        "—".to_owned(),
                        Err(ToolError::Denied {
                            actor: actor.to_string(),
                            tool: spec.name,
                        }),
                    )
                } else if spec.tier == Tier::Pro && !pro {
                    ("—".to_owned(), Err(ToolError::NeedsPro(spec.name)))
                } else {
                    match tokio::time::timeout(
                        CALL_TIMEOUT,
                        tool.call(
                            Cx {
                                actor,
                                pro,
                                redactor: &redactor,
                            },
                            input,
                        ),
                    )
                    .await
                    {
                        Ok(done) => done,
                        Err(_) => (
                            "—".to_owned(),
                            Err(ToolError::Failed(format!(
                                "`{name}` timed out after {}s",
                                CALL_TIMEOUT.as_secs()
                            ))),
                        ),
                    }
                }
            }
        };
        let (output, masked) = match output {
            Ok(mut out) => {
                let (body, masked) = redactor.text(&out.body);
                out.body = body.into_owned();
                (Ok(out), masked)
            }
            Err(e) => {
                let (e, masked) = e.redacted(&redactor);
                (Err(e), masked)
            }
        };
        let call = Call {
            tool: name.to_owned(),
            target,
            output,
            masked,
            elapsed: started.elapsed(),
        };
        if let Some(audit) = &self.audit {
            let entry = AuditEntry::now(
                actor.to_string(),
                call.tool.clone(),
                clip(&call.target),
                clip(&call.summary()),
            );
            if let Err(e) = audit.append(&entry) {
                tracing::warn!(error = %e, "audit append failed");
            }
        }
        call
    }
}

fn clip(s: &str) -> String {
    match s.char_indices().nth(AUDIT_FIELD_CHARS) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// The input schema as providers want it: a bare `object` schema with
/// sub-schemas inlined and no `$schema` / `title` noise.
fn input_schema<T: schemars::JsonSchema>() -> Value {
    let generator = schemars::generate::SchemaSettings::draft07()
        .with(|s| s.inline_subschemas = true)
        .into_generator();
    let mut schema = generator.into_root_schema_for::<T>().to_value();
    if let Value::Object(map) = &mut schema {
        map.remove("$schema");
        map.remove("title");
        map.remove("definitions");
        map.entry("properties")
            .or_insert_with(|| Value::Object(Default::default()));
        map.insert("type".to_owned(), Value::String("object".to_owned()));
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjectRef;
    use crate::redact::Rules;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct EchoIn {
        /// What to echo.
        text: String,
        #[serde(default)]
        times: Option<u8>,
    }

    struct Echo(Access, Tier);

    #[async_trait]
    impl Tool for Echo {
        type Input = EchoIn;
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo",
                description: "Echo text.",
                access: self.0,
                tier: self.1,
            }
        }
        fn target(&self, input: &EchoIn) -> String {
            input.text.clone()
        }
        async fn run(&self, _cx: Cx<'_>, input: EchoIn) -> Result<ToolOutput, ToolError> {
            Ok(
                ToolOutput::text(input.text.repeat(input.times.unwrap_or(1).into()))
                    .cite(ObjectRef::new("c", "pod", Some("ns"), "p"))
                    .note("echoed"),
            )
        }
    }

    #[derive(Deserialize, JsonSchema)]
    struct NoIn {}

    struct Ping;

    #[async_trait]
    impl Tool for Ping {
        type Input = NoIn;
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "ping",
                description: "Ping.",
                access: Access::Read,
                tier: Tier::Free,
            }
        }
        fn target(&self, _: &NoIn) -> String {
            "—".into()
        }
        async fn run(&self, _cx: Cx<'_>, _: NoIn) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("pong"))
        }
    }

    fn registry(access: Access, tier: Tier) -> Registry {
        let mut r = Registry::new(Redactor::new(Rules::default()), None);
        r.register(Echo(access, tier)).register(Ping);
        r
    }

    #[tokio::test]
    async fn runs_typed_tool() {
        let r = registry(Access::Read, Tier::Free);
        let call = r
            .call(
                &Actor::Agent,
                false,
                "echo",
                serde_json::json!({"text": "ab", "times": 2}),
            )
            .await;
        assert_eq!(call.output.as_ref().unwrap().body, "abab");
        assert_eq!(call.target, "ab");
        assert_eq!(call.summary(), "echoed");
    }

    #[tokio::test]
    async fn null_input_means_empty_object() {
        let r = registry(Access::Read, Tier::Free);
        let call = r.call(&Actor::Agent, false, "ping", Value::Null).await;
        assert_eq!(call.model_text(), "pong");
    }

    #[tokio::test]
    async fn rejects_unknown_fields_and_tools() {
        let r = registry(Access::Read, Tier::Free);
        let call = r
            .call(
                &Actor::Agent,
                false,
                "echo",
                serde_json::json!({"text": "a", "rm": true}),
            )
            .await;
        assert!(matches!(call.output, Err(ToolError::Input(_))));
        let call = r.call(&Actor::Agent, false, "nope", Value::Null).await;
        assert_eq!(call.output, Err(ToolError::Unknown("nope".into())));
    }

    #[tokio::test]
    async fn gates_tier_and_access() {
        let r = registry(Access::Propose, Tier::Pro);
        let input = serde_json::json!({"text": "a"});
        let call = r.call(&Actor::Agent, false, "echo", input.clone()).await;
        assert_eq!(call.output, Err(ToolError::NeedsPro("echo")));
        let call = r.call(&Actor::Watch, true, "echo", input.clone()).await;
        assert!(matches!(call.output, Err(ToolError::Denied { .. })));
        assert!(r
            .call(&Actor::Mcp("cursor".into()), true, "echo", input)
            .await
            .output
            .is_ok());
    }

    #[tokio::test]
    async fn redacts_bodies_and_counts() {
        let r = registry(Access::Read, Tier::Free);
        let call = r
            .call(
                &Actor::Agent,
                false,
                "echo",
                serde_json::json!({"text": "auth: Bearer abcdefghijklmnop"}),
            )
            .await;
        let body = &call.output.as_ref().unwrap().body;
        assert!(!body.contains("abcdefghijklmnop"), "{body}");
        assert_eq!(call.masked, 1);
        assert_eq!(call.summary(), "echoed · 1 masked");
    }

    #[tokio::test]
    async fn error_text_is_redacted_too() {
        let r = registry(Access::Read, Tier::Free);
        // An unknown field error quotes the model's input, which here holds a token.
        let call = r
            .call(
                &Actor::Agent,
                false,
                "echo",
                serde_json::json!({"text": "a", "Bearer abcdefghijklmnop": 1}),
            )
            .await;
        let Err(e) = &call.output else {
            panic!("{call:?}")
        };
        assert!(matches!(e, ToolError::Input(_)));
        assert!(!e.to_string().contains("abcdefghijklmnop"), "{e}");
        assert_eq!(call.masked, 1);
        assert_eq!(
            clip(&"x".repeat(600)).chars().count(),
            AUDIT_FIELD_CHARS + 1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn slow_tools_time_out() {
        struct Sleepy;
        #[async_trait]
        impl Tool for Sleepy {
            type Input = NoIn;
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "sleepy",
                    description: "Sleeps.",
                    access: Access::Read,
                    tier: Tier::Free,
                }
            }
            fn target(&self, _: &NoIn) -> String {
                "—".into()
            }
            async fn run(&self, _cx: Cx<'_>, _: NoIn) -> Result<ToolOutput, ToolError> {
                tokio::time::sleep(CALL_TIMEOUT * 2).await;
                Ok(ToolOutput::text("late"))
            }
        }
        let mut r = Registry::new(Redactor::new(Rules::default()), None);
        r.register(Sleepy);
        let call = r.call(&Actor::Agent, false, "sleepy", Value::Null).await;
        assert!(
            matches!(&call.output, Err(ToolError::Failed(m)) if m.contains("timed out")),
            "{call:?}"
        );
    }

    #[test]
    fn defs_hide_pro_and_disallowed() {
        let r = registry(Access::Ui, Tier::Pro);
        let names = |actor: &Actor, pro| r.defs_for(actor, pro).map(|d| d.name).collect::<Vec<_>>();
        assert_eq!(names(&Actor::Agent, false), ["ping"]);
        assert_eq!(names(&Actor::Agent, true), ["echo", "ping"]);
        assert_eq!(names(&Actor::Watch, true), ["ping"]);
    }

    #[test]
    fn schema_is_a_bare_object() {
        let r = registry(Access::Read, Tier::Free);
        let echo = r
            .defs_for(&Actor::Agent, true)
            .find(|d| d.name == "echo")
            .unwrap();
        assert_eq!(echo.schema["type"], "object");
        assert!(echo.schema.get("$schema").is_none());
        assert_eq!(echo.schema["required"], serde_json::json!(["text"]));
        assert_eq!(
            echo.schema["properties"]["text"]["description"],
            "What to echo."
        );
        let ping = r
            .defs_for(&Actor::Agent, true)
            .find(|d| d.name == "ping")
            .unwrap();
        assert!(ping.schema["properties"].is_object());
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_names_panic() {
        let mut r = Registry::new(Redactor::new(Rules::default()), None);
        r.register(Ping).register(Ping);
    }

    #[tokio::test]
    async fn audits_every_call() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Arc::new(AuditLog::open(dir.path()).unwrap());
        let mut r = Registry::new(Redactor::new(Rules::default()), Some(audit.clone()));
        r.register(Ping);
        r.call(&Actor::Mcp("cursor".into()), false, "ping", Value::Null)
            .await;
        r.call(&Actor::Agent, false, "missing", Value::Null).await;
        let rows = audit.read(&Default::default(), 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action, "missing");
        assert_eq!(rows[1].actor, "cursor");
        assert_eq!(rows[1].result, "ok");
    }
}
