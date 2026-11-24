//! Settings → Agents: local MCP enable/pause switches, active per-Host in-memory Grants, Saved
//! connection availability with `~/.ssh/config` guidance, and a copyable MCP config JSON snippet
//! populated with tern's full executable path (ADR 0003, ADR 0004).

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use gpui::prelude::FluentBuilder;
use gpui::{
    ClipboardItem, Context, InteractiveElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};
use serde::Serialize;

use super::{Section, Shell};
use crate::hover::Accessible as _;
use crate::icons::{self, icon};
use crate::settings_widgets as w;
use crate::theme::{MONO_FONT, SPACE_LG, SPACE_SM};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpConfigSnippet<'a> {
    mcp_servers: McpServers<'a>,
}

#[derive(Serialize)]
struct McpServers<'a> {
    tern: McpServerEntry<'a>,
}

#[derive(Serialize)]
struct McpServerEntry<'a> {
    command: String,
    args: &'a [&'a str],
}

/// Normalizes `.` and `..` components without following symlinks so Homebrew or app-bundle
/// paths remain stable across upgrades.
fn clean_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn fallback_executable_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Applications/tern.app/Contents/MacOS/tern")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\tern\tern.exe")
    } else {
        PathBuf::from("/usr/local/bin/tern")
    }
}

/// Resolves an absolute executable path from `exe` and `cwd`, never returning a bare `"tern"`.
#[must_use]
pub(crate) fn resolve_executable_path(exe: Option<PathBuf>, cwd: Option<PathBuf>) -> PathBuf {
    if let Some(path) = exe.filter(|p| !p.as_os_str().is_empty()) {
        if path.is_absolute() {
            return clean_lexical(&path);
        }
        if let Some(dir) = cwd.filter(|d| d.is_absolute()) {
            return clean_lexical(&dir.join(path));
        }
    }
    fallback_executable_path()
}

/// Full path to the running `tern` binary for embedding in MCP config snippets.
#[must_use]
pub(crate) fn current_executable_path() -> PathBuf {
    resolve_executable_path(std::env::current_exe().ok(), std::env::current_dir().ok())
}

/// Builds a pretty-printed MCP configuration JSON snippet invoking `executable` with `["mcp"]`.
#[must_use]
pub(crate) fn mcp_config_json(executable: &Path) -> String {
    let payload = McpConfigSnippet {
        mcp_servers: McpServers {
            tern: McpServerEntry {
                command: executable.to_string_lossy().into_owned(),
                args: &["mcp"],
            },
        },
    };
    serde_json::to_string_pretty(&payload).unwrap_or_default()
}

impl Shell {
    pub(super) fn agents_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let enabled = self.agents_enabled();
        let paused = self.agents_paused();
        let store_error = self.agent_store_error().map(str::to_owned);
        let grants = self.active_agent_grants();
        let saved_count = self.connections.len();
        let snippet = mcp_config_json(&current_executable_path());
        let now = SystemTime::now();

        let enable_desc = if store_error.is_some() {
            "Unavailable while agents.json cannot be read; fix or move the file so tern will not overwrite it"
        } else {
            "Listen on the local agent.sock for tern mcp requests while tern is open"
        };
        let mcp_card = w::card(&t)
            .child(w::row(
                &t,
                true,
                "Enable MCP",
                Some(enable_desc.into()),
                div()
                    .id("toggle-agents-enabled")
                    .switch("Enable MCP", enabled)
                    .cursor_pointer()
                    .on_click(cx.listener(|s, _, _, cx| {
                        let next = !s.agents_enabled();
                        if let Err(e) = s.set_agents_enabled(next, cx) {
                            s.alert_failed(e, cx);
                        }
                    }))
                    .child(w::toggle(&t, enabled, "agents-enabled")),
            ))
            .child(w::row(
                &t,
                false,
                "Pause agents",
                Some(
                    "Reject new Agent requests, cancel pending Decisions, and clear active Grants"
                        .into(),
                ),
                div()
                    .id("toggle-agents-paused")
                    .switch("Pause agents", paused)
                    .cursor_pointer()
                    .on_click(cx.listener(|s, _, _, cx| {
                        let next = !s.agents_paused();
                        if let Err(e) = s.set_agents_paused(next, cx) {
                            s.alert_failed(e.to_string(), cx);
                        }
                    }))
                    .child(w::toggle(&t, paused, "agents-paused")),
            ));

