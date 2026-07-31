#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            _ => None,
        }
    }

    pub fn all() -> &'static [ThinkingLevel] {
        &[
            Self::Off,
            Self::Minimal,
            Self::Low,
            Self::Medium,
            Self::High,
            Self::XHigh,
        ]
    }

    /// Map onto the OpenAI `reasoning_effort` vocabulary.  `Off` omits the
    /// parameter entirely so the model's autonomous reasoning (and its live
    /// reasoning deltas) still stream.
    pub fn to_reasoning_effort(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ThinkingLevel;

    #[test]
    fn to_reasoning_effort_matches_openai_vocabulary() {
        assert_eq!(ThinkingLevel::Off.to_reasoning_effort(), None);
        assert_eq!(
            ThinkingLevel::Minimal.to_reasoning_effort(),
            Some("minimal")
        );
        assert_eq!(ThinkingLevel::Low.to_reasoning_effort(), Some("low"));
        assert_eq!(ThinkingLevel::Medium.to_reasoning_effort(), Some("medium"));
        assert_eq!(ThinkingLevel::High.to_reasoning_effort(), Some("high"));
        assert_eq!(ThinkingLevel::XHigh.to_reasoning_effort(), Some("xhigh"));
    }

    #[test]
    fn parse_round_trip_known_levels() {
        for level in ThinkingLevel::all() {
            let parsed = ThinkingLevel::parse(level.as_str());
            assert_eq!(parsed, Some(*level));
        }
    }
}
