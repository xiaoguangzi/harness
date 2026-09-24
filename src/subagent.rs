//! Subagents: the main agent delegates a self-contained task to a fresh
//! agent with its own conversation, system prompt and tool set, and gets
//! back only the final answer. This keeps long searches out of the main
//! context, and several subagents can run in parallel.
//!
//! The built-in `explore` agent only has the `grep` tool. Custom agents are
//! Markdown files in `.harness/agents/` or `.claude/agents/` (project or `~`):
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: Reviews the current diff for bugs. Use after making changes.
//! tools: read, grep, bash        # optional; default: every tool allowed
//! permission: read-only          # default; `write` allows modifying tools
//! model: claude-sonnet-5         # optional; default: HARNESS_SUBAGENT_MODEL or the main model
//! ---
//! You are a code reviewer...
//! ```
//!
//! Permissions default to read-only: such an agent only gets tools marked
//! read-only (`read`, `grep`, `skill`, MCP tools with `readOnlyHint`), so no
//! `bash` or `write` unless it declares `permission: write`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::{Agent, Observer};
use crate::api::Client;
use crate::skills::{parse_frontmatter, split_frontmatter};
use crate::tools::{
    FileTracker, ReadTool, Tool, ToolOutput, ToolRegistry, WriteTool, parse_input, truncate,
};

const MAX_RESULT_BYTES: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Permission {
    ReadOnly,
    Write,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub permission: Permission,
    /// Tool names (a trailing `*` matches a prefix); `None` allows all tools
    /// the permission grants.
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    /// Where the definition came from; `None` for built-ins.
    pub path: Option<PathBuf>,
}

impl AgentDef {
    fn allows(&self, tool: &str) -> bool {
        match &self.tools {
            None => true,
            Some(names) => names.iter().any(|n| match n.strip_suffix('*') {
                Some(prefix) => tool.starts_with(prefix),
                None => n == tool,
            }),
        }
    }
}

pub fn explore() -> AgentDef {
    AgentDef {
        name: "explore".into(),
        description: "Codebase search agent with only the `grep` tool. Use it for open-ended questions \
                      that need many searches (where is X implemented, which files use Y, how is Z wired \
                      up) when you only need the conclusion, not the search output. It cannot modify \
                      anything."
            .into(),
        prompt: "You are an exploration agent: a code search specialist working for another agent. You \
                 get one question about the codebase; answer it by searching.\n\n\
                 Your only tool is `grep`:\n\
                 - Find candidate files with the default `files_with_matches` mode, narrowing with \
                 `glob` and `path`.\n\
                 - Then look at the code with `output_mode: \"content\"` and `context` lines around a \
                 match (for example the definition of a function), rather than broad patterns that \
                 return everything.\n\
                 - Run independent searches in parallel, and stop once you can answer.\n\n\
                 Your final message is the only thing the caller sees. Make it a concise report: the \
                 answer first, then the key locations as `path:line` with a phrase on each, then anything \
                 you could not determine. Quote code only when a few lines are essential."
            .into(),
        permission: Permission::ReadOnly,
        tools: Some(vec!["grep".into()]),
        model: None,
        path: None,
    }
}

/// Default search roots, highest precedence first.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(".harness/agents"), PathBuf::from(".claude/agents")];
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        roots.push(home.join(".harness/agents"));
        roots.push(home.join(".claude/agents"));
    }
    roots
}

/// The built-in agents plus `<root>/*.md` definitions. On a name clash the
/// earlier root wins, and any file overrides a built-in.
pub fn discover(roots: &[PathBuf]) -> (Vec<AgentDef>, Vec<String>) {
    let mut defs: BTreeMap<String, AgentDef> = BTreeMap::new();
    let mut warnings = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        let mut files: Vec<PathBuf> =
            entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect();
        files.sort();
        for file in files {
            let parsed = std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|t| parse_def(&t, &file));
            match parsed {
                Ok(def) => {
                    defs.entry(def.name.clone()).or_insert(def);
                }
                Err(e) => warnings.push(format!("{}: {e}", file.display())),
            }
        }
    }
    let builtin = explore();
    defs.entry(builtin.name.clone()).or_insert(builtin);
    (defs.into_values().collect(), warnings)
}

