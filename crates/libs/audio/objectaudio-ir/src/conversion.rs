/// Losses recorded at a format or consumer boundary. Repeated properties are
/// reported once per presentation, rather than once per metadata event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversionReport {
    pub approximations: Vec<String>,
}

impl ConversionReport {
    pub fn record(&mut self, property: &str, reason: &str) {
        let message = format!("{property}: {reason}");
        if !self.approximations.contains(&message) {
            self.approximations.push(message);
        }
    }
}