        let mut grants_card = w::card(&t);
        if grants.is_empty() {
            grants_card = grants_card.child(w::row(
                &t,
                true,
                "No active Grants",
                Some(
                    "Approving a Decision with Allow host until lock adds an in-memory Grant you can revoke here; Grants clear on sleep, screen lock, or quit"
                        .into(),
                ),
                div(),
            ));
        } else {
            let count = grants.len();
            let title = format!(
                "{count} active {}",
                if count == 1 { "Grant" } else { "Grants" }
            );
            grants_card = grants_card.child(w::row(
                &t,
                true,
                title,
                Some(
                    "Cleared automatically on sleep, screen lock, session switch, quit, or Host edits"
                        .into(),
                ),
                w::button(&t, "agents-revoke-all", "Revoke active Grants")
                    .text_color(t.danger)
                    .on_click(cx.listener(|s, _, _, cx| {
                        let n = s.revoke_all_agent_grants(cx);
                        if n > 0 {
                            let noun = if n == 1 { "Grant" } else { "Grants" };
                            s.activity(format!("Revoked {n} active {noun}"), cx);
                        }
                    })),
            ));
            for (ix, grant) in grants.into_iter().enumerate() {
                let host = grant.host.clone();
                let when = crate::files_text::modified(grant.granted_at, now);
                grants_card = grants_card.child(w::row(
                    &t,
                    false,
                    grant.host,
                    Some(SharedString::from(format!("Granted {when}"))),
                    w::button(&t, ("agent-revoke-grant", ix), "Revoke").on_click(cx.listener(
                        move |s, _, _, cx| {
                            if s.revoke_agent_grant(&host, cx) {
                                s.activity(format!("Revoked Grant for {host}"), cx);
                            }
                        },
                    )),
                ));
            }
        }

        let hosts_title = if saved_count == 0 {
            "No Saved connections yet".to_owned()
        } else if saved_count == 1 {
            "1 Saved connection available".to_owned()
        } else {
            format!("{saved_count} Saved connections available")
        };
        let hosts_card = w::card(&t)
            .child(w::row(
                &t,
                true,
                hosts_title,
                Some(
                    "Agents can list and request commands on all Saved connections in hosts.json. Every Host requires a Decision unless an active Grant exists."
                        .into(),
                ),
                w::button(&t, "agents-open-connections", "Connections").on_click(
                    cx.listener(|s, _, _, cx| s.select_section(Section::Connections, cx)),
                ),
            ))
            .child(w::row(
                &t,
                false,
                "~/.ssh/config Hosts",
                Some(
                    "Only Saved connections are exposed to Agents. Import or duplicate a ~/.ssh/config Host as a Saved connection first."
                        .into(),
                ),
                w::button(&t, "agents-import-ssh-config", "Import from ~/.ssh/config…")
                    .on_click(cx.listener(|s, _, window, cx| s.open_import(window, cx))),
            ));

