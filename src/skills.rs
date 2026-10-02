//! Agent Skills: folders of instructions the model loads on demand.
//!
//! A skill is a directory containing a `SKILL.md` whose YAML frontmatter
//! gives a `name` and a `description`:
//!
//! ```markdown
//! ---
//! name: release-notes
//! description: Draft release notes from the git log. Use when asked for a changelog.
//! ---
//! 1. Run `git log --oneline <last-tag>..HEAD` ...
//! ```
//!
//! Only names and descriptions are put in front of the model up front (in
//! the `skill` tool's description). The body is loaded when the model calls
//! the tool, or when the user types `/<name>`; files the body refers to are
//! read with the ordinary tools, relative to the skill's directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolOutput, parse_input};

/// What a `!`command`` in a skill may print before it is cut.
const MAX_SHELL_OUTPUT: usize = 30_000;
/// How long it may run.
const SHELL_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill's directory; for a command, the directory of its file.
    pub dir: PathBuf,
    body: Body,
    /// `false` with `disable-model-invocation: true`: only `/<name>` runs it.
    pub model_invocable: bool,
    /// `false` with `user-invocable: false`: only the model loads it.
    pub user_invocable: bool,
    /// For a plugin's skill or command: the plugin's root and data
    /// directories, for `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}`.
    pub plugin: Option<PluginDirs>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginDirs {
    pub root: PathBuf,
    pub data: PathBuf,
}

impl PluginDirs {
    /// `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}` in `text`
    /// replaced by the directories.
    pub fn expand(&self, text: &str) -> String {
        text.replace("${CLAUDE_PLUGIN_ROOT}", &self.root.display().to_string())
            .replace("${CLAUDE_PLUGIN_DATA}", &self.data.display().to_string())
    }

    /// The variables as a plugin's processes get them.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("CLAUDE_PLUGIN_ROOT".into(), self.root.display().to_string()),
            ("CLAUDE_PLUGIN_DATA".into(), self.data.display().to_string()),
        ]
    }
}

/// Where a skill's text comes from.
#[derive(Debug, Clone, PartialEq)]
enum Body {
    /// `SKILL.md` in the skill's directory, which may hold more files.
    Skill,
    /// A command: a Markdown file of its own, in Claude Code's older format.
    File(PathBuf),
    /// A command written into a plugin's manifest.
    Inline(String),
}

impl Skill {
    /// The skill's instructions with `arguments` filled in, read fresh from
    /// disk so edits apply without a restart: the `$ARGUMENTS` placeholders
    /// and path variables replaced, then each `!`command`` replaced by what
    /// it printed. Arguments no placeholder took are appended.
    pub async fn render(&self, arguments: &str) -> Result<String, String> {
        let text = match &self.body {
            Body::Skill => std::fs::read_to_string(self.dir.join("SKILL.md")),
            Body::File(file) => std::fs::read_to_string(file),
            Body::Inline(text) => Ok(text.clone()),
        };
        let text = text.map_err(|e| format!("cannot load skill {}: {e}", self.name))?;
        let (fields, body) = match split_frontmatter(&text) {
            Some((front, body)) => (parse_frontmatter(front), body),
            None => (BTreeMap::new(), text.as_str()),
        };
        let mut body = body.trim().replace("${CLAUDE_SKILL_DIR}", &self.dir.display().to_string());
        if let Ok(cwd) = std::env::current_dir() {
            body = body.replace("${CLAUDE_PROJECT_DIR}", &cwd.display().to_string());
        }
        if let Some(plugin) = &self.plugin {
            body = plugin.expand(&body);
        }
        let names = fields.get("arguments").map_or_else(Vec::new, |a| list_field(a));
        let (body, taken) = substitute_arguments(&body, arguments, &names);
        let mut body = run_commands(&body).await.map_err(|e| format!("cannot load skill {}: {e}", self.name))?;
        let arguments = arguments.trim();
        if !taken && !arguments.is_empty() {
            body.push_str(&format!("\n\nARGUMENTS: {arguments}"));
        }
        Ok(match self.body {
            Body::Skill => format!(
                "Base directory for this skill: {}\nRelative paths in the skill resolve against it.\n\n{body}",
                self.dir.display()
            ),
            Body::File(_) | Body::Inline(_) => body,
        })
    }

