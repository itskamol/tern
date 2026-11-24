//! Allowed systemd service unit editing for Saved connections in [`ConnectionForm`].
//!
//! Keeps input validation, deduplication, Saved-connection gating, and chip-list rendering out of
//! `shell_connections.rs` so that file stays below the 800-line ceiling.

use gpui::prelude::FluentBuilder;
use gpui::{
    AppContext as _, Context, Entity, Focusable as _, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};

use super::ConnectionForm;
use crate::agents::store::{MAX_UNIT_NAME_LEN, validate_unit_name};
use crate::connections::Connection;
use crate::hover::HoverFade as _;
use crate::shell::Shell;
use crate::text_input::TextInput;
use crate::theme::MONO_FONT;

/// Placeholder text for the Saved connection's allowed-service input.
pub(super) const SERVICE_HINT: &str = "e.g. api.service or worker@1.service";

/// Outcome of validating and adding one systemd unit name to a form draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AddOutcome {
    Added,
    AlreadyPresent,
}

/// Validates a trimmed systemd unit name and returns a human-readable inline error on failure.
pub(super) fn validate_service_error(candidate: &str) -> Result<(), String> {
    if validate_unit_name(candidate).is_ok() {
        return Ok(());
    }
    if candidate.is_empty() {
        return Err("Enter a systemd service unit name, e.g. api.service.".into());
    }
    if candidate.starts_with('-') {
        return Err("Service unit name cannot start with '-'.".into());
    }
    if candidate.len() > MAX_UNIT_NAME_LEN {
        return Err(format!(
            "Service unit name must be at most {MAX_UNIT_NAME_LEN} characters."
        ));
    }
    Err("Use only ASCII letters, digits, and : _ @ . - (no spaces or shell characters).".into())
}

/// Validates and appends `raw` to `list` without duplicating existing entries.
///
/// # Errors
/// Returns an inline validation message when `raw` is empty or fails [`validate_unit_name`],
/// leaving `list` unchanged.
pub(super) fn add_service(list: &mut Vec<String>, raw: &str) -> Result<AddOutcome, String> {
    let candidate = raw.trim();
    validate_service_error(candidate)?;
    if list.iter().any(|existing| existing == candidate) {
        return Ok(AddOutcome::AlreadyPresent);
    }
    list.push(candidate.to_owned());
    Ok(AddOutcome::Added)
}

/// Produces the final validated, deduplicated service list when saving a Saved connection.
///
/// Any non-empty `pending_input` still in the text box is validated and folded in so valid
/// entries are never silently lost and invalid input is never silently dropped.
///
/// # Errors
/// Returns an inline validation message if any existing item or non-empty `pending_input` is
/// invalid.
pub(super) fn finalize_services(
    list: &[String],
    pending_input: &str,
) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(list.len() + 1);
    for unit in list {
        add_service(&mut out, unit)?;
    }
    if !pending_input.trim().is_empty() {
        add_service(&mut out, pending_input)?;
    }
    Ok(out)
}

/// Returns `true` only when `host` is a Saved connection (`hosts.json`), never a `~/.ssh/config`
/// entry that has not been imported or saved first.
#[must_use]
#[allow(dead_code)] // Pure gating helper tested alongside store::AgentConfig checks.
pub(super) fn host_is_saved_connection(host: &str, saved: &[Connection]) -> bool {
    saved.iter().any(|c| c.name == host)
}

/// Form state for editing a Saved connection's Agent-allowed systemd service units.
pub(super) struct AllowedServicesField {
    pub(super) services: Vec<String>,
    initial: Vec<String>,
    original_host: Option<String>,
    pub(super) input: Entity<TextInput>,
    last_text: String,
    pub(super) inline_error: Option<String>,
}

impl AllowedServicesField {
    pub(super) fn new(
        original_host: Option<String>,
        services: Vec<String>,
        input: Entity<TextInput>,
    ) -> Self {
        Self {
            initial: services.clone(),
            services,
            original_host,
            input,
            last_text: String::new(),
            inline_error: None,
        }
    }

    /// Whether saving `final_services` under `saved_name` needs an `agents.json` write.
    pub(super) fn needs_save(&self, final_services: &[String], saved_name: &str) -> bool {
        if self.initial != final_services {
            return true;
        }
        self.original_host
            .as_deref()
            .is_some_and(|old| old != saved_name && !self.initial.is_empty())
    }
}

