//! What commands print line by line: without a terminal, with arguments, or
//! where they have no panel (see `commands::interactive` for those).

use serde_json::Value;

use super::parse::{BUILTINS, Builtin, KEYS};
use super::{Output, dialog, output, text};
use super::style::{ACCENT, BOLD, DIM, GRAY, GREEN, RED, YELLOW};
use crate::agent::Agent;
use crate::extensions::mcp;
use crate::extensions::plugins::Plugins;
use crate::extensions::skills::Skill;
use crate::llm;
use crate::security::permissions::Permissions;
use crate::setup::Env;
use crate::subagent::AgentDef;
use crate::tools::ToolRegistry;

pub(super) fn banner(agent: &Agent, skills: &[Skill], env: &Env, out: &Output) {
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = match cwd.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => cwd,
    };
    let servers = match env.mcp_connecting() {
        Some(count) => format!("{count} MCP servers connecting"),
        None => format!("{} MCP servers", env.mcp_statuses().filter(|s| s.outcome.is_ok()).count()),
    };
    let loaded = env.ext.plugins.list.iter().filter(|p| p.outcome.is_ok()).count();
    let plugins = match loaded {
        0 => String::new(),
        1 => " · 1 plugin".to_string(),
        n => format!(" · {n} plugins"),
    };
    let effort = out.paint(DIM, &format!(" · {} effort", agent.client().effort().as_str()));
    out.line(format!("{} {}{effort}", out.paint(ACCENT, "✻ harness"), out.paint(BOLD, agent.model())));
    let sandboxed = if crate::security::sandbox::active().is_some() { " · sandboxed" } else { "" };
    let tools = agent.tools().names().count();
    out.note(format!("  {cwd} · {tools} tools · {} skills · {servers}{plugins}{sandboxed}", skills.len()));
    out.note("  / for commands · @ for files · Ctrl+O for transcript · Ctrl+V to paste an image · Esc or Ctrl-C to interrupt");
    out.line("");
}

/// Says how a command went, under its line.
pub(super) fn done(out: &Output, text: impl AsRef<str>) {
    out.line(format!("{}{}", out.paint(DIM, "  ⎿ "), text.as_ref()));
}

/// Says why a command did nothing, under its line.
pub(super) fn failed(out: &Output, text: impl AsRef<str>) {
    out.line(format!("{}{}", out.paint(DIM, "  ⎿ "), out.paint(RED, text.as_ref())));
}

/// Prints `lines` the way everything a command lists is: two columns in.
fn listing<S: AsRef<str>>(out: &Output, lines: &[S]) {
    out.lines(&lines.iter().map(|line| format!("  {}", line.as_ref()).trim_end().to_string()).collect::<Vec<_>>());
}

/// `pairs` as rows of a dim name and its value, aligned.
fn rows(out: &Output, pairs: &[(String, String)], room: usize) -> Vec<String> {
    dialog::fields(pairs, room, &|sgr, s| out.paint(sgr, s))
}

/// The keys and input forms, under what they are for.
pub(super) fn key_lines(out: &Output) -> Vec<String> {
    let width = KEYS.iter().flat_map(|(_, keys)| keys.iter()).map(|(key, _)| text::width(key)).max().unwrap_or(0);
    let mut lines = Vec::new();
    for (group, keys) in KEYS {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(out.paint(BOLD, group));
        lines.extend(keys.iter().map(|(key, what)| format!("  {}  {}", text::pad(key, width), out.paint(DIM, what))));
    }
    lines
}