    /// The prompt for a `/<skill> args` command typed by the user.
    pub async fn invocation_prompt(&self, args: &str) -> Result<String, String> {
        let body = self.render(args).await?;
        Ok(format!("The user invoked the `{}` skill:\n\n{body}", self.name))
    }

    /// A command: the Markdown `file` run as `/<name>`, which the model
    /// may load as a skill too.
    pub fn command(name: String, file: &Path, plugin: Option<PluginDirs>) -> Result<Self, String> {
        let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
        let dir = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        let mut skill = Self::inline(name, &text, None, plugin);
        skill.dir = std::path::absolute(&dir).unwrap_or(dir);
        skill.body = Body::File(std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf()));
        Ok(skill)
    }

    /// A command whose text is `text`, as a plugin manifest can define
    /// one; `description` wins over the frontmatter's.
    pub fn inline(name: String, text: &str, description: Option<&str>, plugin: Option<PluginDirs>) -> Self {
        let (fields, body) = match split_frontmatter(text) {
            Some((front, body)) => (parse_frontmatter(front), body),
            None => (BTreeMap::new(), text),
        };
        // Claude Code shows a command without a description by its first
        // line.
        let first_line = || {
            let line = body.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty());
            crate::util::ellipsize(line.unwrap_or("(no description)"), 100)
        };
        let description = description
            .map(String::from)
            .or_else(|| fields.get("description").cloned().filter(|d| !d.is_empty()))
            .unwrap_or_else(first_line);
        let dir = plugin.as_ref().map_or_else(PathBuf::new, |p| p.root.clone());
        let (model_invocable, user_invocable) = invocable(&fields);
        Self { name, description, dir, body: Body::Inline(text.to_string()), model_invocable, user_invocable, plugin }
    }
}

/// Who may run a skill with these frontmatter `fields`: the model, the user.
fn invocable(fields: &BTreeMap<String, String>) -> (bool, bool) {
    let flag = |key: &str| fields.get(key).map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"));
    // `hide-from-slash-command-tool` is what older commands used.
    let model = !(flag("disable-model-invocation").unwrap_or(false) || flag("hide-from-slash-command-tool").unwrap_or(false));
    let user = fields.get("user-invocable").is_none_or(|v| !matches!(v.to_ascii_lowercase().as_str(), "false" | "no" | "off" | "0"));
    (model, user)
}

/// The items of a frontmatter list: `a b`, `a, b`, `[a, b]`, or a YAML
/// list (which [`parse_frontmatter`] joins into `- a - b`).
pub(crate) fn list_field(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c.is_whitespace() || c == '[' || c == ']')
        .map(|n| n.trim_matches(['"', '\'']))
        .filter(|n| !n.is_empty() && *n != "-")
        .map(String::from)
        .collect()
}

/// Splits arguments the way a shell would: on whitespace, keeping quoted
/// parts together.
fn split_arguments(arguments: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = arguments.chars();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            (None, c) if c.is_whitespace() => words.extend(word.take()),
            (q, '\\') if q != Some('\'') => word.get_or_insert_default().extend(chars.next()),
            (_, c) => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// Claude Code's argument placeholders: `$ARGUMENTS` (everything),
/// `$ARGUMENTS[N]` and `$N` (the Nth argument, from 0), and `$name` for the
/// names the frontmatter's `arguments` lists. `\$` keeps a `$` as it is,
/// and an index past the last argument stays as written. Also says whether
/// a placeholder took the arguments.
fn substitute_arguments(body: &str, arguments: &str, names: &[String]) -> (String, bool) {
    static PLACEHOLDER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\?\$(?:ARGUMENTS\[(\d+)\]|ARGUMENTS|(\d+)|([A-Za-z_][A-Za-z0-9_-]*))").unwrap());
    let words = split_arguments(arguments);
    let mut taken = false;
    let out = PLACEHOLDER.replace_all(body, |c: &regex::Captures| {
        let all = &c[0];
        let named = c.get(3).and_then(|n| names.iter().position(|name| name == n.as_str()));
        if c.get(3).is_some() && named.is_none() {
            return all.to_string();
        }
        if let Some(escaped) = all.strip_prefix('\\') {
            return escaped.to_string();
        }
        let index = c.get(1).or(c.get(2)).map(|n| n.as_str().parse::<usize>().unwrap_or(usize::MAX)).or(named);
        match index {
            None => {
                taken = true;
                arguments.trim().to_string()
            }
            Some(i) => match words.get(i) {
                Some(word) => {
                    taken = true;
                    word.clone()
                }
                None if named.is_some() => String::new(),
                None => all.to_string(),
            },
        }
    });
    (out.into_owned(), taken)
}

/// Replaces each `!`command`` (at the start of a line or after a space)
/// and each ```` ```! ```` block with what the command printed. They run
/// like the `bash` tool's commands, in the sandbox when it is on; one that
/// fails fails the whole skill.
async fn run_commands(body: &str) -> Result<String, String> {
    static INLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)(^|[ \t])!`([^`\n]+)`").unwrap());
    let mut out = String::new();
    let mut lines = body.split_inclusive('\n');
    while let Some(line) = lines.next() {
        if line.trim_end() == "```!" {
            let mut script = String::new();
            for line in lines.by_ref() {
                if line.trim_end() == "```" {
                    break;
                }
                script.push_str(line);
            }
            out.push_str(&run_command(&script).await?);
            out.push('\n');
            continue;
        }
        let mut last = 0;
        for c in INLINE.captures_iter(line) {
            let (whole, lead, command) = (c.get(0).unwrap(), &c[1], &c[2]);
            out.push_str(&line[last..whole.start()]);
            out.push_str(lead);
            out.push_str(run_command(command).await?.trim_end());
            last = whole.end();
        }
        out.push_str(&line[last..]);
    }
    Ok(out)
}

