use super::table;

/// Words contain characters, each represented by its dot/dash pattern.
#[derive(Debug, PartialEq, Eq)]
pub struct Message(pub Vec<Vec<&'static str>>);

pub fn encode(text: &str) -> Result<Message, String> {
    let words = text
        .split_whitespace()
        .map(|word| {
            word.chars()
                .map(|c| {
                    table::lookup(c.to_ascii_uppercase())
                        .ok_or_else(|| format!("Unsupported Morse character: {c:?}"))
                })
                .collect()
        })
        .collect::<Result<Vec<Vec<_>>, _>>()?;
    if words.is_empty() {
        return Err("Enter at least one Morse character".into());
    }
    Ok(Message(words))
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let words: Vec<_> = self.0.iter().map(|w| w.join(" ")).collect();
        write!(f, "{}", words.join("   "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encodes_and_normalizes() {
        assert_eq!(
            encode(" hello\n W8ABC ").unwrap().to_string(),
            ".... . .-.. .-.. ---   .-- ---.. .- -... -.-."
        );
    }
    #[test]
    fn rejects_empty_and_unknown() {
        assert!(encode(" \n").is_err());
        assert!(encode("HI🦀").is_err());
        assert!(encode("é").is_err());
    }
}
