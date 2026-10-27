//! What commands print line by line: without a terminal, with arguments, or
//! where they have no panel (see `commands::interactive` for those).

use serde_json::Value;

use super::parse::{BUILTINS, help_keys};
use super::{Output, output, text};
use crate::agent::Agent;
use crate::llm;
use crate::mcp;
use crate::permissions::Permissions;
use crate::plugins::Plugins;
use crate::setup::Env;
use crate::skills::Skill;
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
    let effort = out.paint("2", &format!(" · {} effort", agent.client().effort().as_str()));
    out.line(format!("{} {}{effort}", out.paint("1;35", "✻ harness"), out.paint("1", agent.model())));
    out.line(out.paint(
        "2",
        &format!(
            "  {cwd} · {} tools · {} skills · {servers}{plugins}{}",
            agent.tools().names().count(),
            skills.len(),
            if crate::sandbox::active().is_some() { " · sandboxed" } else { "" }
        ),
    ));
    out.line(out.paint("2", "  / for commands · @ for files · Ctrl+O for transcript · Ctrl+V to paste an image · Esc or Ctrl-C to interrupt"));
    out.line("");
}

pub(super) fn help(out: &Output, skills: &[Skill]) {
    let keys = help_keys();
    for (key, what) in BUILTINS.iter().chain(&keys) {
        out.line(format!("  {} {what}", out.paint("1", &text::pad(key, 16))));
    }
    let names: Vec<String> = skills.iter().filter(|s| s.user_invocable).map(|s| format!("/{}", s.name)).collect();
    if !names.is_empty() {
        out.line(format!("  {}", out.paint("2", &format!("skills: {}", names.join(" ")))));
    }
}

/// The permission mode and rules, line by line.
pub(super) fn permissions(permissions: &Permissions, out: &Output) {
    let mode = permissions.mode();
    out.line(format!("permission mode: {} — {}", out.paint("1", mode.as_str()), mode.describe()));
    for (kind, rules) in permissions.rules() {
        if !rules.is_empty() {
            out.line(format!("  {}: {}", out.paint("2", kind), rules.join(", ")));
        }
    }
    out.line(out.paint("2", "change it with /permissions <mode> or Shift+Tab; rules go in `permissions` in ~/.harness/settings.json"));
}

pub(super) fn tools(out: &Output, tools: &ToolRegistry) {
    let tools = tools.definitions();
    let name_width = tools.iter().map(|t| text::width(&t.name)).max().unwrap_or(0);
    let room = text::terminal_width().saturating_sub(name_width + 5);
    for tool in tools {
        let summary = tool.description.split(". ").next().unwrap_or("").trim_end_matches('.');
        out.line(format!("  {}  {}", out.paint("1", &text::pad(&tool.name, name_width)), out.paint("2", &text::fit(summary, room))));
    }
}

pub(super) fn sandbox(out: &Output) {
    let Some(sandbox) = crate::sandbox::active() else {
        out.line("the sandbox is off; start with --sandbox, HARNESS_SANDBOX=1 or \"sandbox\": {\"enabled\": true} in ~/.harness/settings.json");
        return;
    };
    let startup = ("startup", format!("{} per command (measured at launch)", crate::util::short_duration(sandbox.startup())));
    for (label, value) in sandbox.describe().into_iter().chain([startup]) {
        out.line(format!("  {} {value}", out.paint("1", &text::pad(label, 10))));
    }
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
    Some(format!("about ${:.2} at list prices{left_out}", priced.iter().sum::<f64>()))
}

