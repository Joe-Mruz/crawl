//! A minimal, purpose-built substitute for Tornado's template engine,
//! covering exactly the constructs actually used by the WebTiles
//! templates in `crawl-ref/source/webserver/templates/` (`client.html`,
//! `banner.html`, `footer.html`) - not a general-purpose template engine.
//! `game_links.html`/`shutdown.html` (game-links list, admin-only stats,
//! chat-window HTML fragments) are NOT YET PORTED; see `ARCHITECTURE.md`.
//!
//! Supported syntax:
//! - `{{ static_url("path") }}` -> `/static/path`
//! - `{{ name }}` -> looked up in [`TemplateContext::strings`] (empty
//!   string if absent, matching Tornado's default undefined-variable
//!   behavior for these templates)
//! - `{% set ... %}` -> stripped (a no-op here; we substitute the aliased
//!   variable directly instead of evaluating the `set`)
//! - `{% if <bool-key> %} ... [{% else %} ...] {% end %}` -> the
//!   condition is looked up in [`TemplateContext::bools`] (`getattr(config,
//!   "x", False)` style conditions are matched by extracting `x`)
//! - `{% include name.html %}` -> the named template is loaded (relative
//!   to the same template directory) and rendered recursively with the
//!   same context

use std::collections::HashMap;
use std::path::Path;

use crate::error::{Result, WebtilesError};

#[derive(Debug, Clone, Default)]
pub struct TemplateContext {
    pub bools: HashMap<String, bool>,
    pub strings: HashMap<String, String>,
}

impl TemplateContext {
    pub fn with_bool(mut self, key: impl Into<String>, value: bool) -> Self {
        self.bools.insert(key.into(), value);
        self
    }

    pub fn with_string(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.strings.insert(key.into(), value.into());
        self
    }
}

/// Render a template file, resolving `{% include %}` relative to
/// `template_dir`.
pub fn render_file(template_dir: &Path, name: &str, ctx: &TemplateContext) -> Result<String> {
    let path = template_dir.join(name);
    let source = std::fs::read_to_string(&path)
        .map_err(|e| WebtilesError::Internal(format!("failed to read template {name}: {e}")))?;
    render_str(&source, template_dir, ctx)
}

pub fn render_str(source: &str, template_dir: &Path, ctx: &TemplateContext) -> Result<String> {
    let expanded = expand_includes(source, template_dir, ctx)?;
    let expanded = strip_set_tags(&expanded);
    let expanded = eval_conditionals(&expanded, ctx)?;
    Ok(substitute_expressions(&expanded, ctx))
}

fn expand_includes(source: &str, template_dir: &Path, ctx: &TemplateContext) -> Result<String> {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    loop {
        let Some(start) = rest.find("{% include ") else {
            out.push_str(rest);
            break;
        };
        let Some(end) = rest[start..].find("%}") else {
            out.push_str(rest);
            break;
        };
        let end = start + end + 2;
        out.push_str(&rest[..start]);
        let tag_inner = rest[start + "{% include ".len()..end - 2].trim();
        let included = render_file(template_dir, tag_inner, ctx)?;
        out.push_str(&included);
        rest = &rest[end..];
    }
    Ok(out)
}

fn strip_set_tags(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    loop {
        let Some(start) = rest.find("{% set ") else {
            out.push_str(rest);
            break;
        };
        let Some(end) = rest[start..].find("%}") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + end + 2..];
    }
    out
}