impl Shell {
    /// Builds the form's Allowed services field pre-populated for `editing` (if editing a Saved
    /// connection) and returns an observer clearing stale inline errors as the person types.
    pub(super) fn new_allowed_services_field(
        &self,
        editing: Option<usize>,
        cx: &mut Context<Self>,
    ) -> (AllowedServicesField, Subscription) {
        let original_host = editing
            .and_then(|ix| self.connections.get(ix))
            .map(|c| c.name.clone());
        let initial = original_host
            .as_deref()
            .map(|h| self.agent_allowed_services(h))
            .unwrap_or_default();
        let colors = self.theme.input_colors();
        let input = cx.new(|cx| TextInput::new(SERVICE_HINT.to_owned(), false, colors, cx));
        let sub = cx.observe(&input, |shell: &mut Self, input, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(form) = shell.form.as_mut()
                && form.allowed_services.last_text != text
            {
                form.allowed_services.last_text = text;
                form.allowed_services.inline_error = None;
            }
            cx.notify();
        });
        (
            AllowedServicesField::new(original_host, initial, input),
            sub,
        )
    }

    /// Handles Enter when focus is inside the Allowed services input with non-empty text.
    /// Returns `true` if handled as an add instead of submitting the whole form.
    pub(super) fn try_enter_allowed_service(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let adding = self.form.as_ref().is_some_and(|f| {
            f.allowed_services.input.focus_handle(cx).is_focused(window)
                && !f.allowed_services.input.read(cx).text().trim().is_empty()
        });
        if adding {
            self.add_form_allowed_service(cx);
        }
        adding
    }

    /// Validates the form's allowed services before saving `saved_name`.
    /// Returns `Ok(Some(list))` when `agents.json` should be updated, `Ok(None)` when unchanged.
    pub(super) fn validate_form_allowed_services(
        &mut self,
        saved_name: &str,
        cx: &gpui::App,
    ) -> Result<Option<Vec<String>>, String> {
        let Some(form) = self.form.as_mut() else {
            return Ok(None);
        };
        let pending = form.allowed_services.input.read(cx).text().to_owned();
        let final_services = match finalize_services(&form.allowed_services.services, &pending) {
            Ok(svcs) => svcs,
            Err(e) => {
                form.allowed_services.inline_error = Some(e.clone());
                return Err(e);
            }
        };
        if !form
            .allowed_services
            .needs_save(&final_services, saved_name)
        {
            return Ok(None);
        }
        if let Some(e) = self.agents.store_error.clone() {
            return Err(e);
        }
        Ok(Some(final_services))
    }

    /// Validates and adds the text currently typed in the Allowed services input.
    pub(super) fn add_form_allowed_service(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let raw = form.allowed_services.input.read(cx).text().to_owned();
        match add_service(&mut form.allowed_services.services, &raw) {
            Ok(_) => {
                form.allowed_services.inline_error = None;
                let input = form.allowed_services.input.clone();
                input.update(cx, |i, cx| i.set_text(String::new(), cx));
            }
            Err(msg) => {
                form.allowed_services.inline_error = Some(msg);
            }
        }
        cx.notify();
    }

    /// Removes one allowed service chip from the open connection form.
    pub(super) fn remove_form_allowed_service(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(form) = self.form.as_mut()
            && ix < form.allowed_services.services.len()
        {
            form.allowed_services.services.remove(ix);
            form.allowed_services.inline_error = None;
        }
        cx.notify();
    }

    /// Renders the Saved connection editor's "Allowed services (Agents)" section.
    pub(super) fn allowed_services_section(
        &self,
        form: &ConnectionForm,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = &self.theme;
        let inline_error = form.allowed_services.inline_error.clone();
        let store_error = self.agent_store_error().map(str::to_owned);
        let has_error = inline_error.is_some();
        let chips = div().flex().flex_wrap().gap(px(6.)).children(
            form.allowed_services
                .services
                .iter()
                .enumerate()
                .map(|(ix, unit)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .pl(px(8.))
                        .pr(px(4.))
                        .py(px(3.))
                        .rounded(px(6.))
                        .bg(t.row_hover)
                        .border_1()
                        .border_color(t.hairline)
                        .child(
                            div()
                                .font_family(MONO_FONT)
                                .text_xs()
                                .text_color(t.text)
                                .child(SharedString::from(unit.clone())),
                        )
                        .child(
                            div()
                                .id(("form-remove-service", ix))
                                .size(px(18.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.))
                                .text_xs()
                                .text_color(t.muted)
                                .cursor_pointer()
                                .hover(|s| s.bg(t.row_active).text_color(t.text))
                                .on_click(cx.listener(move |shell, _, _, cx| {
                                    shell.remove_form_allowed_service(ix, cx);
                                }))
                                .child("×"),
                        )
                }),
        );

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .pt(px(4.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(t.muted)
                            .child("Allowed services (Agents)"),
                    )
                    .child(div().text_xs().text_color(t.faint).child(
                        "Systemd units Agents may inspect on this Saved connection (local to this Mac only).",
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(30.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(if has_error { t.danger } else { t.border })
                            .bg(t.row_hover)
                            .overflow_hidden()
                            .text_sm()
                            .child(form.allowed_services.input.clone()),
                    )
                    .child(
                        div()
                            .id("form-add-service")
                            .flex_none()
                            .h(px(30.))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(t.border)
                            .text_xs()
                            .text_color(t.text)
                            .cursor_pointer()
                            .hover_fade("form-add-service", t.row_hover, t.row_active)
                            .on_click(cx.listener(|shell, _, _, cx| {
                                shell.add_form_allowed_service(cx);
                            }))
                            .child("Add"),
                    ),
            )
            .when_some(inline_error, |el, msg| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(t.danger)
                        .child(SharedString::from(msg)),
                )
            })
            .when_some(store_error, |el, msg| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(t.danger)
                        .child(SharedString::from(msg)),
                )
            })
            .when(!form.allowed_services.services.is_empty(), |el| {
                el.child(chips)
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn conn(name: &str) -> Connection {
        Connection {
            name: name.into(),
            host: format!("{name}.internal"),
            port: 22,
            user: "deploy".into(),
            ..Connection::default()
        }
    }

    #[test]
    fn add_service_trims_valid_units_and_deduplicates_without_duplicates() {
        let mut list = Vec::new();
        assert_eq!(
            add_service(&mut list, "  api.service  "),
            Ok(AddOutcome::Added)
        );
        assert_eq!(
            add_service(&mut list, "worker@1.service"),
            Ok(AddOutcome::Added)
        );
        assert_eq!(
            add_service(&mut list, "postgresql@16-main.service"),
            Ok(AddOutcome::Added)
        );
        assert_eq!(
            add_service(&mut list, "app_web:prod.service"),
            Ok(AddOutcome::Added)
        );
        assert_eq!(
            add_service(&mut list, "api.service"),
            Ok(AddOutcome::AlreadyPresent)
        );
        assert_eq!(
            list,
            vec![
                "api.service",
                "worker@1.service",
                "postgresql@16-main.service",
                "app_web:prod.service",
            ]
        );
    }

    #[test]
    fn add_service_rejects_empty_leading_dash_and_shell_chars_with_clear_error() {
        let mut list = vec!["api.service".to_owned()];

        let empty_err = add_service(&mut list, "   ").unwrap_err();
        assert!(empty_err.contains("systemd service unit"));

        let dash_err = add_service(&mut list, "-p").unwrap_err();
        assert!(dash_err.contains("'-'"));

        let double_dash = add_service(&mut list, "--all").unwrap_err();
        assert!(double_dash.contains("'-'"));

        for bad in [
            "api.service; id",
            "api service",
            "$(whoami)",
            "api/service",
            "café.service",
        ] {
            let err = add_service(&mut list, bad).unwrap_err();
            assert!(
                err.contains("ASCII"),
                "expected ASCII character guidance for {bad:?}, got {err:?}"
            );
        }

        let too_long = "a".repeat(MAX_UNIT_NAME_LEN + 1);
        let len_err = add_service(&mut list, &too_long).unwrap_err();
        assert!(len_err.contains(&MAX_UNIT_NAME_LEN.to_string()));

        assert_eq!(
            list,
            vec!["api.service"],
            "invalid adds must leave list untouched"
        );
    }

    #[test]
    fn finalize_services_folds_valid_pending_input_and_never_silently_drops_invalid_input() {
        let existing = vec!["api.service".to_owned()];

        assert_eq!(
            finalize_services(&existing, "   "),
            Ok(vec!["api.service".to_owned()])
        );
        assert_eq!(
            finalize_services(&existing, " worker@1.service "),
            Ok(vec![
                "api.service".to_owned(),
                "worker@1.service".to_owned()
            ])
        );
        assert_eq!(
            finalize_services(&existing, "api.service"),
            Ok(vec!["api.service".to_owned()])
        );
        assert!(
            finalize_services(&existing, "-bad-flag").is_err(),
            "invalid pending input must fail validation, never be silently dropped"
        );
        assert!(finalize_services(&existing, "api; rm -rf /").is_err());
    }

    #[test]
    fn saved_connection_gate_allows_only_saved_connections_and_rejects_ssh_config_only_hosts() {
        let saved = vec![conn("staging"), conn("prod")];
        assert!(host_is_saved_connection("staging", &saved));
        assert!(host_is_saved_connection("prod", &saved));
        assert!(!host_is_saved_connection("ssh-config-only", &saved));
    }
}
