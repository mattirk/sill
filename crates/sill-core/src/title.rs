//! Session names (SPEC §3.2): short and memorable. A new session gets a two-word name
//! (`quiet-otter`); after the first prompt the name becomes two or three keywords from it when the
//! prompt has any, otherwise the two-word name stays. No model call: names must be instant and free.

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brisk", "calm", "clever", "cosmic", "crisp", "dusty", "eager", "fuzzy", "gentle",
    "golden", "hazy", "humble", "jolly", "keen", "lucid", "lunar", "mellow", "misty", "nimble", "odd",
    "pale", "plain", "quiet", "rapid", "rusty", "sharp", "silent", "sly", "snug", "solar", "stark",
    "steady", "stray", "swift", "tidy", "velvet", "vivid", "wild", "witty", "zesty",
];

const NOUNS: &[&str] = &[
    "otter", "heron", "lynx", "badger", "falcon", "walrus", "ferret", "marten", "osprey", "newt",
    "beetle", "comet", "ember", "fjord", "glacier", "harbor", "island", "jungle", "kestrel", "lantern",
    "meadow", "nebula", "orchard", "pebble", "quarry", "river", "saddle", "thicket", "valley", "willow",
    "anvil", "bramble", "cinder", "dune", "fathom", "grotto", "hollow", "ingot", "kiln", "lagoon",
];

/// Words that say nothing about what a session is for.
const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "in", "on", "at", "to", "for", "with", "from", "by", "as",
    "is", "are", "was", "were", "be", "been", "this", "that", "these", "those", "it", "its", "i", "me",
    "my", "we", "our", "you", "your", "please", "can", "could", "would", "should", "will", "do", "does",
    "did", "make", "create", "add", "write", "check", "look", "let", "lets", "let's", "want", "need",
    "like", "just", "some", "any", "one", "two", "into", "about", "then", "there", "here", "how", "what",
    "which", "when", "where", "why", "if", "so", "also", "very", "really", "new", "file", "files",
    "reply", "done", "line", "containing", "called", "named", "using", "use", "run", "give",
];

pub fn random_name() -> String {
    use rand::RngExt;
    let mut r = rand::rng();
    let a = ADJECTIVES[r.random_range(0..ADJECTIVES.len())];
    let n = NOUNS[r.random_range(0..NOUNS.len())];
    format!("{a}-{n}")
}

/// Two or three keywords from the first prompt, or `None` when it has nothing to say.
pub fn from_prompt(text: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    for raw in text.split_whitespace() {
        let w: String = raw
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.' && c != '-' && c != '_')
            .to_string();
        if w.is_empty() {
            continue;
        }
        // a path contributes its last component
        let w = if w.contains('/') { w.rsplit('/').find(|p| !p.is_empty()).unwrap_or(&w).to_string() } else { w };
        let lower = w.to_lowercase();
        if STOP.contains(&lower.as_str()) || lower.chars().count() < 3 {
            continue;
        }
        if words.iter().any(|x| x.eq_ignore_ascii_case(&lower)) {
            continue;
        }
        words.push(lower);
        if words.len() == 3 {
            break;
        }
    }
    if words.len() < 2 {
        return None;
    }
    let mut title = words.join(" ");
    if title.chars().count() > 28 {
        title = words[..2].join(" ");
    }
    if title.chars().count() > 28 {
        title = title.chars().take(27).collect::<String>() + "…";
    }
    Some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords() {
        assert_eq!(from_prompt("In ~/GIT/workdir-1/sill-shelf create a file m2-test.md containing one line"), Some("sill-shelf m2-test.md".into()));
        assert_eq!(from_prompt("fix the autoscroll bug in conversation view"), Some("fix autoscroll bug".into()));
        assert_eq!(from_prompt("hi"), None);
        assert!(random_name().contains('-'));
    }
}
