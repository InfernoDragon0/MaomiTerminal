//! Text that hands a card to an agent, the same as the Workbench's "Copy prompt"
//! (`cardPrompt` in AgentSummary's `shared/ui/src/cardPrompt.ts`). The agent then claims the card
//! itself, so the board's rules keep one card from being taken twice.

/// Longest card title kept in a prompt, in characters.
const MAX_TITLE: usize = 200;

pub fn card_prompt(plan_id: &str, card_id: &str, title: &str) -> String {
    format!(
        "New task: {card_id} (plan {plan_id}) {}. open_card and start it.",
        clip(title, MAX_TITLE)
    )
}

/// `text` cut to `max` characters, the last one an ellipsis when it was cut (as `clip` in view.ts).
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_matches_the_workbench() {
        assert_eq!(
            card_prompt("p-1", "m-2", "Board folder watcher"),
            "New task: m-2 (plan p-1) Board folder watcher. open_card and start it."
        );
    }

    #[test]
    fn long_titles_are_clipped() {
        let title = "x".repeat(250);
        let prompt = card_prompt("p", "m", &title);
        let kept = format!("{}\u{2026}", "x".repeat(199));
        assert_eq!(
            prompt,
            format!("New task: m (plan p) {kept}. open_card and start it.")
        );
        assert_eq!(clip(&"y".repeat(200), 200), "y".repeat(200));
    }
}