pub(super) fn help(out: &Output, skills: &[Skill]) {
    let name = |b: &Builtin| if b.args.is_empty() { b.name.to_string() } else { format!("{} {}", b.name, b.args) };
    let skills: Vec<&Skill> = skills.iter().filter(|s| s.user_invocable).collect();
    let width = BUILTINS.iter().map(|b| text::width(&name(b))).chain(skills.iter().map(|s| text::width(&s.name) + 1)).max().unwrap_or(0);
    let room = text::terminal_width().saturating_sub(width + 8);
    let row = |name: &str, args: &str, about: &str| {
        let pad = " ".repeat(width.saturating_sub(text::width(name) + text::width(args)));
        format!("  {name}{}{pad}  {}", out.paint(DIM, args), out.paint(DIM, &text::fit(&text::printable(about), room)))
    };
    let mut lines = vec![out.paint(BOLD, "Commands")];
    lines.extend(BUILTINS.iter().map(|b| row(b.name, &if b.args.is_empty() { String::new() } else { format!(" {}", b.args) }, b.about)));
    if !skills.is_empty() {
        lines.extend([String::new(), out.paint(BOLD, "Skills")]);
        lines.extend(skills.iter().map(|s| row(&format!("/{}", s.name), "", s.description.lines().next().unwrap_or(""))));
    }
    lines.push(String::new());
    lines.extend(key_lines(out));
    listing(out, &lines);
}

/// The permission mode and rules, line by line.
pub(super) fn permissions(permissions: &Permissions, out: &Output) {
    let mode = permissions.mode();
    let mut pairs = vec![("Mode".to_string(), format!("{} · {}", mode.as_str(), mode.describe()))];
    pairs.extend(permissions.rules().into_iter().filter(|(_, r)| !r.is_empty()).map(|(kind, r)| (capitalized(kind), r.join(", "))));
    let mut lines = rows(out, &pairs, text::terminal_width().saturating_sub(3));
    lines.push(out.paint(DIM, "/permissions <mode> or Shift+Tab changes the mode; rules go under `permissions` in ~/.harness/settings.json"));
    listing(out, &lines);
}

/// `text` starting with a capital.
pub(super) fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// The first sentence of `description`, for a row of a list.
pub(super) fn summary(description: &str) -> &str {
    let line = description.lines().next().unwrap_or("");
    line.split(". ").next().unwrap_or("").trim_end_matches('.')
}

pub(super) fn tools(out: &Output, tools: &ToolRegistry) {
    let tools = tools.definitions();
    let name_width = tools.iter().map(|t| text::width(&t.name)).max().unwrap_or(0);
    let room = text::terminal_width().saturating_sub(name_width + 5);
    for tool in tools {
        let summary = text::fit(summary(&tool.description), room);
        out.line(format!("  {}  {}", out.paint(BOLD, &text::pad(&tool.name, name_width)), out.paint(DIM, &summary)));
    }
}

/// Whether the sandbox is on and what it lets commands do, `room` columns
/// wide.
pub(super) fn sandbox_lines(out: &Output, room: usize) -> Vec<String> {
    let Some(sandbox) = crate::security::sandbox::active() else {
        return vec![
            format!("{} Off", out.paint(DIM, "○")),
            out.paint(DIM, "Commands run with your own access. To contain them, start with --sandbox or HARNESS_SANDBOX=1,"),
            out.paint(DIM, "or set \"sandbox\": {\"enabled\": true} in ~/.harness/settings.json"),
        ];
    };
    let startup = ("startup", format!("{} per command (measured at launch)", crate::util::short_duration(sandbox.startup())));
    let pairs: Vec<_> = sandbox.describe().into_iter().chain([startup]).map(|(name, value)| (capitalized(name), value)).collect();
    let mut lines = vec![format!("{} On", out.paint(GREEN, "●")), String::new()];
    lines.extend(rows(out, &pairs, room));
    lines
}

pub(super) fn sandbox(out: &Output) {
    listing(out, &sandbox_lines(out, text::terminal_width().saturating_sub(3)));
}