fn parse_def(text: &str, file: &Path) -> Result<AgentDef, String> {
    let (front, body) = split_frontmatter(text).ok_or("missing `---` frontmatter")?;
    let fields = parse_frontmatter(front);
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = fields.get("name").cloned().unwrap_or(stem);
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("invalid agent name `{name}`"));
    }
    let description = fields.get("description").cloned().filter(|d| !d.is_empty()).ok_or("missing `description`")?;
    let permission = match fields.get("permission").map(|p| p.to_ascii_lowercase()).as_deref() {
        None | Some("read-only" | "readonly" | "read_only") => Permission::ReadOnly,
        Some("write") => Permission::Write,
        Some(other) => return Err(format!("unknown permission `{other}` (use `read-only` or `write`)")),
    };
    // Accepts `read, grep`, `[read, grep]` and YAML lists (joined by the
    // frontmatter parser into `- read - grep`). Claude Code style names
    // such as `Read` are lowercased; MCP names keep their case.
    let tools = fields.get("tools").map(|t| {
        t.split(|c: char| c == ',' || c.is_whitespace() || c == '[' || c == ']')
            .filter(|n| !n.is_empty() && *n != "-")
            .map(|n| if n.starts_with("mcp__") { n.to_string() } else { n.to_ascii_lowercase() })
            .collect()
    });
    let model = fields.get("model").cloned().filter(|m| !m.is_empty() && m != "inherit");
    let prompt = body.trim().to_string();
    if prompt.is_empty() {
        return Err("empty system prompt".into());
    }
    let path = std::path::absolute(file).ok();
    Ok(AgentDef { name, description, prompt, permission, tools, model, path })
}

/// Called for each tool call a subagent makes: `(agent label, tool, input)`.
pub type Reporter = Arc<dyn Fn(&str, &str, &Value) + Send + Sync>;

/// The `agent` tool.
pub struct AgentTool {
    defs: Arc<Vec<AgentDef>>,
    /// Tools subagents are chosen from. Must not contain this tool, so
    /// subagents cannot spawn subagents.
    base: ToolRegistry,
    client: Client,
    default_model: Option<String>,
    reporter: Reporter,
    description: String,
}

impl AgentTool {
    pub fn new(defs: Arc<Vec<AgentDef>>, base: ToolRegistry, client: Client, reporter: Reporter) -> Self {
        let mut description = String::from(
            "Delegate a task to a subagent that works in its own separate context and returns only its \
             final report. Use it to keep long searches and file reading out of this conversation. \
             The subagent cannot see this conversation, so `prompt` must be self-contained: state the \
             goal, what you already know, and what the report should contain. For independent \
             questions, launch several subagents in parallel in one response. The report comes back \
             to you, not the user; relay what matters.\n\nAvailable agents:",
        );
        for d in defs.iter() {
            let access = match d.permission {
                Permission::ReadOnly => "read-only",
                Permission::Write => "can modify files",
            };
            description.push_str(&format!("\n- {} ({access}): {}", d.name, d.description));
        }
        let default_model = std::env::var("HARNESS_SUBAGENT_MODEL").ok().filter(|m| !m.is_empty());
        Self { defs, base, client, default_model, reporter, description }
    }

    /// Tool names in `defs` that no available tool matches.
    pub fn unknown_tools(&self) -> Vec<(String, String)> {
        let mut unknown = Vec::new();
        for def in self.defs.iter() {
            let registry = self.registry_for(&AgentDef { tools: None, permission: Permission::Write, ..def.clone() });
            for name in def.tools.iter().flatten() {
                let probe = AgentDef { tools: Some(vec![name.clone()]), ..def.clone() };
                if !registry.names().any(|t| probe.allows(t)) {
                    unknown.push((def.name.clone(), name.clone()));
                }
            }
        }
        unknown
    }

