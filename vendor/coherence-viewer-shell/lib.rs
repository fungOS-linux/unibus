//! Presentation only: callers own discovery, authorization, and endpoint selection.
pub const CSS: &str = include_str!("shell.css");

#[derive(Clone, Copy)]
pub enum View {
    Mycelium,
    Unibus,
}

/// Only explicit HTTPS entrances are linkable. Absent/invalid entrances remain
/// visibly unavailable; this component never guesses a hostname or listener.
pub fn switcher(active: View, other: Option<&str>) -> String {
    let valid = other.filter(|url| {
        let Some(rest) = url.strip_prefix("https://") else {
            return false;
        };
        let authority = rest.split('/').next().unwrap_or("");
        !authority.is_empty()
            && url.len() <= 2048
            && !url.chars().any(|c| c.is_control() || c.is_whitespace())
            && !url.contains(['@', '\\', '<', '>', '"', '\''])
    });
    let mut html = String::from("<nav class=\"co-viewer-switch\" aria-label=\"Environment view\">");
    for (name, selected) in [
        ("Mycelium", matches!(active, View::Mycelium)),
        ("Unibus", matches!(active, View::Unibus)),
    ] {
        if selected {
            html.push_str(&format!("<a href=\"/\" aria-current=\"page\">{name}</a>"));
        } else if let Some(url) = valid {
            html.push_str(&format!(
                "<a href=\"{}\">{name}</a>",
                url.replace('&', "&amp;")
            ));
        } else {
            html.push_str(&format!("<span aria-disabled=\"true\" title=\"Viewer entrance not configured\">{name}</span>"));
        }
    }
    html.push_str("</nav>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_and_no_guessed_endpoint() {
        let html = switcher(View::Unibus, None);
        assert!(html.contains("aria-current=\"page\">Unibus"));
        assert!(html.contains("aria-disabled=\"true\""));
        assert!(!html.contains("mycelium.myc"));
    }
    #[test]
    fn rejects_credentials_and_injection() {
        for url in [
            "http://mycelium.myc",
            "https://user:secret@host/",
            "https:///",
            "https://host/\" onclick=\"bad",
            "https://host\\evil",
            "javascript:alert(1)",
        ] {
            assert!(switcher(View::Mycelium, Some(url)).contains("aria-disabled"));
        }
        assert!(
            switcher(View::Unibus, Some("https://mycelium.myc/?a=1&b=2")).contains("a=1&amp;b=2")
        );
    }
}