/// About what the requests cost at list prices (or the configured ones),
/// for the models whose price is known.
pub(super) fn usage_cost(agent: &Agent) -> Option<String> {
    let providers = agent.client().providers();
    let models = agent.usage().models();
    let priced: Vec<f64> =
        models.iter().filter_map(|(model, t)| providers.price(model).map(|(price, ttl)| price.cost(&t.usage, ttl))).collect();
    if priced.is_empty() {
        return None;
    }
    let left_out = if priced.len() < models.len() { ", leaving out models without a known price" } else { "" };
    Some(format!("About ${:.2} at list prices{left_out}", priced.iter().sum::<f64>()))
}

/// Cells of the bar showing how full the context is.
const CONTEXT_BAR: usize = 24;

/// How full the context is, as a bar where `room` columns have place for
/// one, and the tokens used by model.
pub(super) fn usage_lines(out: &Output, agent: &Agent, room: usize) -> Vec<String> {
    let (used, window) = (agent.context_tokens(), agent.client().context_window());
    let context = if window >= u64::from(u32::MAX) {
        // A model the providers do not know.
        format!("about {} tokens", output::tokens(used))
    } else {
        let percent = used.saturating_mul(100) / window.max(1);
        let size = format!("{} of {} tokens · {percent}%", output::tokens(used), output::tokens(window));
        if room < CONTEXT_BAR + text::width(&size) + 11 {
            size
        } else {
            let filled = ((used.saturating_mul(CONTEXT_BAR as u64) + window / 2) / window.max(1)).min(CONTEXT_BAR as u64) as usize;
            let filled = if used > 0 { filled.max(1) } else { 0 };
            let color = if percent >= 90 { RED } else if percent >= 70 { YELLOW } else { GREEN };
            format!("{}{}  {size}", out.paint(color, &"█".repeat(filled)), out.paint(DIM, &"░".repeat(CONTEXT_BAR - filled)))
        }
    };
    let mut lines = vec![format!("{}  {context}", out.paint(DIM, "Context"))];
    let models = agent.usage().models();
    if models.is_empty() {
        lines.extend([String::new(), out.paint(DIM, "No requests yet")]);
        return lines;
    }
    let width = models.iter().map(|(m, _)| text::width(m)).max().unwrap_or(0).max("Model".len());
    let header = format!(
        "{}  {:>8}  {:>9}  {:>10}  {:>11}  {:>9}",
        text::pad("Model", width), "Requests", "Input", "Cache read", "Cache write", "Output"
    );
    lines.extend([String::new(), out.paint(DIM, &header)]);
    for (model, t) in &models {
        let u = &t.usage;
        lines.push(format!(
            "{}  {:>8}  {:>9}  {:>10}  {:>11}  {:>9}",
            text::pad(model, width),
            t.requests,
            output::tokens(u.input_tokens),
            output::tokens(u.cache_read_tokens),
            output::tokens(u.cache_write_tokens),
            output::tokens(u.output_tokens)
        ));
    }
    let input: u64 = models.iter().map(|(_, t)| t.usage.input_tokens).sum();
    let cached: u64 = models.iter().map(|(_, t)| t.usage.cache_read_tokens).sum();
    lines.push(String::new());
    if let Some(percent) = (cached * 100).checked_div(input) {
        lines.push(out.paint(DIM, &format!("{percent}% of input tokens came from the prompt cache")));
    }
    lines.extend(usage_cost(agent).map(|cost| out.paint(DIM, &cost)));
    lines
}

pub(super) fn usage(out: &Output, agent: &Agent) {
    listing(out, &usage_lines(out, agent, text::terminal_width().saturating_sub(3)));
}

pub(super) fn skills(out: &Output, skills: &[Skill]) {
    if skills.is_empty() {
        done(out, "No skills or commands found in .harness, .claude or .agents, here or under ~");
    }
    let room = text::terminal_width().saturating_sub(5);
    for s in skills {
        let description = text::fit(&text::printable(s.description.lines().next().unwrap_or("")), room.saturating_sub(text::width(&s.name) + 3));
        out.line(format!("  {}  {}", out.paint(BOLD, &format!("/{}", s.name)), out.paint(DIM, &description)));
        out.line(format!("    {}", out.paint(GRAY, &s.dir.display().to_string())));
    }
}