async fn run_command(command: &str) -> Result<String, String> {
    let input = json!({"command": command, "timeout_ms": SHELL_TIMEOUT_MS});
    let output = crate::tools::bash::BashTool.run(input).await;
    if output.is_error {
        return Err(format!("shell command `{}` failed: {}", command.trim(), output.content.trim()));
    }
    let text = if output.content == "(no output)" { "" } else { &output.content };
    Ok(crate::tools::truncate(text, MAX_SHELL_OUTPUT))
}

/// Default search roots, highest precedence first: project-level
/// `.harness/skills` and `.claude/skills`, then the same under `$HOME`.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(".harness/skills"), PathBuf::from(".claude/skills")];
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        roots.push(home.join(".harness/skills"));
        roots.push(home.join(".claude/skills"));
    }
    roots
}

/// Finds `<root>/<dir>/SKILL.md` in each root. When two skills share a name,
/// the one from the earlier root wins. Returns skills sorted by name plus
/// warnings for skill files that could not be used.
pub fn discover(roots: &[PathBuf]) -> (Vec<Skill>, Vec<String>) {
    let mut skills = BTreeMap::new();
    let mut warnings = Vec::new();
    for root in roots {
        for dir in skill_dirs(root) {
            match load_skill(&dir, None, None) {
                Ok(skill) => {
                    skills.entry(skill.name.clone()).or_insert(skill);
                }
                Err(e) => warnings.push(e),
            }
        }
    }
    (skills.into_values().collect(), warnings)
}

/// The directories under `root` with a `SKILL.md`, sorted.
fn skill_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> =
        entries.flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.join("SKILL.md").is_file()).collect();
    dirs.sort();
    dirs
}

/// A plugin's skills in `root`: the skill `root` is itself, or those in its
/// directories, named `<prefix>:<name>`.
pub fn discover_in_plugin(root: &Path, prefix: &str, plugin: &PluginDirs) -> (Vec<Skill>, Vec<String>) {
    let dirs = if root.join("SKILL.md").is_file() { vec![root.to_path_buf()] } else { skill_dirs(root) };
    let (mut skills, mut warnings) = (Vec::new(), Vec::new());
    for dir in dirs {
        match load_skill(&dir, Some(prefix), Some(plugin)) {
            Ok(skill) => skills.push(skill),
            Err(e) => warnings.push(e),
        }
    }
    (skills, warnings)
}

fn load_skill(dir: &Path, prefix: Option<&str>, plugin: Option<&PluginDirs>) -> Result<Skill, String> {
    let file = dir.join("SKILL.md");
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut skill = parse_skill(&text, dir).map_err(|e| format!("{}: {e}", file.display()))?;
    if let Some(prefix) = prefix {
        skill.name = format!("{prefix}:{}", skill.name);
    }
    skill.plugin = plugin.cloned();
    Ok(skill)
}