/// Evaluate `{% if <cond> %} ... [{% else %} ...] {% end %}` blocks,
/// respecting nesting. `<cond>` is either a bare name (`reset_token`,
/// `reset_token_error`) or `getattr(config, "name", False)`.
fn eval_conditionals(source: &str, ctx: &TemplateContext) -> Result<String> {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    loop {
        let Some(if_start) = rest.find("{% if ") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..if_start]);
        let Some(tag_end) = rest[if_start..].find("%}") else {
            return Err(WebtilesError::Internal("unterminated {% if %} tag".to_string()));
        };
        let tag_end = if_start + tag_end + 2;
        let condition_expr = rest[if_start + "{% if ".len()..tag_end - 2].trim();
        let condition = eval_condition(condition_expr, ctx);

        let (body, remainder) = extract_block(&rest[tag_end..])?;
        let (then_branch, else_branch) = split_else(body);

        let chosen = if condition { then_branch } else { else_branch };
        out.push_str(&eval_conditionals(chosen, ctx)?);
        rest = remainder;
    }
    Ok(out)
}

fn eval_condition(expr: &str, ctx: &TemplateContext) -> bool {
    let name = if let Some(inner) = expr
        .strip_prefix("getattr(config, \"")
        .and_then(|s| s.split('"').next())
    {
        inner
    } else {
        expr
    };
    ctx.bools.get(name).copied().unwrap_or(false)
}

/// Given the text right after an `{% if %}` tag's closing `%}`, find the
/// matching `{% end %}`, respecting nested `{% if %}` blocks, and return
/// `(block_body, text_after_end)`.
fn extract_block(text: &str) -> Result<(&str, &str)> {
    let mut depth = 1usize;
    let mut idx = 0usize;
    loop {
        let next_if = text[idx..].find("{% if ").map(|p| p + idx);
        let next_end = text[idx..].find("{% end %}").map(|p| p + idx);
        match (next_if, next_end) {
            (Some(i), Some(e)) if i < e => {
                depth += 1;
                idx = i + "{% if ".len();
            }
            (_, Some(e)) => {
                depth -= 1;
                if depth == 0 {
                    return Ok((&text[..e], &text[e + "{% end %}".len()..]));
                }
                idx = e + "{% end %}".len();
            }
            _ => return Err(WebtilesError::Internal("unterminated {% if %} block".to_string())),
        }
    }
}

/// Split a top-level `{% if %}` body on a top-level `{% else %}`, if any.
fn split_else(body: &str) -> (&str, &str) {
    // top-level only: skip over any nested if/end pairs while searching.
    let mut depth = 0usize;
    let mut idx = 0usize;
    while idx < body.len() {
        if let Some(rel) = body[idx..].find("{% if ") {
            let if_pos = idx + rel;
            let else_pos = body[idx..].find("{% else %}").map(|p| p + idx);
            let end_pos = body[idx..].find("{% end %}").map(|p| p + idx);
            match (else_pos, end_pos) {
                (Some(e), _) if depth == 0 && e < if_pos => {
                    return (&body[..e], &body[e + "{% else %}".len()..]);
                }
                _ => {}
            }
            if let Some(end_pos) = end_pos {
                if end_pos < if_pos {
                    if depth == 0 {
                        return (body, "");
                    }
                    depth -= 1;
                    idx = end_pos + "{% end %}".len();
                    continue;
                }
            }
            depth += 1;
            idx = if_pos + "{% if ".len();
        } else if let Some(rel) = body[idx..].find("{% else %}") {
            let else_pos = idx + rel;
            if depth == 0 {
                return (&body[..else_pos], &body[else_pos + "{% else %}".len()..]);
            }
            idx = else_pos + "{% else %}".len();
        } else {
            break;
        }
    }
    (body, "")
}

fn substitute_expressions(source: &str, ctx: &TemplateContext) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    loop {
        let Some(start) = rest.find("{{") else {
            out.push_str(rest);
            break;
        };
        let Some(end) = rest[start..].find("}}") else {
            out.push_str(rest);
            break;
        };
        let end = start + end + 2;
        out.push_str(&rest[..start]);
        let expr = rest[start + 2..end - 2].trim();
        out.push_str(&eval_expression(expr, ctx));
        rest = &rest[end..];
    }
    out
}