        let copy_snippet = snippet.clone();
        let code_lines: Vec<SharedString> = snippet
            .lines()
            .map(|line| SharedString::from(line.to_owned()))
            .collect();
        let config_card = w::card(&t)
            .child(w::row(
                &t,
                true,
                "MCP config JSON",
                Some(
                    "Paste into your Agent's MCP configuration. Embeds tern's full executable path so it works without tern on PATH."
                        .into(),
                ),
                w::button(&t, "agents-copy-mcp-json", "Copy JSON")
                    .child(icon(icons::COPY).size(px(14.)).text_color(t.muted))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_snippet.clone()));
                        s.activity("Copied MCP config JSON", cx);
                    })),
            ))
            // Code block geometry (`docs/ui.md`, Agents): `SPACE_LG` side margin matching `w::row`,
            // 12 bottom/side inset matching `w::row`'s vertical rhythm, `SPACE_SM` top/bottom pad
            // and corner radius, and `w::row`'s 12/16 description typography in `MONO_FONT`.
            .child(
                div()
                    .mx(px(SPACE_LG))
                    .mb(px(12.))
                    .px(px(12.))
                    .py(px(SPACE_SM))
                    .rounded(px(SPACE_SM))
                    .bg(t.ink(0.05))
                    .border_1()
                    .border_color(t.hairline)
                    .font_family(MONO_FONT)
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .text_color(t.text)
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .children(code_lines.into_iter().map(|line| {
                        div().w_full().min_w_0().child(line)
                    })),
            );

        w::page_column()
            .child(w::page_header(&t, "Agents"))
            .child(w::page_subtitle(
                &t,
                "Let local Agents run commands on your Saved connections without sharing SSH keys or passwords. Agent settings (agents.json) stay on this Mac only and are never synced.",
            ))
            .when_some(store_error, |el, err| {
                el.child(w::page_subtitle(&t, err).text_color(t.danger))
            })
            .child(w::section(&t, "Local MCP", mcp_card))
            .child(w::section(&t, "Active Grants", grants_card))
            .child(w::section(&t, "Hosts", hosts_card))
            .child(w::section(&t, "MCP configuration", config_card))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::Value;

    use super::*;

    #[test]
    fn mcp_config_json_embeds_full_absolute_path_and_args_mcp() {
        let exe = Path::new("/Applications/tern.app/Contents/MacOS/tern");
        let json = mcp_config_json(exe);
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["mcpServers"]["tern"]["command"],
            "/Applications/tern.app/Contents/MacOS/tern"
        );
        assert_eq!(
            parsed["mcpServers"]["tern"]["args"],
            serde_json::json!(["mcp"])
        );
        assert_ne!(parsed["mcpServers"]["tern"]["command"], "tern");
    }

    #[test]
    fn mcp_config_json_escapes_spaces_quotes_and_backslashes_as_valid_json() {
        let tricky = Path::new(r#"/Users/Alice "A"/My Apps/dir\name/tern"#);
        let json = mcp_config_json(tricky);
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["mcpServers"]["tern"]["command"].as_str(),
            Some(r#"/Users/Alice "A"/My Apps/dir\name/tern"#)
        );
        assert_eq!(
            parsed["mcpServers"]["tern"]["args"],
            serde_json::json!(["mcp"])
        );
    }

    #[test]
    fn resolve_executable_path_keeps_absolute_and_resolves_relative_against_cwd() {
        let abs = resolve_executable_path(
            Some(PathBuf::from("/opt/homebrew/bin/./tern")),
            Some(PathBuf::from("/tmp")),
        );
        assert_eq!(abs, PathBuf::from("/opt/homebrew/bin/tern"));
        assert!(abs.is_absolute());

        let rel = resolve_executable_path(
            Some(PathBuf::from("./target/debug/../debug/tern")),
            Some(PathBuf::from("/Users/dev/tern")),
        );
        assert_eq!(rel, PathBuf::from("/Users/dev/tern/target/debug/tern"));
        assert!(rel.is_absolute());
    }

    #[test]
    fn resolve_executable_path_fallback_is_absolute_and_never_bare_tern() {
        let fallback = resolve_executable_path(None, None);
        assert!(
            fallback.is_absolute(),
            "fallback must be absolute: {fallback:?}"
        );
        assert_ne!(fallback, PathBuf::from("tern"));

        let rel_without_cwd = resolve_executable_path(Some(PathBuf::from("tern")), None);
        assert!(rel_without_cwd.is_absolute());
        assert_ne!(rel_without_cwd, PathBuf::from("tern"));
    }
}