fn parse_skill(text: &str, dir: &Path) -> Result<Skill, String> {
    let (front, _) = split_frontmatter(text).ok_or("missing `---` frontmatter")?;
    let fields = parse_frontmatter(front);
    let dir_name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = fields.get("name").cloned().unwrap_or(dir_name);
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':') {
        return Err(format!("invalid skill name `{name}`"));
    }
    let description = fields.get("description").cloned().filter(|d| !d.is_empty()).ok_or("missing `description`")?;
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let (model_invocable, user_invocable) = invocable(&fields);
    Ok(Skill { name, description, dir, body: Body::Skill, model_invocable, user_invocable, plugin: None })
}

/// Splits `---\n<frontmatter>\n---\n<body>`.
pub(crate) fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?.trim_start_matches(['\r', '\n']);
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Reads top-level `key: value` pairs from simple YAML: quoted values,
/// `>` / `|` block scalars and indented continuation lines are joined
/// into a single line. Nested structures are ignored.
pub(crate) fn parse_frontmatter(front: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in front.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if indented {
            if let Some(key) = &current {
                let value: &mut String = fields.entry(key.clone()).or_default();
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            current = None;
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim();
        let value = match value {
            ">" | "|" | ">-" | "|-" => String::new(),
            v if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) => {
                v[1..v.len() - 1].to_string()
            }
            v => v.to_string(),
        };
        fields.insert(key.clone(), value);
        current = Some(key);
    }
    fields
}

/// The `skill` tool: loads a skill's instructions into the conversation.
pub struct SkillTool {
    /// The skills the model may load.
    skills: Vec<Skill>,
    description: String,
    /// Whether none of them runs shell commands.
    read_only: bool,
}

impl SkillTool {
    pub fn new(skills: &[Skill]) -> Self {
        let skills: Vec<Skill> = skills.iter().filter(|s| s.model_invocable).cloned().collect();
        let mut description = String::from(
            "Load a skill: a set of instructions (and possibly scripts or reference files) for a \
             specific kind of task. When the user's request matches a skill below, call this tool \
             with the skill's name before starting the task, then follow the instructions it \
             returns. Do not load a skill that is already loaded in this conversation.\n\n\
             Available skills:",
        );
        for s in &skills {
            description.push_str(&format!("\n- {}: {}", s.name, s.description));
        }
        let read_only = !skills.iter().any(Skill::runs_commands);
        Self { skills, description, read_only }
    }

    /// Whether any skill is offered to the model.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

impl Skill {
    /// Whether the skill's text has `!`command``s, which loading it runs.
    fn runs_commands(&self) -> bool {
        let text = match &self.body {
            Body::Skill => std::fs::read_to_string(self.dir.join("SKILL.md")).unwrap_or_default(),
            Body::File(file) => std::fs::read_to_string(file).unwrap_or_default(),
            Body::Inline(text) => text.clone(),
        };
        text.contains("!`") || text.lines().any(|l| l.trim_end() == "```!")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    name: String,
    arguments: Option<String>,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "enum": names, "description": "Name of the skill to load."},
                "arguments": {"type": "string", "description": "Optional arguments or context for the skill."}
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    /// Loading a skill whose text runs shell commands runs them.
    fn read_only(&self) -> bool {
        self.read_only
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let Some(skill) = self.skills.iter().find(|s| s.name == input.name) else {
            return ToolOutput::error(format!("Unknown skill: {}", input.name));
        };
        match skill.render(input.arguments.as_deref().unwrap_or("")).await {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, content: &str) {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parses_frontmatter_forms() {
        let fields = parse_frontmatter(
            "name: \"quoted\"\ndescription: >\n  folded line one\n  line two\nlicense: MIT\nmetadata:\n  k: v\n",
        );
        assert_eq!(fields["name"], "quoted");
        assert_eq!(fields["description"], "folded line one line two");
        assert_eq!(fields["license"], "MIT");
    }

    #[test]
    fn discovers_with_precedence_and_warnings() {
        let project = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        write_skill(project.path(), "notes", "---\nname: notes\ndescription: project notes\n---\nbody\n");
        write_skill(user.path(), "notes", "---\nname: notes\ndescription: user notes\n---\nbody\n");
        write_skill(user.path(), "dirname", "---\ndescription: name from dir\n---\nx\n");
        write_skill(user.path(), "broken", "no frontmatter here\n");

        let (skills, warnings) = discover(&[project.path().into(), user.path().into()]);
        let names: Vec<(&str, &str)> = skills.iter().map(|s| (s.name.as_str(), s.description.as_str())).collect();
        assert_eq!(names, vec![("dirname", "name from dir"), ("notes", "project notes")]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("broken"));
    }

    #[tokio::test]
    async fn tool_loads_body_with_arguments() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "greet", "---\nname: greet\ndescription: Say hi\n---\n\nSay hi to the user.\n");
        let (skills, _) = discover(&[root.path().into()]);
        let tool = SkillTool::new(&skills);
        assert!(tool.description().contains("- greet: Say hi"));

        let out = tool.run(json!({"name": "greet", "arguments": "Bob"})).await;
        assert!(!out.is_error);
        assert!(out.content.starts_with("Base directory for this skill: "));
        assert!(out.content.ends_with("Say hi to the user.\n\nARGUMENTS: Bob"), "{}", out.content);

        let out = tool.run(json!({"name": "nope"})).await;
        assert!(out.is_error);
    }

