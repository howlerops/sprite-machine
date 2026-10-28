//! Fluent string catalog (plan §3 cross-cutting: strings in a Fluent catalog, en only at launch).
//!
//! Every user-visible string of the shell is a message in `locales/en/shell.ftl`. Views call
//! [`tr`] / [`tr_args`]; a missing id renders as the id itself (never panics) and is caught by
//! `tests/shell.rs`, which checks every id the code uses against the catalog.

use std::sync::OnceLock;

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource, FluentValue};
use unic_langid::LanguageIdentifier;

/// The English catalog, embedded at build time.
pub const EN_FTL: &str = include_str!("../../locales/en/shell.ftl");

/// A loaded catalog for one locale.
pub struct Catalog {
    bundle: FluentBundle<FluentResource>,
}

impl Catalog {
    /// Parse `source` as the catalog for `locale`. Parse errors are returned, not swallowed.
    pub fn from_source(locale: &str, source: &str) -> Result<Self, String> {
        let lang: LanguageIdentifier = locale
            .parse()
            .map_err(|error| format!("bad locale {locale:?}: {error}"))?;
        let resource = FluentResource::try_new(source.to_string())
            .map_err(|(_, errors)| format!("ftl parse errors: {errors:?}"))?;
        let mut bundle = FluentBundle::new_concurrent(vec![lang]);
        // Isolation marks would show up as stray characters in tests and in labels.
        bundle.set_use_isolating(false);
        bundle
            .add_resource(resource)
            .map_err(|errors| format!("ftl duplicate ids: {errors:?}"))?;
        Ok(Self { bundle })
    }

    /// The built-in English catalog.
    pub fn english() -> Self {
        Self::from_source("en", EN_FTL).expect("locales/en/shell.ftl parses")
    }

    pub fn has(&self, id: &str) -> bool {
        self.bundle.has_message(id)
    }

    /// Format `id` with `args`; `None` when the message is missing or has formatting errors.
    pub fn format(&self, id: &str, args: &[(&str, &str)]) -> Option<String> {
        let message = self.bundle.get_message(id)?;
        let pattern = message.value()?;
        let mut fluent_args = FluentArgs::new();
        for (name, value) in args {
            fluent_args.set(*name, FluentValue::from(*value));
        }
        let mut errors = Vec::new();
        let text = self
            .bundle
            .format_pattern(pattern, Some(&fluent_args), &mut errors);
        errors.is_empty().then(|| text.into_owned())
    }

    /// Format with a visible fallback (the id) so a missing string is obvious, not a crash.
    pub fn tr(&self, id: &str, args: &[(&str, &str)]) -> String {
        self.format(id, args).unwrap_or_else(|| id.to_string())
    }

    /// Every message id in the catalog source (for tests).
    pub fn ids(source: &str) -> Vec<String> {
        source
            .lines()
            .filter(|line| !line.starts_with(['#', ' ', '\t']) && line.contains('='))
            .filter_map(|line| line.split('=').next())
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect()
    }
}

fn global() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(Catalog::english)
}

/// Look up a string without arguments.
pub fn tr(id: &str) -> String {
    global().tr(id, &[])
}

/// Look up a string with named arguments.
pub fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    global().tr(id, args)
}

/// Does the built-in catalog define `id`?
pub fn has(id: &str) -> bool {
    global().has(id)
}
