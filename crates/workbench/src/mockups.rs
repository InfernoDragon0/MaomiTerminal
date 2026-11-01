//! Mockup Studio data: the design mockups agents save in a project's `.maomi/mockups` folder.
//!
//! Layout (written by the board's MCP server; read-only here):
//! - `_system/system.json`, `tokens.css`, `components.css`, `theme.html`, `components.html`
//! - `pages/<id>/meta.json`, `pages/<id>/index.html` (latest), `pages/<id>/versions/v<n>.html`
//!
//! [`preview_html`] wraps a page in the preview shell ported from AgentSummary
//! (`previewHtml.ts`): the page runs in a sandboxed iframe (`allow-scripts` only) under a CSP
//! with no network, so it can be opened in any browser from a local file.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::Engine as _;
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// The mockups folder of a project folder.
pub fn mockups_dir(project_root: &Path) -> PathBuf {
    project_root.join(".maomi").join("mockups")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MockupStatus {
    Draft,
    Review,
    Approved,
    SentBack,
    #[serde(other)]
    Other,
}

impl MockupStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Review => "In review",
            Self::Approved => "Approved",
            Self::SentBack => "Sent back",
            Self::Other => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MockupVersion {
    pub v: u32,
    /// Unix milliseconds.
    #[serde(default)]
    pub at: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub note: Option<String>,
}

/// A plan (and maybe its card) that uses a page. The first one is the page's home plan.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MockupPlanLink {
    pub plan: String,
    #[serde(default)]
    pub milestone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MockupPage {
    pub id: String,
    pub title: String,
    pub status: MockupStatus,
    pub version: u32,
    /// Kept versions, oldest first.
    #[serde(default)]
    pub versions: Vec<MockupVersion>,
    #[serde(default)]
    pub approved_version: Option<u32>,
    #[serde(default)]
    pub plans: Vec<MockupPlanLink>,
    /// Older meta.json files have a single plan link instead of `plans`.
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    milestone: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub updated_at: u64,
}

impl MockupPage {
    /// The plan that first saved the page.
    pub fn home_plan(&self) -> Option<&str> {
        self.plans.first().map(|link| link.plan.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MockupComponent {
    pub name: String,
    #[serde(default)]
    pub cls: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MockupSystem {
    pub version: u32,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub themes: Vec<String>,
    #[serde(default)]
    pub components: Vec<MockupComponent>,
    #[serde(default)]
    pub rules: Vec<String>,
}

/// One project's mockups. `system` is None before the design system exists.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MockupIndex {
    pub system: Option<MockupSystem>,
    /// Newest first.
    pub pages: Vec<MockupPage>,
}

/// Page ids are folder names: lowercase letters, digits and dashes, so a target never leaves
/// the mockups folder.
pub fn is_mockup_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Reads every page and the design system. Broken or missing files are skipped.
pub fn read_mockups(dir: &Path) -> MockupIndex {
    let system = fs::read(dir.join("_system").join("system.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let mut pages: Vec<MockupPage> = fs::read_dir(dir.join("pages"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            is_mockup_id(&name).then_some(())?;
            read_page(dir, &name)
        })
        .collect();
    pages.sort_by_key(|page| std::cmp::Reverse(page.updated_at));
    MockupIndex { system, pages }
}

fn read_page(dir: &Path, id: &str) -> Option<MockupPage> {
    let bytes = fs::read(dir.join("pages").join(id).join("meta.json")).ok()?;
    let mut page: MockupPage = serde_json::from_slice(&bytes)
        .inspect_err(|err| tracing::warn!(id, %err, "skipped a broken mockup meta.json"))
        .ok()?;
    if page.id != id {
        return None;
    }
    if page.plans.is_empty()
        && let Some(plan) = page.plan.take()
    {
        page.plans.push(MockupPlanLink {
            plan,
            milestone: page.milestone.take(),
        });
    }
    Some(page)
}

/// What a preview shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockupTarget {
    /// A page; None = its latest version.
    Page { id: String, version: Option<u32> },
    /// The design system's generated palette page.
    Theme,
    /// The design system's component gallery.
    Components,
}

/// The HTML file a target shows.
pub fn target_file(dir: &Path, target: &MockupTarget, latest: Option<u32>) -> Option<PathBuf> {
    match target {
        MockupTarget::Page { id, version } => {
            if !is_mockup_id(id) {
                return None;
            }
            let page = dir.join("pages").join(id);
            Some(match version {
                Some(v) if Some(*v) != latest => page.join("versions").join(format!("v{v}.html")),
                _ => page.join("index.html"),
            })
        }
        MockupTarget::Theme => Some(dir.join("_system").join("theme.html")),
        MockupTarget::Components => Some(dir.join("_system").join("components.html")),
    }
}

/// The preview shell for `target`, ready to save as an .html file and open in a browser.
pub fn preview_html(
    dir: &Path,
    target: &MockupTarget,
    title: &str,
    latest: Option<u32>,
) -> io::Result<String> {
    let file = target_file(dir, target, latest)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a mockup id"))?;
    let page = fs::read_to_string(file)?;
    let system = dir.join("_system");
    let read = |name: &str| fs::read_to_string(system.join(name)).unwrap_or_default();
    let html = inline_system_css(&page, &read("tokens.css"), &read("components.css"));
    let themes = read_mockups_system(dir)
        .map(|s| s.themes)
        .unwrap_or_default();
    let hashes = inline_script_hashes(&html);
    Ok(preview_shell(title, &html, &themes, &new_nonce(), &hashes))
}

fn read_mockups_system(dir: &Path) -> Option<MockupSystem> {
    let bytes = fs::read(dir.join("_system").join("system.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 16 random bytes, base64: authorises the shell's own script.
fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No OS randomness: the time still makes the nonce unguessable for a page written once.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        bytes = nanos.to_le_bytes();
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

static TOKENS_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<link\b[^>]*\bhref\s*=\s*["']?(?:\.\./\.\./_system/)?tokens\.css["']?[^>]*>"#)
        .expect("valid regex")
});
static COMPONENTS_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)<link\b[^>]*\bhref\s*=\s*["']?(?:\.\./\.\./_system/)?components\.css["']?[^>]*>"#,
    )
    .expect("valid regex")
});
static STYLE_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</style").expect("valid regex"));
static INLINE_SCRIPT_BODY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script\b([^>]*)>(.*?)</script\s*>").expect("valid regex"));
static SRC_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\ssrc\s*=").expect("valid regex"));

/// The preview cannot read files, so the system stylesheets go into the page: tokens always
/// (first when the page does not link them), components.css only where the page links it.
fn inline_system_css(html: &str, tokens: &str, components: &str) -> String {
    let tokens = STYLE_END.replace_all(tokens, r"<\/style");
    let components = STYLE_END.replace_all(components, r"<\/style");
    let token_style = format!("<style data-maomi=\"tokens\">\n{tokens}\n</style>");
    let out = if TOKENS_LINK.is_match(html) {
        TOKENS_LINK
            .replace_all(html, regex::NoExpand(&token_style))
            .into_owned()
    } else {
        format!("{token_style}\n{html}")
    };
    let component_style = format!("<style data-maomi=\"components\">\n{components}\n</style>");
    COMPONENTS_LINK
        .replace_all(&out, regex::NoExpand(&component_style))
        .into_owned()
}

/// sha256 (base64) of each inline script: the CSP lets exactly these run when Scripts is on.
fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for cap in INLINE_SCRIPT_BODY.captures_iter(html) {
        if SRC_ATTR.is_match(&cap[1]) {
            continue;
        }
        let text = cap[2].replace("\r\n", "\n").replace('\r', "\n");
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(text));
        if seen.insert(hash.clone()) {
            out.push(hash);
        }
    }
    out
}

