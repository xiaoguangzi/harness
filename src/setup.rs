//! What a conversation gets besides its model: the built-in tools, the
//! user's MCP servers, and what the settings and the plugins they enable add
//! (skills and commands, subagent types, hooks, the plugins' MCP servers).
//! Built at startup; `/reload-plugins` builds the second part again, which
//! the next conversation gets: a conversation keeps its tools and system
//! prompt, the prompt cache's prefix.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;

use crate::hooks;
use crate::llm::Client;
use crate::lsp;
use crate::mcp;
use crate::plugins::{self, Checkout, Plugins};
use crate::prompt;
use crate::settings::Settings;
use crate::skills::{self, Skill, SkillTool};
use crate::subagent::{self, AgentDef, AgentTool, Forks, MessageAgentTool, Reporter, StopAgentTool};
use crate::tools::{self, FileTracker, ToolRegistry};

/// What the settings' hooks and the enabled plugins add.
pub struct Extensions {
    pub plugins: Plugins,
    /// Skills and commands, sorted by name.
    pub skills: Arc<Vec<Skill>>,
    pub agent_defs: Arc<Vec<AgentDef>>,
    pub hooks: Arc<hooks::Config>,
    /// The plugins' MCP servers.
    pub mcp: mcp::Manager,
    /// The plugins' language servers, started as they are needed.
    pub lsp: Arc<lsp::Manager>,
    bin_dirs: Vec<PathBuf>,
    /// What was left out, and why.
    pub warnings: Vec<String>,
    /// What marketplaces with `autoUpdate` want fetched.
    pub auto_update: Vec<Checkout>,
}

impl Extensions {
    /// Loads the plugins `settings` enable and those in `plugin_dirs`
    /// (fetching what is missing), the skills, commands and agents, and the
    /// hooks (taken out of `settings`), and starts the plugins' MCP servers.
    pub async fn load(settings: &mut Settings, plugin_dirs: &[PathBuf]) -> Self {
        let loaded = tokio::task::block_in_place(|| plugins::load(settings, plugin_dirs));
        let mut warnings = loaded.warnings;
        for plugin in &loaded.plugins.list {
            if let Err(e) = &plugin.outcome {
                warnings.push(format!("plugin {} not loaded: {e}", plugin.id));
            }
        }
        let mut hooks = std::mem::take(&mut settings.hooks);
        for (event, groups) in loaded.hooks {
            hooks.entry(event).or_default().extend(groups);
        }

        let (mut skills, skipped) = skills::discover(&skills::default_roots());
        warnings.extend(skipped.into_iter().map(|w| format!("skill skipped: {w}")));
        // A skill wins over a command of the same name, as in Claude Code.
        let (commands, skipped) = skills::discover_commands(&skills::default_command_roots());
        warnings.extend(skipped.into_iter().map(|w| format!("command skipped: {w}")));
        for command in commands {
            if !skills.iter().any(|s| s.name == command.name) {
                skills.push(command);
            }
        }
        skills.extend(loaded.skills);
        skills.sort_by(|a, b| a.name.cmp(&b.name));

        let (mut agent_defs, skipped) = subagent::discover(&subagent::default_roots());
        warnings.extend(skipped.into_iter().map(|w| format!("agent skipped: {w}")));
        agent_defs.extend(loaded.agents);
        agent_defs.sort_by(|a, b| a.name.cmp(&b.name));

        // A language server that is not installed would only fail the tool.
        let mut lsp_servers = loaded.lsp_servers;
        lsp_servers.retain(|name, server| {
            let installed = lsp::command_exists(&server.command);
            if !installed {
                warnings.push(format!("LSP server `{name}`: `{}` is not installed or not on PATH; left out", server.command));
            }
            installed
        });
        let mcp = mcp::Manager::start(&loaded.mcp_servers).await;
        for status in &mcp.statuses {
            if let Err(e) = &status.outcome {
                warnings.push(format!("MCP server `{}` failed: {e}", status.name));
            }
        }
        Self {
            plugins: loaded.plugins,
            skills: Arc::new(skills),
            agent_defs: Arc::new(agent_defs),
            hooks: Arc::new(hooks),
            mcp,
            lsp: Arc::new(lsp::Manager::new(lsp_servers)),
            bin_dirs: loaded.bin_dirs,
            warnings,
            auto_update: loaded.auto_update,
        }
    }

    /// Makes its hooks the ones conversations started from now on run, and
    /// its plugins' executables the ones on the model's commands' `PATH`.
    pub fn activate(&self) {
        hooks::set(self.hooks.clone());
        tools::bash::add_to_path(self.bin_dirs.clone());
    }
}

