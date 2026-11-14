//! Session names (SPEC §3.2): short and memorable. A new session gets a two-word name
//! (`quiet-otter`) and keeps it until the person renames it or its agent writes a summary — at a
//! checkpoint or at the end (memory.rs puts the name on the summary's first line; it replaces
//! whatever the name was, the person's too). No model call here: a new session's name must be
//! instant and free; the one model call that names it is paid for anyway.

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

pub fn random_name() -> String {
    use rand::RngExt;
    let mut r = rand::rng();
    let a = ADJECTIVES[r.random_range(0..ADJECTIVES.len())];
    let n = NOUNS[r.random_range(0..NOUNS.len())];
    format!("{a}-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_words() {
        let n = random_name();
        assert_eq!(n.split('-').count(), 2, "{n}");
    }
}