/// Turns a fragment link into a hash change and drops every other link (a srcdoc page would
/// otherwise load the shell into the frame). Kept byte-identical to AgentSummary's.
const LINK_GUARD: &str = r##"document.addEventListener("click",function(e){var a=e.target&&e.target.closest&&e.target.closest("a[href]");if(!a||e.defaultPrevented)return;var h=a.getAttribute("href")||"";e.preventDefault();if(h.charAt(0)==="#")location.hash=h.slice(1)})"##;

fn preview_csp(nonce: &str) -> String {
    [
        "default-src 'none'",
        "style-src 'unsafe-inline'",
        &format!("script-src 'nonce-{nonce}'"),
        "img-src data:",
        "font-src data:",
        "media-src data:",
        "form-action 'none'",
        "base-uri 'none'",
    ]
    .join("; ")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// JSON that is safe inside an inline `<script>`.
fn script_json(value: &serde_json::Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

const SHELL_CSS: &str = r#":root { color-scheme: light dark; --b: #16161c; --p: #1e1e26; --f: #c8c8d2; --m: #8e8ea2; --l: rgba(127,127,127,.3); --a: #8b7bff; --af: #fff; --ok: #4cc38a; }
* { box-sizing: border-box; }
html, body { margin: 0; height: 100%; background: var(--b); color: var(--f); font: 12.5px/1.4 "Segoe UI", system-ui, sans-serif; }
body { display: flex; flex-direction: column; }
.bar { flex: none; display: flex; flex-wrap: wrap; align-items: center; gap: 6px 12px; padding: 6px 10px; background: var(--p); border-bottom: 1px solid var(--l); }
.t { font-weight: 600; max-width: 40ch; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.seg { display: inline-flex; border: 1px solid var(--l); border-radius: 6px; overflow: hidden; }
.seg button, .btn { font: inherit; font-size: 12px; padding: 2px 10px; color: var(--f); background: transparent; border: 0; cursor: pointer; }
.seg button + button { border-left: 1px solid var(--l); }
.seg button[aria-pressed="true"] { background: var(--a); color: var(--af); }
.btn { border: 1px solid var(--l); border-radius: 6px; }
label { display: inline-flex; gap: 6px; align-items: center; color: var(--m); cursor: pointer; }
.safe { color: var(--m); margin-left: auto; }
.safe b { color: var(--ok); font-weight: 600; }
iframe { flex: 1; width: 100%; border: 0; background: #fff0; }"#;

/// The shell's script; `__SRC__` and friends are filled in by [`preview_shell`]. It builds the
/// srcdoc the same way as AgentSummary's shell: a CSP meta first, the theme on `<html>`, the
/// nonce on inline scripts when Scripts is on, then the link guard.
const SHELL_SCRIPT: &str = r#"
// No literal less-than sign below (hex escapes instead): markup-like text in a script can trip the HTML parser.
(function () {
  var SRC = __SRC__;
  var N = __NONCE__;
  var BASE = __BASE__;
  var HASHES = __HASHES__;
  var GUARD = __GUARD__;
  var GUARD_HASH = __GUARD_HASH__;
  function innerCsp() {
    var ok = scripts.checked ? HASHES.filter(function (h) { return typeof h === "string" && /^[A-Za-z0-9+\/]+=*$/.test(h); }) : [];
    var src = [GUARD_HASH].concat(ok).map(function (h) { return "'sha256-" + h + "'"; }).join(" ");
    return BASE.replace("'nonce-" + N + "'", src);
  }
  var theme = __THEME__;
  var view = document.getElementById("view");
  var scripts = document.getElementById("scripts");
  function esc(s) { return String(s).replace(/&/g, "&amp;").replace(/\x3c/g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"); }
  function build() {
    var doc = SRC, attr = ' data-theme="' + esc(theme) + '"';
    if (/\x3chtml\b[^>]*>/i.test(doc)) {
      doc = doc.replace(/\x3chtml\b([^>]*)>/i, function (_m, a) { return "\x3chtml" + a.replace(/\sdata-theme\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi, "") + attr + ">"; });
    } else {
      doc = "\x3chtml" + attr + ">\x3cbody>" + doc + "\x3c/body>\x3c/html>";
    }
    if (scripts.checked) doc = doc.replace(/\x3cscript\b(?![^>]*\ssrc\s*=)/gi, '\x3cscript nonce="' + N + '"');
    doc += '\x3cscript nonce="' + N + '">' + GUARD + "\x3c/script>";
    return '\x3cmeta http-equiv="Content-Security-Policy" content="' + innerCsp() + '">\n' + doc;
  }
  function render() {
    // A fresh iframe each time: a page stuck in a loop is dropped with its frame.
    var next = view.cloneNode(false);
    next.srcdoc = build();
    view.parentNode.replaceChild(next, view);
    view = next;
  }
  document.querySelector(".seg").addEventListener("click", function (e) {
    var b = e.target.closest && e.target.closest("[data-theme-btn]");
    if (!b) return;
    theme = b.getAttribute("data-theme-btn");
    document.querySelectorAll("[data-theme-btn]").forEach(function (x) { x.setAttribute("aria-pressed", String(x === b)); });
    render();
  });
  scripts.addEventListener("change", render);
  document.getElementById("reload").addEventListener("click", render);
  render();
})();
"#;

/// sha256 (base64) of [`LINK_GUARD`]; a test checks it.
const LINK_GUARD_HASH: &str = "ZmM+le69pXd60W1qkkW+w1o2c8u/F5WczmD5Hyl6p2o=";

fn preview_shell(
    title: &str,
    html: &str,
    themes: &[String],
    nonce: &str,
    hashes: &[String],
) -> String {
    let themes: Vec<String> = if themes.is_empty() {
        vec!["default".into()]
    } else {
        themes.to_vec()
    };
    let buttons: String = themes
        .iter()
        .enumerate()
        .map(|(i, t)| {
            format!(
                r#"<button type="button" data-theme-btn="{0}" aria-pressed="{1}">{0}</button>"#,
                esc(t),
                i == 0
            )
        })
        .collect();
    let json = |v: serde_json::Value| script_json(&v);
    let script = SHELL_SCRIPT
        .replace("__SRC__", &json(html.into()))
        .replace("__NONCE__", &json(nonce.into()))
        .replace("__BASE__", &json(preview_csp(nonce).into()))
        .replace("__HASHES__", &json(hashes.into()))
        .replace("__GUARD_HASH__", &json(LINK_GUARD_HASH.into()))
        .replace("__GUARD__", &json(LINK_GUARD.into()))
        .replace("__THEME__", &json(themes[0].clone().into()));
    let csp = preview_csp(nonce);
    let title = esc(title);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="{csp}">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
{SHELL_CSS}
</style>
</head>
<body>
<div class="bar">
  <span class="t">{title}</span>
  <span class="seg" role="group" aria-label="Theme">{buttons}</span>
  <label><input type="checkbox" id="scripts" checked> Scripts</label>
  <button class="btn" type="button" id="reload" title="Stop the page and load it again">↻ Reload</button>
  <span class="safe"><b>●</b> Sandboxed · no network</span>
</div>
<iframe id="view" title="Mockup" sandbox="allow-scripts" referrerpolicy="no-referrer"></iframe>
<script nonce="{nonce}">{script}</script>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_guard_hash_matches() {
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(LINK_GUARD));
        assert_eq!(hash, LINK_GUARD_HASH);
    }

    #[test]
    fn ids_stay_inside_the_folder() {
        assert!(is_mockup_id("repo-tab"));
        assert!(!is_mockup_id("../x"));
        assert!(!is_mockup_id("-x"));
        assert!(!is_mockup_id("Repo"));
        assert!(!is_mockup_id(""));
    }

    #[test]
    fn tokens_replace_the_link_or_go_first() {
        let linked = r#"<link rel="stylesheet" href="../../_system/tokens.css"><p>x</p>"#;
        let out = inline_system_css(linked, ":root{--a:1}", "");
        assert!(out.starts_with("<style data-maomi=\"tokens\">"));
        assert!(!out.contains("tokens.css"));
        let bare = inline_system_css("<p>x</p>", "a</style>", ".c{}");
        assert!(bare.starts_with("<style data-maomi=\"tokens\">\na<\\/style"));
        assert!(!bare.contains("data-maomi=\"components\""));
    }

    #[test]
    fn hashes_cover_inline_scripts_only() {
        let html = "<script>a()\r\n</script><script src=\"x.js\"></script><script type=module>a()\n</script>";
        let hashes = inline_script_hashes(html);
        assert_eq!(hashes.len(), 1, "same text after newline fixes counts once");
        let want = base64::engine::general_purpose::STANDARD.encode(Sha256::digest("a()\n"));
        assert_eq!(hashes[0], want);
    }

    #[test]
    fn shell_escapes_the_page() {
        let shell = preview_shell("A <b>", "</script><script>x</script>", &[], "N", &[]);
        assert!(shell.contains("<title>A &lt;b&gt;</title>"));
        assert!(!shell.contains("</script><script>x"));
        assert!(shell.contains("script-src 'nonce-N'"));
        assert!(shell.contains(r#"data-theme-btn="default""#));
    }

    #[test]
    fn reads_pages_newest_first_with_legacy_links() {
        let dir = std::env::temp_dir().join(format!("maomi-mockups-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let page = |id: &str, json: &str| {
            let d = dir.join("pages").join(id);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("meta.json"), json).unwrap();
        };
        page(
            "old",
            r#"{"id":"old","title":"Old","status":"approved","version":1,"updatedAt":1,"plan":"p-1","milestone":"m-1"}"#,
        );
        page(
            "new",
            r#"{"id":"new","title":"New","status":"review","version":2,"updatedAt":5,"plans":[{"plan":"p-2"}],"versions":[{"v":1,"at":1,"bytes":3},{"v":2,"at":2,"bytes":4,"note":"n"}]}"#,
        );
        page("bad", "{");
        page(
            "other",
            r#"{"id":"mismatch","title":"x","status":"draft","version":1}"#,
        );
        let index = read_mockups(&dir);
        let ids: Vec<_> = index.pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["new", "old"]);
        assert_eq!(index.pages[1].plans[0].milestone.as_deref(), Some("m-1"));
        assert_eq!(index.pages[0].status, MockupStatus::Review);
        assert!(index.system.is_none());
        let latest = target_file(
            &dir,
            &MockupTarget::Page {
                id: "new".into(),
                version: Some(2),
            },
            Some(2),
        )
        .unwrap();
        assert!(latest.ends_with("index.html"));
        let _ = fs::remove_dir_all(&dir);
    }
}
