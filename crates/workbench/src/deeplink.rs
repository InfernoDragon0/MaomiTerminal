//! `maomi://` deep links, parsed the same way as `parseDesktopLink` in AgentSummary's
//! `shared/core/src/deepLink.ts`:
//!
//! - `maomi://plans?project=<key>&plan=<planId>[&card=<cardId>]`
//! - `maomi://mockups?project=<key>&id=<mockupId>`
//!
//! A link is untrusted input (any program can print one), so parsing is strict: a known route, every
//! key exactly once, ids of the board / mockup shapes. Anything else is `None`, never half a target.

/// What a valid link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    Plan {
        project: String,
        plan: String,
        card: Option<String>,
    },
    Mockup {
        project: String,
        id: String,
    },
}

/// The OS protocol the Workbench desktop app registers.
pub const DESKTOP_LINK_SCHEME: &str = "maomi";

/// Whether `url` uses the `maomi:` scheme (valid or not); such links are never handed to the
/// default URL opener before [`parse_desktop_link`] has looked at them.
pub fn is_desktop_link(url: &str) -> bool {
    url.split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case(DESKTOP_LINK_SCHEME))
}

/// Parses a `maomi://…` link. `maomi://plans?…` carries the route in the host, `maomi:///plans?…`
/// in the path; slashes around it are ignored.
pub fn parse_desktop_link(url: &str) -> Option<DeepLink> {
    let (scheme, rest) = url.split_once(':')?;
    if !scheme.eq_ignore_ascii_case(DESKTOP_LINK_SCHEME) {
        return None;
    }
    let rest = rest.strip_prefix("//")?;
    // A fragment is not part of the query (as in a URL's `search`).
    let rest = rest.split('#').next().unwrap_or_default();
    let (route, query) = rest.split_once('?').unwrap_or((rest, ""));
    // No user info or port: those never belong in these links.
    if route.contains(['@', ':', '\\']) {
        return None;
    }
    parse_route(route.trim_matches('/'), query)
}

fn parse_route(route: &str, query: &str) -> Option<DeepLink> {
    let params = query_pairs(query);
    // One value per key: a repeated key is ambiguous, so the link is refused.
    let one = |key: &str| {
        let mut values = params.iter().filter(|(k, _)| k == key).map(|(_, v)| v);
        match (values.next(), values.next()) {
            (Some(value), None) => Some(value.clone()),
            _ => None,
        }
    };
    let has = |key: &str| params.iter().any(|(k, _)| k == key);
    let project = one("project").filter(|p| is_board_id(p))?;
    match route {
        "plans" => {
            let plan = one("plan").filter(|p| is_board_id(p))?;
            let card = if has("card") {
                Some(one("card").filter(|c| is_board_id(c))?)
            } else {
                None
            };
            Some(DeepLink::Plan {
                project,
                plan,
                card,
            })
        }
        "mockups" => {
            let id = one("id").filter(|id| is_mockup_id(id))?;
            Some(DeepLink::Mockup { project, id })
        }
        _ => None,
    }
}

/// Board project keys and plan / card ids: 1 to 128 of letters, digits, `_` and `-`.
fn is_board_id(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Mockup page ids: a lowercase letter or digit, then up to 63 of those or `-`.
fn is_mockup_id(text: &str) -> bool {
    let lower_or_digit = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = text.as_bytes();
    (1..=64).contains(&bytes.len())
        && lower_or_digit(bytes[0])
        && bytes[1..].iter().all(|&b| lower_or_digit(b) || b == b'-')
}

/// `application/x-www-form-urlencoded` pairs, decoded like `URLSearchParams`.
fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// `+` is a space and `%XX` a byte; a broken escape stays as written, as in `URLSearchParams`.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "f--GitProjects-Automation-AgentSummary";

    fn plan(card: Option<&str>) -> Option<DeepLink> {
        Some(DeepLink::Plan {
            project: KEY.into(),
            plan: "p-mvkq6howw9o5".into(),
            card: card.map(Into::into),
        })
    }

    #[test]
    fn plan_and_card_links_parse() {
        let base = format!("maomi://plans?project={KEY}&plan=p-mvkq6howw9o5");
        assert_eq!(parse_desktop_link(&base), plan(None));
        assert_eq!(
            parse_desktop_link(&format!("{base}&card=m-mvkq6how40nw")),
            plan(Some("m-mvkq6how40nw"))
        );
        // The route may sit in the path, with slashes around it, and the scheme in any case.
        assert_eq!(
            parse_desktop_link(&format!(
                "MAOMI:///plans/?project={KEY}&plan=p-mvkq6howw9o5#x"
            )),
            plan(None)
        );
        // Escapes decode before the id check.
        assert_eq!(
            parse_desktop_link(&format!(
                "maomi://plans?project={KEY}&plan=p%2Dmvkq6howw9o5"
            )),
            plan(None)
        );
    }

    #[test]
    fn mockup_links_parse() {
        assert_eq!(
            parse_desktop_link(&format!("maomi://mockups?project={KEY}&id=workbench")),
            Some(DeepLink::Mockup {
                project: KEY.into(),
                id: "workbench".into()
            })
        );
        assert_eq!(
            parse_desktop_link(&format!("maomi://mockups?project={KEY}&id=Workbench")),
            None
        );
    }

    #[test]
    fn anything_off_shape_is_refused() {
        for url in [
            "https://example.com/plans?project=a&plan=b",
            "maomi:plans?project=a&plan=b",
            "maomi://plans?project=a",
            "maomi://plans?plan=b",
            "maomi://plans?project=a&plan=b&plan=c",
            "maomi://plans?project=a&project=a&plan=b",
            "maomi://plans?project=a&plan=b&card=",
            "maomi://plans?project=a&plan=b&card=x&card=y",
            "maomi://plans?project=a&plan=b%20c",
            "maomi://plans?project=a/..&plan=b",
            "maomi://Plans?project=a&plan=b",
            "maomi://boards?project=a&plan=b",
            "maomi://user@plans?project=a&plan=b",
            "maomi://plans:80?project=a&plan=b",
            "maomi://plans/extra?project=a&plan=b",
        ] {
            assert_eq!(parse_desktop_link(url), None, "{url}");
        }
        let long = "a".repeat(129);
        assert_eq!(
            parse_desktop_link(&format!("maomi://plans?project={long}&plan=b")),
            None
        );
    }

    #[test]
    fn scheme_check_ignores_case() {
        assert!(is_desktop_link("Maomi://anything"));
        assert!(!is_desktop_link("https://maomi.dev"));
        assert!(!is_desktop_link("maomi"));
    }

    #[test]
    fn decoding_matches_url_search_params() {
        assert_eq!(decode("a+b"), "a b");
        assert_eq!(decode("%41%2d"), "A-");
        assert_eq!(decode("%zz%4"), "%zz%4");
        assert_eq!(decode("100%"), "100%");
    }
}