    /// Builds the tool set for one run. File tools get a fresh tracker, so
    /// a subagent's reads never count as reads by the main agent.
    pub fn registry_for(&self, def: &AgentDef) -> ToolRegistry {
        let files = FileTracker::default();
        let mut registry = self.base.clone();
        registry.register(ReadTool::new(files.clone()));
        registry.register(WriteTool::new(files));
        if def.permission == Permission::ReadOnly {
            registry.retain(|t| t.read_only());
        }
        registry.retain(|t| t.name() != "agent" && def.allows(t.name()));
        registry
    }

    fn system_prompt(def: &AgentDef) -> String {
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        format!(
            "{}\n\nThe working directory is {cwd}. You were started by another agent, which sees only \
             your final message; the user does not see it directly.",
            def.prompt
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    agent_type: String,
    description: String,
    prompt: String,
}

/// Forwards a subagent's tool calls to the reporter; its text is read from
/// the finished conversation instead of being streamed.
struct SubObserver {
    label: String,
    reporter: Reporter,
    tool_calls: usize,
}

impl Observer for SubObserver {
    fn on_text_delta(&mut self, _text: &str) {}
    fn on_text_end(&mut self) {}
    fn on_tool_call(&mut self, name: &str, input: &Value) {
        self.tool_calls += 1;
        (self.reporter)(&self.label, name, input);
    }
    fn on_tool_result(&mut self, _name: &str, _content: &str, _is_error: bool) {}
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        let names: Vec<&str> = self.defs.iter().map(|d| d.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "agent_type": {"type": "string", "enum": names, "description": "Which agent to run."},
                "description": {"type": "string", "description": "A 3-6 word summary of the task, shown to the user."},
                "prompt": {"type": "string", "description": "The complete, self-contained task for the subagent."}
            },
            "required": ["agent_type", "description", "prompt"],
            "additionalProperties": false
        })
    }

    /// Only as read-only as the agents it can start.
    fn read_only(&self) -> bool {
        self.defs.iter().all(|d| d.permission == Permission::ReadOnly)
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let Some(def) = self.defs.iter().find(|d| d.name == input.agent_type) else {
            return ToolOutput::error(format!("Unknown agent type: {}", input.agent_type));
        };
        let model = def.model.as_deref().or(self.default_model.as_deref()).unwrap_or(self.client.model());
        let mut agent = Agent::new(self.client.with_model(model), self.registry_for(def), Self::system_prompt(def));
        let mut observer = SubObserver {
            label: format!("{}: {}", def.name, input.description),
            reporter: self.reporter.clone(),
            tool_calls: 0,
        };

        let result = agent.run_turn(vec![json!({"type": "text", "text": input.prompt})], &mut observer).await;
        let report = truncate(agent.last_text().trim(), MAX_RESULT_BYTES);
        match result {
            Ok(()) if report.is_empty() => ToolOutput::error("The subagent finished without a report"),
            Ok(()) => ToolOutput::ok(report),
            Err(e) => ToolOutput::error(format!(
                "The subagent failed after {} tool calls: {e:#}{}",
                observer.tool_calls,
                if report.is_empty() { String::new() } else { format!("\n\nLast output:\n{report}") }
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Auth, Config};
    use crate::tools::GrepTool;

    fn client() -> Client {
        Client::new(Config {
            base_url: "http://127.0.0.1:9".into(),
            model: "main-model".into(),
            max_tokens: 1000,
            auth: Auth::ApiKey("test".into()),
        })
        .unwrap()
    }

    fn agent_tool(defs: Vec<AgentDef>) -> AgentTool {
        let base = ToolRegistry::with_defaults(FileTracker::default());
        AgentTool::new(Arc::new(defs), base, client(), Arc::new(|_: &str, _: &str, _: &Value| {}))
    }

    fn names(r: &ToolRegistry) -> Vec<&str> {
        r.names().collect()
    }

    #[test]
    fn explore_only_gets_grep() {
        let tool = agent_tool(vec![explore()]);
        assert_eq!(names(&tool.registry_for(&explore())), vec!["grep"]);
        assert!(tool.read_only());
    }

    #[test]
    fn read_only_is_the_default() {
        let mut def = explore();
        def.tools = None;
        assert_eq!(names(&agent_tool(vec![]).registry_for(&def)), vec!["grep", "read"]);
    }

    #[test]
    fn write_permission_and_tool_lists() {
        let mut def = explore();
        def.permission = Permission::Write;
        def.tools = Some(vec!["read".into(), "write".into(), "bash".into()]);
        let tool = agent_tool(vec![def.clone()]);
        let registry = tool.registry_for(&def);
        assert_eq!(names(&registry), vec!["bash", "read", "write"]);
        assert!(!tool.read_only());

        // A read-only agent cannot get write tools even by listing them.
        def.permission = Permission::ReadOnly;
        assert_eq!(names(&tool.registry_for(&def)), vec!["read"]);
    }

    #[test]
    fn subagent_reads_do_not_count_for_main_agent() {
        let files = FileTracker::default();
        let mut base = ToolRegistry::new();
        base.register(ReadTool::new(files.clone()));
        base.register(GrepTool);
        let tool = AgentTool::new(Arc::new(vec![explore()]), base, client(), Arc::new(|_: &str, _: &str, _: &Value| {}));
        let registry = tool.registry_for(&AgentDef { tools: None, ..explore() });
        let read = registry.get("read").unwrap().clone();

        let f = tempfile::NamedTempFile::new().unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(read.run(json!({"path": f.path()})));
        assert!(files.check_writable(f.path()).is_err(), "main tracker must not see the subagent's read");
    }

    #[test]
    fn parses_definitions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("reviewer.md"),
            "---\ndescription: Reviews diffs\ntools: Read, Grep, mcp__gh__*\nmodel: claude-sonnet-5\n---\nYou review code.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("fixer.md"),
            "---\nname: fixer\ndescription: Fixes things\npermission: write\ntools:\n  - bash\n  - write\n---\nFix it.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("explore.md"), "---\ndescription: custom explore\n---\nMine.\n").unwrap();
        std::fs::write(dir.path().join("bad.md"), "---\ndescription: x\npermission: root\n---\nbody\n").unwrap();

        let (defs, warnings) = discover(&[dir.path().into()]);
        let by_name: BTreeMap<&str, &AgentDef> = defs.iter().map(|d| (d.name.as_str(), d)).collect();
        assert_eq!(by_name.keys().copied().collect::<Vec<_>>(), vec!["explore", "fixer", "reviewer"]);
        assert_eq!(by_name["explore"].description, "custom explore", "files override built-ins");
        let reviewer = by_name["reviewer"];
        assert_eq!(reviewer.permission, Permission::ReadOnly);
        assert_eq!(reviewer.tools.as_deref(), Some(&["read".to_string(), "grep".into(), "mcp__gh__*".into()][..]));
        assert!(reviewer.allows("mcp__gh__list_issues") && !reviewer.allows("bash"));
        assert_eq!(reviewer.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(by_name["fixer"].permission, Permission::Write);
        assert_eq!(by_name["fixer"].tools.as_deref(), Some(&["bash".to_string(), "write".into()][..]));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unknown permission"));
    }

    #[test]
    fn reports_unknown_tool_names() {
        let mut def = explore();
        def.tools = Some(vec!["read".into(), "glob".into()]);
        let tool = agent_tool(vec![def]);
        assert_eq!(tool.unknown_tools(), vec![("explore".to_string(), "glob".to_string())]);
    }
}