/// Everything a conversation is built from, but its model.
pub struct Env {
    pub ext: Extensions,
    /// Reloaded extensions waiting for the next conversation.
    pending: Option<Extensions>,
    pub user_mcp: mcp::Manager,
    /// The main conversation's file tracker, shared with `@` mentions.
    pub files: FileTracker,
    pub forks: Forks,
    pub reporter: Reporter,
    plugin_dirs: Vec<PathBuf>,
    /// Plugin MCP and language servers no conversation gets any more, to
    /// shut down once the tools holding them are gone.
    retired: Vec<(mcp::Manager, Arc<lsp::Manager>)>,
}

impl Env {
    pub fn new(ext: Extensions, user_mcp: mcp::Manager, reporter: Reporter, plugin_dirs: Vec<PathBuf>) -> Self {
        let (files, forks) = (FileTracker::default(), Forks::default());
        Self { ext, pending: None, user_mcp, files, forks, reporter, plugin_dirs, retired: Vec::new() }
    }

    /// The built-in tools, `skill` and the MCP servers' tools: what
    /// subagents choose from.
    pub fn base_tools(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::with_defaults(self.files.clone());
        let skill_tool = SkillTool::new(&self.ext.skills, self.forks.clone());
        if !skill_tool.is_empty() {
            registry.register(skill_tool);
        }
        self.user_mcp.register(&mut registry);
        self.ext.mcp.register(&mut registry);
        if !self.ext.lsp.is_empty() {
            registry.register(lsp::LspTool::new(self.ext.lsp.clone()));
        }
        registry
    }

    /// The main conversation's tools on `client`, and its system prompt.
    /// Subagents choose from the tools registered before the `agent` tool,
    /// so they never get it.
    pub fn conversation(&self, client: &Client) -> (ToolRegistry, String) {
        let mut registry = self.base_tools();
        let agent_tool = AgentTool::new(self.ext.agent_defs.clone(), registry.clone(), client.clone(), self.reporter.clone());
        let agent_tool = Arc::new(agent_tool);
        self.forks.set(&agent_tool);
        registry.register_arc(agent_tool);
        registry.register(MessageAgentTool);
        registry.register(StopAgentTool);
        let system = prompt::main(&registry);
        (registry, system)
    }

    /// The MCP servers: the user's, then the plugins'.
    pub fn mcp_statuses(&self) -> impl Iterator<Item = &mcp::Status> {
        self.user_mcp.statuses.iter().chain(&self.ext.mcp.statuses)
    }

    /// Loads the plugins, skills, agents and hooks again from the settings
    /// file, for the next conversation; returns what to tell the user.
    pub async fn reload(&mut self) -> Result<Vec<String>> {
        let mut settings = Settings::load()?;
        let ext = Extensions::load(&mut settings, &self.plugin_dirs).await;
        let loaded = ext.plugins.list.iter().filter(|p| p.outcome.is_ok()).count();
        let mut lines = vec![format!(
            "reloaded: {loaded} plugin{}, {} skills and commands, {} agents, {} MCP servers from plugins",
            if loaded == 1 { "" } else { "s" },
            ext.skills.len(),
            ext.agent_defs.len(),
            ext.mcp.statuses.iter().filter(|s| s.outcome.is_ok()).count(),
        )];
        lines.extend(ext.warnings.iter().cloned());
        if let Some(old) = self.pending.replace(ext) {
            self.retired.push((old.mcp, old.lsp));
        }
        Ok(lines)
    }

    /// Whether reloaded extensions wait for the next conversation.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Makes the reloaded extensions the current ones; false if there are
    /// none. Rebuild the conversation's tools after it, then call
    /// [`Env::shutdown_retired`].
    pub fn apply(&mut self) -> bool {
        let Some(ext) = self.pending.take() else { return false };
        ext.activate();
        let old = std::mem::replace(&mut self.ext, ext);
        self.retired.push((old.mcp, old.lsp));
        true
    }

    /// Shuts down the plugin MCP and language servers no conversation uses
    /// now.
    pub async fn shutdown_retired(&mut self) {
        for (mcp, lsp) in std::mem::take(&mut self.retired) {
            lsp.shutdown().await;
            mcp.shutdown().await;
        }
    }

    /// Closes every MCP connection; call it once the conversations are
    /// dropped.
    pub async fn shutdown(mut self) {
        self.shutdown_retired().await;
        if let Some(pending) = self.pending.take() {
            pending.lsp.shutdown().await;
            pending.mcp.shutdown().await;
        }
        self.ext.lsp.shutdown().await;
        self.ext.mcp.shutdown().await;
        self.user_mcp.shutdown().await;
    }
}