    #[test]
    fn substitutes_arguments() {
        let sub = |body: &str, args: &str| substitute_arguments(body, args, &["issue".to_string()]);
        assert_eq!(sub("Fix $ARGUMENTS now", "a b"), ("Fix a b now".into(), true));
        assert_eq!(sub("$0 then $ARGUMENTS[1], $2", "one 'two words'"), ("one then two words, $2".into(), true));
        assert_eq!(sub("issue #$issue, $HOME", "42"), ("issue #42, $HOME".into(), true));
        assert_eq!(sub("costs \\$1.00", "x"), ("costs $1.00".into(), false));
        assert_eq!(sub("no placeholder", "x"), ("no placeholder".into(), false));
        assert_eq!(sub("$1 alone", "x"), ("$1 alone".into(), false), "an index past the arguments stays");
        assert_eq!(split_arguments(r#"a "b c" d\ e ''"#), ["a", "b c", "d e", ""]);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn runs_shell_commands() {
        let _guard = crate::util::lock_cwd();
        let body = "Branch: !`echo main`\nLiteral x!`not run` and a`!`b`\n```!\necho one\necho two\n```\nend";
        let out = run_commands(body).await.unwrap();
        assert_eq!(out, "Branch: main\nLiteral x!`not run` and a`!`b`\none\ntwo\n\nend");
        let err = run_commands("!`exit 3`").await.unwrap_err();
        assert!(err.contains("`exit 3` failed"), "{err}");
    }

    #[tokio::test]
    async fn commands_and_plugin_skills() {
        let root = tempfile::tempdir().unwrap();
        let plugin = PluginDirs { root: root.path().into(), data: root.path().join("data") };
        let file = root.path().join("commit.md");
        std::fs::write(&file, "---\ndisable-model-invocation: true\n---\n# Commit the work\nRun ${CLAUDE_PLUGIN_ROOT}/x $1\n")
            .unwrap();
        let command = Skill::command("p:commit".into(), &file, Some(plugin.clone())).unwrap();
        assert_eq!(command.description, "Commit the work");
        assert!(!command.model_invocable && command.user_invocable);
        let text = command.render("a b").await.unwrap();
        assert_eq!(text, format!("# Commit the work\nRun {}/x b", root.path().display()));

        write_skill(root.path(), "skills/review", "---\ndescription: Review\nuser-invocable: false\n---\nbody\n");
        let (skills, warnings) = discover_in_plugin(&root.path().join("skills"), "p", &plugin);
        assert!(warnings.is_empty());
        assert_eq!(skills[0].name, "p:review");
        assert!(skills[0].model_invocable && !skills[0].user_invocable);

        let tool = SkillTool::new(&[command.clone(), skills[0].clone()]);
        assert!(tool.description().contains("p:review") && !tool.description().contains("p:commit"));
        assert!(tool.run(json!({"name": "p:commit"})).await.is_error, "the model cannot run it");
        assert!(tool.read_only());
        let inline = Skill::inline("p:status".into(), "Show !`git status`", Some("Status"), Some(plugin));
        assert!(!SkillTool::new(&[inline]).read_only(), "loading it runs a command");
    }
}
