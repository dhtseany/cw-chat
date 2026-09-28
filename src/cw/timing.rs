use crate::morse::encoder::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub tone: bool,
    pub units: u32,
}

/// Gaps replace each other: a word gap is seven units total, never 3 + 7.
pub fn schedule(message: &Message) -> Vec<Event> {
    let mut events = Vec::new();
    for (wi, word) in message.0.iter().enumerate() {
        if wi > 0 {
            events.push(Event {
                tone: false,
                units: 7,
            });
        }
        for (ci, character) in word.iter().enumerate() {
            if ci > 0 {
                events.push(Event {
                    tone: false,
                    units: 3,
                });
            }
            for (ei, element) in character.chars().enumerate() {
                if ei > 0 {
                    events.push(Event {
                        tone: false,
                        units: 1,
                    });
                }
                events.push(Event {
                    tone: true,
                    units: if element == '.' { 1 } else { 3 },
                });
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::morse::encoder::encode;
    #[test]
    fn standard_spacing() {
        let actual: Vec<_> = schedule(&encode("AE E").unwrap())
            .iter()
            .map(|e| (e.tone, e.units))
            .collect();
        assert_eq!(
            actual,
            [
                (true, 1),
                (false, 1),
                (true, 3),
                (false, 3),
                (true, 1),
                (false, 7),
                (true, 1)
            ]
        );
    }
    #[test]
    fn paris_is_fifty_units_with_word_gap() {
        assert_eq!(
            schedule(&encode("PARIS").unwrap())
                .iter()
                .map(|e| e.units)
                .sum::<u32>()
                + 7,
            50
        );
    }
}