pub(super) fn usage(out: &Output, agent: &Agent) {
    let models = agent.usage().models();
    if models.is_empty() {
        out.line("no requests yet");
    } else {
        let width = models.iter().map(|(m, _)| text::width(m)).max().unwrap_or(0).max("model".len());
        let header = format!(
            "{}  {:>8}  {:>9}  {:>10}  {:>11}  {:>9}",
            text::pad("model", width),
            "requests",
            "input",
            "cache read",
            "cache write",
            "output"
        );
        out.line(format!("  {}", out.paint("2", &header)));
        for (model, t) in &models {
            let u = &t.usage;
            out.line(format!(
                "  {}  {:>8}  {:>9}  {:>10}  {:>11}  {:>9}",
                out.paint("1", &text::pad(model, width)),
                t.requests,
                output::tokens(u.input_tokens),
                output::tokens(u.cache_read_tokens),
                output::tokens(u.cache_write_tokens),
                output::tokens(u.output_tokens)
            ));
        }
        let input: u64 = models.iter().map(|(_, t)| t.usage.input_tokens).sum();
        let cached: u64 = models.iter().map(|(_, t)| t.usage.cache_read_tokens).sum();
        if let Some(percent) = (cached * 100).checked_div(input) {
            out.line(format!("  {}", out.paint("2", &format!("{percent}% of input tokens came from the prompt cache"))));
        }
        if let Some(cost) = usage_cost(agent) {
            out.line(format!("  {}", out.paint("2", &cost)));
        }
    }
    let used = agent.context_tokens();
    let window = agent.client().context_window();
    let size = match window {
        // A model the providers do not know.
        w if w >= u64::from(u32::MAX) => String::new(),
        w => format!(" of {} ({}%)", output::tokens(w), used * 100 / w.max(1)),
    };
    out.line(format!("context: about {} tokens{size}", output::tokens(used)));
}

pub(super) fn skills(out: &Output, skills: &[Skill]) {
    if skills.is_empty() {
        out.line(
            "no skills or commands found (looked in .harness/skills, .claude/skills, .agents/skills, \
             .harness/commands, .claude/commands and the same under ~)",
        );
    }
    for s in skills {
        out.line(format!("{}  {}", out.paint("1", &s.name), s.description));
        out.line(format!("  {}", out.paint("90", &s.dir.display().to_string())));
    }
}

pub(super) fn models(out: &Output, client: &llm::Client) {
    let providers = client.providers();
    out.line(format!("current: {}", out.paint("1", client.spec())));
    out.line(format!("subagents: {}", client.subagent_model().unwrap_or("follow main model")));
    out.line(format!("effort: {}", client.effort().as_str()));
    for name in providers.names() {
        let protocol = providers.protocol_of(name).map_or("?", llm::Protocol::as_str);
        out.line(format!("provider {} ({protocol})", out.paint("1", name)));
    }
    for m in providers.listed_models() {
        let mark = if m == client.spec() { out.paint("32", "●") } else { " ".to_string() };
        out.line(format!(" {mark} {m}"));
    }
    out.line(out.paint("90", "switch with /model <provider>/<model>; any model name the gateway accepts works"));
    out.line(out.paint("90", "add a provider with /provider add <name>, a model with /provider model add <provider>/<model>"));
}

pub(super) fn agents(out: &Output, defs: &[AgentDef]) {
    for d in defs {
        let access = d.permission.as_str();
        let tools = d.tools.as_ref().map_or("all permitted".to_string(), |t| t.join(", "));
        let source = d.path.as_ref().map_or("built-in".to_string(), |p| p.display().to_string());
        let model = d.model.as_ref().map_or(String::new(), |m| format!(" · model {m}"));
        out.line(format!("{}  [{access}; tools: {tools}]", out.paint("1", &d.name)));
        out.line(format!("  {}", d.description));
        out.line(format!("  {}", out.paint("90", &format!("{source}{model}"))));
    }
}

