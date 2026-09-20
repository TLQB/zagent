use gpui::actions;

actions!(
    slides,
    [
        /// Generates a slide deck from the given topic, falling back to the
        /// clipboard when absent.
        GenerateSlides { topic: Option<String> },
    ]
);
