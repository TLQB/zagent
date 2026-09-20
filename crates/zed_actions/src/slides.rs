use gpui::Action;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

/// Generates a slide deck from the given topic, falling back to the
/// clipboard text when absent.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = slides)]
#[serde(deny_unknown_fields)]
pub struct GenerateSlides {
    /// The topic describing the deck to generate. When absent, the clipboard
    /// text is used instead.
    pub topic: Option<String>,
}
