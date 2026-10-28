//! Session names: `<prefix>-<word>-<word>` (for example `laptop-brave-otter`). The prefix is the
//! host name unless `--name-prefix` says otherwise.

use rand::seq::IndexedRandom;

const ADJECTIVES: &[&str] = &[
    "amber", "brave", "calm", "clever", "crisp", "eager", "gentle", "happy", "jolly", "keen",
    "lucid", "merry", "nimble", "proud", "quiet", "rapid", "sunny", "swift", "tidy", "witty",
];

const NOUNS: &[&str] = &[
    "badger", "comet", "falcon", "fern", "harbor", "heron", "lynx", "maple", "meadow", "otter",
    "panda", "pine", "quartz", "river", "sparrow", "summit", "tiger", "willow", "wolf", "zebra",
];

/// The default name prefix: the host name, lower-cased to `[a-z0-9-]`, else `sm`.
pub fn host_prefix() -> String {
    let raw = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
        })
        .unwrap_or_default();
    // `mac.local` -> `mac`: only the first label names the machine.
    let first = raw.trim().split('.').next().unwrap_or_default();
    sanitize(first).unwrap_or_else(|| "sm".to_owned())
}

/// `prefix` as a name part: lower-case letters, digits and `-`; `None` when nothing is left.
pub fn sanitize(prefix: &str) -> Option<String> {
    let cleaned: String = prefix
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let cleaned = cleaned.trim_matches('-').to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A fresh name `prefix-word-word`.
pub fn session_name(prefix: &str) -> String {
    let mut rng = rand::rng();
    let adjective = ADJECTIVES.choose(&mut rng).copied().unwrap_or("calm");
    let noun = NOUNS.choose(&mut rng).copied().unwrap_or("otter");
    format!("{prefix}-{adjective}-{noun}")
}