pub(super) fn tasks(out: &Output, subagents: &[Value], shells: &[(String, String, bool)]) {
    if subagents.is_empty() && shells.is_empty() {
        out.line("no subagents or background commands in this conversation");
        return;
    }
    for s in subagents {
        let status = s["status"].as_str().unwrap_or("");
        let color = match status {
            "running" => "36",
            "completed" => "32",
            "failed" => "31",
            _ => "33",
        };
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
            "  {}  {}  {}  {}",
            out.paint("1", s["id"].as_str().unwrap_or("")),
            out.paint(color, &text::pad(status, 9)),
            text::printable(s["label"].as_str().unwrap_or("")),
            out.paint("2", &info.join(" · "))
        ));
    }
    for (id, command, running) in shells {
        let (color, status) = if *running { ("36", "running") } else { ("2", "ended") };
        let command = text::fit(&text::printable(command.lines().next().unwrap_or("")), 80);
        out.line(format!("  {}  {}  {}", out.paint("1", id), out.paint(color, &text::pad(status, 9)), out.paint("2", &format!("$ {command}"))));
    }
    out.line(out.paint("90", "stop one with /stop <id>, or every running one with /stop"));
}

pub(super) fn plugins(out: &Output, plugins: &Plugins) {
    if plugins.list.is_empty() {
        out.line("no plugins enabled (`enabledPlugins` in ~/.harness/settings.json, or --plugin-dir)");
    }
    for p in &plugins.list {
        let version = p.version.as_ref().map_or(String::new(), |v| format!(" {}", out.paint("2", &format!("v{v}"))));
        match &p.outcome {
            Ok(parts) => {
                let parts = if parts.is_empty() { "nothing the harness can use".to_string() } else { parts.join(", ") };
                out.line(format!("{} {}{version}  {parts}", out.paint("32", "●"), out.paint("1", &p.id)));
            }
            Err(e) => out.line(format!("{} {}  {e}", out.paint("31", "●"), out.paint("1", &p.id))),
        }
        if let Some(description) = &p.description {
            out.line(format!("    {description}"));
        }
        if let Some(root) = &p.root {
            out.line(format!("    {}", out.paint("90", &root.display().to_string())));
        }
        for warning in &p.warnings {
            out.line(format!("    {}", out.paint("33", &format!("! {warning}"))));
        }
        if !p.ignored.is_empty() {
            out.line(format!("    {}", out.paint("2", &format!("not used: {}", p.ignored.join("; ")))));
        }
    }
    out.line(out.paint("90", "manage them with /plugins install|uninstall|enable|disable|configure|marketplace … (see /plugins help)"));
}

/// Pulls the latest of every plugin and marketplace cloned from git.
pub(super) async fn update_plugins(plugins: &Plugins, out: &Output) {
    let plugins = plugins.clone();
    out.line(out.paint("2", "updating plugins…"));
    let lines = match tokio::task::spawn_blocking(move || plugins.update()).await {
        Ok(lines) => lines,
        Err(e) => vec![Err(e.to_string())],
    };
    if lines.is_empty() {
        out.line(out.paint("2", "no plugins come from git repositories"));
        return;
    }
    for line in lines {
        match line {
            Ok(line) => out.line(line),
            Err(e) => out.line(out.paint("31", &e)),
        }
    }
    out.line(out.paint("2", "/reload-plugins (or a restart) uses the new versions"));
}

pub(super) fn mcp<'a>(out: &Output, statuses: impl Iterator<Item = &'a mcp::Status>) {
    let statuses: Vec<&mcp::Status> = statuses.collect();
    if statuses.is_empty() {
        out.line("no MCP servers configured (`mcpServers` in ~/.harness/settings.json)");
    }
    for status in statuses {
        let took = crate::util::short_duration(status.took);
        match &status.outcome {
            Ok(tools) => {
                out.line(format!("{} {} ({} tools, connected in {took})", out.paint("32", "●"), status.name, tools.len()));
                for t in tools {
                    out.line(format!("    {t}"));
                }
            }
            Err(e) => out.line(format!("{} {} failed after {took}: {e}", out.paint("31", "●"), status.name)),
        }
        out.line(format!("    {}", out.paint("90", &format!("log: {}", mcp::log_path(&status.name).display()))));
    }
}
