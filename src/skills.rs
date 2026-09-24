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
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolOutput, parse_input};

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

impl Skill {
    /// Reads the skill body fresh from disk, so edits apply without a restart.
    pub fn load(&self, arguments: Option<&str>) -> std::io::Result<String> {
        let text = std::fs::read_to_string(self.dir.join("SKILL.md"))?;
        let body = split_frontmatter(&text).map_or(text.as_str(), |(_, body)| body).trim();
        let mut out = format!(
            "Base directory for this skill: {}\nRelative paths in the skill resolve against it.\n\n{body}",
            self.dir.display()
        );
        if let Some(args) = arguments.map(str::trim).filter(|a| !a.is_empty()) {
            out.push_str(&format!("\n\nARGUMENTS: {args}"));
        }
        Ok(out)
    }
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
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            let file = dir.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            match parse_skill(&text, &dir) {
                Ok(skill) => {
                    skills.entry(skill.name.clone()).or_insert(skill);
                }
                Err(e) => warnings.push(format!("{}: {e}", file.display())),
            }
        }
    }
    (skills.into_values().collect(), warnings)
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
    Ok(Skill { name, description, dir })
}

/// Splits `---\n<frontmatter>\n---\n<body>`.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
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
fn parse_frontmatter(front: &str) -> BTreeMap<String, String> {
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
    skills: Arc<Vec<Skill>>,
    description: String,
}

impl SkillTool {
    pub fn new(skills: Arc<Vec<Skill>>) -> Self {
        let mut description = String::from(
            "Load a skill: a set of instructions (and possibly scripts or reference files) for a \
             specific kind of task. When the user's request matches a skill below, call this tool \
             with the skill's name before starting the task, then follow the instructions it \
             returns. Do not load a skill that is already loaded in this conversation.\n\n\
             Available skills:",
        );
        for s in skills.iter() {
            description.push_str(&format!("\n- {}: {}", s.name, s.description));
        }
        Self { skills, description }
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

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let Some(skill) = self.skills.iter().find(|s| s.name == input.name) else {
            return ToolOutput::error(format!("Unknown skill: {}", input.name));
        };
        match skill.load(input.arguments.as_deref()) {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::error(format!("Cannot load skill {}: {e}", skill.name)),
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
        let tool = SkillTool::new(Arc::new(skills));
        assert!(tool.description().contains("- greet: Say hi"));

        let out = tool.run(json!({"name": "greet", "arguments": "Bob"})).await;
        assert!(!out.is_error);
        assert!(out.content.starts_with("Base directory for this skill: "));
        assert!(out.content.ends_with("Say hi to the user.\n\nARGUMENTS: Bob"), "{}", out.content);

        let out = tool.run(json!({"name": "nope"})).await;
        assert!(out.is_error);
    }
}