/// Fetches the latest of `checkouts` in the background, for the next start;
/// what happened goes to `~/.harness/plugins/auto-update.log`.
pub fn auto_update(checkouts: Vec<Checkout>) {
    let mut unique: Vec<Checkout> = Vec::new();
    for checkout in checkouts {
        if !unique.iter().any(|c| c.dir == checkout.dir) {
            unique.push(checkout);
        }
    }
    if unique.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let log = plugins::home().join("auto-update.log");
        let lines: Vec<String> = unique.iter().map(|c| c.update().unwrap_or_else(|e| format!("{}: {e}", c.label))).collect();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            for line in lines {
                let _ = writeln!(file, "{stamp} {line}");
            }
        }
    });
}

/// `harness --check-plugins`: loads the enabled plugins and those in
/// `plugin_dirs`, starts their MCP servers, and says what loaded, what was
/// left out and why. Fails if a plugin or one of its MCP servers did not
/// load.
pub async fn check_plugins(settings: &Settings, plugin_dirs: &[PathBuf]) -> ExitCode {
    let loaded = tokio::task::block_in_place(|| plugins::load(settings, plugin_dirs));
    let mut failed = 0;
    if loaded.plugins.list.is_empty() {
        println!("no plugins enabled (`enabledPlugins` in the settings, or --plugin-dir)");
    }
    for plugin in &loaded.plugins.list {
        let version = plugin.version.as_ref().map(|v| format!(" v{v}")).unwrap_or_default();
        match &plugin.outcome {
            Ok(parts) => {
                let parts = if parts.is_empty() { "nothing the harness can use".to_string() } else { parts.join(", ") };
                println!("\x1b[32m✓\x1b[0m \x1b[1m{}\x1b[0m{version}  {parts}", plugin.id);
            }
            Err(e) => {
                failed += 1;
                println!("\x1b[31m✗\x1b[0m \x1b[1m{}\x1b[0m  {e}", plugin.id);
            }
        }
        if let Some(root) = &plugin.root {
            println!("    \x1b[90m{}\x1b[0m", root.display());
        }
        for warning in &plugin.warnings {
            println!("    \x1b[33m! {warning}\x1b[0m");
        }
        for part in &plugin.ignored {
            println!("    \x1b[2m- not used: {part}\x1b[0m");
        }
    }
    let skills: Vec<&str> = loaded.skills.iter().map(|s| s.name.as_str()).collect();
    if !skills.is_empty() {
        println!("skills and commands: {}", skills.join(", "));
    }
    let agents: Vec<&str> = loaded.agents.iter().map(|a| a.name.as_str()).collect();
    if !agents.is_empty() {
        println!("agents: {}", agents.join(", "));
    }
    let mcp = mcp::Manager::start(&loaded.mcp_servers).await;
    for status in &mcp.statuses {
        match &status.outcome {
            Ok(tools) => println!(
                "\x1b[32m✓\x1b[0m MCP server {} ({} tools, connected in {})",
                status.name,
                tools.len(),
                crate::util::short_duration(status.took)
            ),
            Err(e) => {
                failed += 1;
                println!("\x1b[31m✗\x1b[0m MCP server {} (after {}): {e}", status.name, crate::util::short_duration(status.took));
                println!("    \x1b[90mlog: {}\x1b[0m", mcp::log_path(&status.name).display());
            }
        }
    }
    mcp.shutdown().await;
    // Language servers start when first asked; this only checks that they can.
    for (name, server) in &loaded.lsp_servers {
        let extensions: Vec<String> = server.extensions.keys().map(|e| format!(".{e}")).collect();
        if lsp::command_exists(&server.command) {
            println!("\x1b[32m✓\x1b[0m LSP server {name} ({}; {})", server.command, extensions.join(", "));
        } else {
            failed += 1;
            println!("\x1b[31m✗\x1b[0m LSP server {name}: `{}` is not installed or not on PATH", server.command);
        }
    }
    let warnings: usize = loaded.plugins.list.iter().map(|p| p.warnings.len()).sum();
    println!(
        "{} plugin{}, {failed} error{}, {warnings} warning{}",
        loaded.plugins.list.len(),
        if loaded.plugins.list.len() == 1 { "" } else { "s" },
        if failed == 1 { "" } else { "s" },
        if warnings == 1 { "" } else { "s" },
    );
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
