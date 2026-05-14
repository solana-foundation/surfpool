use std::env;

/// Returns true when NO_DNA requests non-interactive agent mode.                                                                                                              
/// Truthy values (case-insensitive)
pub fn is_no_dna() -> bool {
    parse_truthy(env::var("NO_DNA").ok().as_deref())
}

fn parse_truthy(value: Option<&str>) -> bool {
    match value {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_truthy;

    #[test]
    fn parse_truthy_values() {
        assert!(parse_truthy(Some("1")));
        assert!(parse_truthy(Some("true")));
        assert!(parse_truthy(Some("TRUE")));
        assert!(parse_truthy(Some("yes")));
        assert!(parse_truthy(Some("on")));
        assert!(parse_truthy(Some(" On ")));
    }

    #[test]
    fn parse_falsey_values() {
        assert!(!parse_truthy(None));
        assert!(!parse_truthy(Some("0")));
        assert!(!parse_truthy(Some("fals e")));
        assert!(!parse_truthy(Some("no")));
        assert!(!parse_truthy(Some("off")));
        assert!(!parse_truthy(Some("")));
    }
}