pub(super) fn models(out: &Output, client: &llm::Client) {
    let providers = client.providers();
    let pairs = [
        ("Model".to_string(), client.spec().to_string()),
        ("Subagents".to_string(), client.subagent_model().unwrap_or("follow the main model").to_string()),
        ("Effort".to_string(), client.effort().as_str().to_string()),
    ];
    let mut lines = rows(out, &pairs, text::terminal_width().saturating_sub(3));
    let listed = providers.listed_models();
    for name in providers.names() {
        let protocol = providers.protocol_of(name).map_or("?", llm::Protocol::as_str);
        let models: Vec<&String> = listed.iter().filter(|m| m.split_once('/').is_some_and(|(provider, _)| provider == name)).collect();
        let about = if models.is_empty() { format!("· {protocol} · no models listed") } else { format!("· {protocol}") };
        lines.extend([String::new(), format!("{} {}", out.paint(BOLD, name), out.paint(DIM, &about))]);
        for model in models {
            let mark = if model == client.spec() { out.paint(GREEN, "✓") } else { " ".to_string() };
            lines.push(format!("{mark} {}", &model[name.len() + 1..]));
        }
    }
    lines.extend([
        String::new(),
        out.paint(DIM, "/model <provider>/<model> switches; any model name the gateway accepts works"),
        out.paint(DIM, "/provider add <name> adds a provider, /provider model add <provider>/<model> a model"),
    ]);
    listing(out, &lines);
}

pub(super) fn agents(out: &Output, defs: &[AgentDef]) {
    let room = text::terminal_width().saturating_sub(5);
    for d in defs {
        let tools = d.tools.as_ref().map_or("all permitted tools".to_string(), |t| t.join(", "));
        let source = d.path.as_ref().map_or("built-in".to_string(), |p| p.display().to_string());
        let model = d.model.as_ref().map_or(String::new(), |m| format!(" · model {m}"));
        out.line(format!("  {}  {}", out.paint(BOLD, &d.name), out.paint(DIM, &format!("{} · {tools}", d.permission.as_str()))));
        out.lines(&text::wrap(&text::printable(&d.description), room).iter().map(|line| format!("    {line}")).collect::<Vec<_>>());
        out.line(format!("    {}", out.paint(GRAY, &format!("{source}{model}"))));
    }
}

/// A subagent's status, as a mark and its color.
pub(super) fn status_mark(status: &str) -> (&'static str, &'static str) {
    match status {
        "running" => ("●", GREEN),
        "completed" | "ended" => ("✓", GREEN),
        "failed" => ("✗", RED),
        _ => ("○", YELLOW),
    }
}

pub(super) fn tasks(out: &Output, subagents: &[Value], shells: &[(String, String, bool)]) {
    if subagents.is_empty() && shells.is_empty() {
        done(out, "No subagents or background commands in this conversation");
        return;
    }
    for s in subagents {
        let status = s["status"].as_str().unwrap_or("");
        let (mark, color) = status_mark(status);
        let mut info = vec![
            s["model"].as_str().unwrap_or("").to_string(),
            format!("{} tool calls", s["tool_calls"]),
            output::elapsed(s["seconds"].as_u64().unwrap_or(0)),
        ];
        if s["background"] == true {
            info.push("background".into());
        }
        if let Some(last) = s["last_call"].as_str().filter(|_| status == "running") {
            info.push(format!("latest {}", text::printable(last)));
        }
        out.line(format!(
            "  {} {}  {}  {}  {}",
            out.paint(color, mark),
            out.paint(BOLD, s["id"].as_str().unwrap_or("")),
            out.paint(color, &text::pad(status, 9)),
            text::printable(s["label"].as_str().unwrap_or("")),
            out.paint(DIM, &info.join(" · "))
        ));
    }
    for (id, command, running) in shells {
        let status = if *running { "running" } else { "ended" };
        let (mark, color) = status_mark(status);
        let command = text::fit(&text::printable(command.lines().next().unwrap_or("")), 80);
        let status = out.paint(if *running { color } else { DIM }, &text::pad(status, 9));
        out.line(format!("  {} {}  {status}  {}", out.paint(color, mark), out.paint(BOLD, id), out.paint(DIM, &format!("$ {command}"))));
    }
    out.line(out.paint(DIM, "  /stop <id> stops one, /stop every running one"));
}