fn eval_expression(expr: &str, ctx: &TemplateContext) -> String {
    if let Some(arg) = expr
        .strip_prefix("static_url(\"")
        .and_then(|s| s.strip_suffix("\")"))
    {
        return format!("/static/{arg}");
    }
    ctx.strings.get(expr).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir() -> PathBuf {
        PathBuf::from("/nonexistent") // no includes used in these unit tests
    }

    #[test]
    fn static_url_becomes_static_path() {
        let ctx = TemplateContext::default();
        let out = render_str(r#"<script src="{{ static_url("scripts/app.js") }}">"#, &dir(), &ctx).unwrap();
        assert_eq!(out, r#"<script src="/static/scripts/app.js">"#);
    }

    #[test]
    fn simple_string_substitution() {
        let ctx = TemplateContext::default().with_string("socket_server", "ws://x/socket");
        let out = render_str(r#"var s = "{{ socket_server }}";"#, &dir(), &ctx).unwrap();
        assert_eq!(out, r#"var s = "ws://x/socket";"#);
    }

    #[test]
    fn set_tag_is_stripped() {
        let ctx = TemplateContext::default();
        let out = render_str("{% set x = 1 %}rest", &dir(), &ctx).unwrap();
        assert_eq!(out, "rest");
    }

    #[test]
    fn getattr_conditional_true_keeps_content() {
        let ctx = TemplateContext::default().with_bool("allow_password_reset", true);
        let out = render_str(
            r#"a{% if getattr(config, "allow_password_reset", False) %}b{% end %}c"#,
            &dir(),
            &ctx,
        )
        .unwrap();
        assert_eq!(out, "abc");
    }

    #[test]
    fn getattr_conditional_false_drops_content() {
        let ctx = TemplateContext::default(); // defaults to false
        let out = render_str(
            r#"a{% if getattr(config, "allow_password_reset", False) %}b{% end %}c"#,
            &dir(),
            &ctx,
        )
        .unwrap();
        assert_eq!(out, "ac");
    }

    #[test]
    fn nested_if_else_matches_reset_token_flow() {
        let ctx = TemplateContext::default()
            .with_bool("reset_token", true)
            .with_bool("reset_token_error", false)
            .with_string("reset_token", "abc123");
        let template = concat!(
            "{% if reset_token %}",
            "{% if reset_token_error %}ERR{% else %}OK:{{ reset_token }}{% end %}",
            "{% end %}"
        );
        let out = render_str(template, &dir(), &ctx).unwrap();
        assert_eq!(out, "OK:abc123");
    }

    #[test]
    fn nested_if_takes_error_branch() {
        let ctx = TemplateContext::default()
            .with_bool("reset_token", true)
            .with_bool("reset_token_error", true)
            .with_string("reset_token_error", "bad token");
        let template = concat!(
            "{% if reset_token %}",
            "{% if reset_token_error %}ERR:{{ reset_token_error }}{% else %}OK{% end %}",
            "{% end %}"
        );
        let out = render_str(template, &dir(), &ctx).unwrap();
        assert_eq!(out, "ERR:bad token");
    }

    #[test]
    fn outer_false_skips_inner_entirely() {
        let ctx = TemplateContext::default(); // reset_token defaults false
        let template = concat!(
            "before{% if reset_token %}",
            "{% if reset_token_error %}ERR{% else %}OK{% end %}",
            "{% end %}after"
        );
        let out = render_str(template, &dir(), &ctx).unwrap();
        assert_eq!(out, "beforeafter");
    }

    #[test]
    fn renders_real_banner_template_if_present() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../webserver/templates");
        if !dir.join("banner.html").exists() {
            eprintln!("skipping: banner.html not found at {dir:?}");
            return;
        }
        let ctx = TemplateContext::default()
            .with_bool("username", true)
            .with_string("username", "alice");
        let out = render_file(&dir, "banner.html", &ctx).unwrap();
        assert!(out.contains("Welcome to WebTiles!"));
        assert!(out.contains("Hello, alice!"));
    }
}
