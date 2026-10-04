use crate::ProviderError;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Event {
    pub event: String,
    pub data: String,
    pub id: String,
    pub retry: Option<u64>,
}

/// Line-oriented framing holds bytes until a complete UTF-8 line arrives.
/// No reconnect logic is needed because each provider call is one attempt.
#[derive(Default)]
pub(super) struct Parser {
    line: Vec<u8>,
    skip_lf: bool,
    first_line: bool,
    current: Event,
    has_data: bool,
    last_id: String,
    retry: Option<u64>,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>, ProviderError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.line(&mut events)?;
                self.skip_lf = byte == b'\r';
            } else {
                self.line.push(byte);
            }
        }
        Ok(events)
    }

    fn line(&mut self, events: &mut Vec<Event>) -> Result<(), ProviderError> {
        let bytes = std::mem::take(&mut self.line);
        let mut line = std::str::from_utf8(&bytes)
            .map_err(|_| ProviderError::Protocol("invalid SSE UTF-8".into()))?;
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            if self.has_data {
                self.current.data.pop(); // Remove the final newline added per data field.
                if self.current.event.is_empty() {
                    self.current.event = "message".into();
                }
                self.current.id.clone_from(&self.last_id);
                self.current.retry = self.retry;
                events.push(std::mem::take(&mut self.current));
            } else {
                self.current = Event::default();
            }
            self.has_data = false;
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.current.event = value.into(),
            "data" => {
                self.current.data.push_str(value);
                self.current.data.push('\n');
                self.has_data = true;
            }
            "id" if !value.contains('\0') => self.last_id = value.into(),
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                if let Ok(retry) = value.parse() {
                    self.retry = Some(retry);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_split_point_and_single_byte_chunks() {
        let input = "\u{feff}: comment\r\nevent: custom\r\nid: 12\r\nretry: 150\r\ndata: hé🌍\r\ndata: second\r\n\r\ndata:\n\nevent: ignored\n\nid: bad\0id\nretry: -1\ndata: tail\r\r";
        let expected = vec![
            Event {
                event: "custom".into(),
                data: "hé🌍\nsecond".into(),
                id: "12".into(),
                retry: Some(150),
            },
            Event {
                event: "message".into(),
                data: "".into(),
                id: "12".into(),
                retry: Some(150),
            },
            Event {
                event: "message".into(),
                data: "tail".into(),
                id: "12".into(),
                retry: Some(150),
            },
        ];
        for split in 0..=input.len() {
            let mut parser = Parser::new();
            let mut actual = parser.feed(&input.as_bytes()[..split]).unwrap();
            actual.extend(parser.feed(&input.as_bytes()[split..]).unwrap());
            assert_eq!(actual, expected, "split {split}");
        }
        let mut parser = Parser::new();
        let actual: Vec<_> = input
            .as_bytes()
            .chunks(1)
            .flat_map(|chunk| parser.feed(chunk).unwrap())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn arbitrary_three_way_splits() {
        let input = "data: €\r\n\r\ndata: 2\n\n".as_bytes();
        for first in 0..=input.len() {
            for second in first..=input.len() {
                let mut parser = Parser::new();
                let mut events = parser.feed(&input[..first]).unwrap();
                events.extend(parser.feed(&input[first..second]).unwrap());
                events.extend(parser.feed(&input[second..]).unwrap());
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].data, "€");
                assert_eq!(events[1].data, "2");
            }
        }
    }

    #[test]
    fn invalid_utf8_and_incomplete_frames() {
        let mut parser = Parser::new();
        assert!(parser.feed(b"data: unfinished").unwrap().is_empty());
        let mut parser = Parser::new();
        assert!(matches!(
            parser.feed(b"data: \xff\n\n"),
            Err(ProviderError::Protocol(_))
        ));
    }
}