pub(super) fn plugins(out: &Output, plugins: &Plugins) {
    if plugins.list.is_empty() {
        done(out, "No plugins enabled");
    }
    for p in &plugins.list {
        let version = p.version.as_ref().map_or(String::new(), |v| format!(" {}", out.paint(DIM, &format!("v{v}"))));
        match &p.outcome {
            Ok(parts) => {
                let parts = if parts.is_empty() { "nothing the harness can use".to_string() } else { parts.join(", ") };
                out.line(format!("  {} {}{version}  {}", out.paint(GREEN, "●"), out.paint(BOLD, &p.id), out.paint(DIM, &parts)));
            }
            Err(e) => out.line(format!("  {} {}  {}", out.paint(RED, "✗"), out.paint(BOLD, &p.id), out.paint(RED, e))),
        }
        if let Some(description) = &p.description {
            out.line(format!("    {description}"));
        }
        if let Some(root) = &p.root {
            out.line(format!("    {}", out.paint(GRAY, &root.display().to_string())));
        }
        for warning in &p.warnings {
            out.line(format!("    {}", out.paint(YELLOW, &format!("! {warning}"))));
        }
        if !p.ignored.is_empty() {
            out.line(format!("    {}", out.paint(DIM, &format!("not used: {}", p.ignored.join("; ")))));
        }
    }
    out.line(out.paint(DIM, "  /plugins install|uninstall|enable|disable|configure|marketplace … manages them (see /plugins help)"));
}

/// Pulls the latest of every plugin and marketplace cloned from git.
pub(super) async fn update_plugins(plugins: &Plugins, out: &Output) {
    let plugins = plugins.clone();
    done(out, "Updating plugins…");
    let lines = match tokio::task::spawn_blocking(move || plugins.update()).await {
        Ok(lines) => lines,
        Err(e) => vec![Err(e.to_string())],
    };
    if lines.is_empty() {
        done(out, "No plugins come from git repositories");
        return;
    }
    for line in lines {
        match line {
            Ok(line) => out.line(line),
            Err(e) => out.error(&e),
        }
    }
    out.note("  /reload-plugins (or a restart) uses the new versions");
}

pub(super) fn mcp<'a>(out: &Output, statuses: impl Iterator<Item = &'a mcp::Status>) {
    let statuses: Vec<&mcp::Status> = statuses.collect();
    if statuses.is_empty() {
        done(out, NO_MCP);
    }
    for status in statuses {
        let took = crate::util::short_duration(status.took);
        match &status.outcome {
            Ok(tools) => {
                let info = format!("{} {} · connected in {took}", tools.len(), text::plural(tools.len(), "tool"));
                out.line(format!("  {} {}  {}", out.paint(GREEN, "●"), out.paint(BOLD, &status.name), out.paint(DIM, &info)));
                for t in tools {
                    out.line(format!("    {t}"));
                }
            }
            Err(e) => {
                out.line(format!("  {} {}  {}", out.paint(RED, "✗"), out.paint(BOLD, &status.name), out.paint(DIM, &format!("failed after {took}"))));
                out.line(format!("    {}", out.paint(RED, e)));
            }
        }
        out.line(format!("    {}", out.paint(GRAY, &format!("log: {}", mcp::log_path(&status.name).display()))));
    }
}

/// What `/mcp` says without servers.
pub(super) const NO_MCP: &str = "No MCP servers configured · they go under `mcpServers` in ~/.harness/settings.json";
