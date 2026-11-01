//! Record selectors shared by `search --kind` and `edit`.
//!
//! Grammar: comma-separated union of
//! `kind:<tag>` · `index:<a>` | `index:<a>-<b>` · `id:<record-id>` ·
//! `matching:<regex>` (matched against the record's flat text).

use regex::Regex;

use crate::{Error, Record, RecordKind, Result};

#[derive(Debug)]
pub enum Selector {
    Kind(RecordKind),
    Index(std::ops::RangeInclusive<u64>),
    Id(String),
    Matching(Regex),
}

impl Selector {
    fn matches(&self, record: &Record) -> bool {
        match self {
            Selector::Kind(k) => record.kind() == *k,
            Selector::Index(r) => r.contains(&record.ordinal),
            Selector::Id(id) => record.id.as_deref() == Some(id.as_str()),
            Selector::Matching(re) => re.is_match(&record.text()),
        }
    }
}

/// The union of parsed selectors: a record is selected when *any* member
/// matches.
#[derive(Debug)]
pub struct Selectors(pub Vec<Selector>);

impl Selectors {
    pub fn parse(spec: &str) -> Result<Self> {
        let mut out = Vec::new();
        for part in spec.split(',') {
            let part = part.trim();
            let (tag, value) = part.split_once(':').ok_or_else(|| {
                Error::Selector(part.to_string(), "expected <tag>:<value>".into())
            })?;
            out.push(match tag {
                "kind" => Selector::Kind(
                    RecordKind::parse(value)
                        .ok_or_else(|| Error::Selector(part.into(), "unknown kind".into()))?,
                ),
                "index" => {
                    let (a, b) = match value.split_once('-') {
                        Some((a, b)) => (a, b),
                        None => (value, value),
                    };
                    let (a, b) = (a.trim().parse(), b.trim().parse());
                    match (a, b) {
                        (Ok(a), Ok(b)) if a <= b => Selector::Index(a..=b),
                        _ => return Err(Error::Selector(part.into(), "bad index range".into())),
                    }
                }
                "id" => Selector::Id(value.to_string()),
                "matching" => Selector::Matching(
                    Regex::new(value).map_err(|e| Error::Selector(part.into(), e.to_string()))?,
                ),
                _ => return Err(Error::Selector(part.into(), "unknown selector tag".into())),
            });
        }
        if out.is_empty() {
            return Err(Error::Selector(spec.into(), "empty selector".into()));
        }
        Ok(Selectors(out))
    }

    /// Ordinals selected within `records`.
    pub fn apply(&self, records: &[Record]) -> Vec<u64> {
        records
            .iter()
            .filter(|r| self.0.iter().any(|s| s.matches(r)))
            .map(|r| r.ordinal)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, Conversation};

    fn conv() -> Conversation {
        let mut c = Conversation::new("s1");
        c.records = vec![
            Record {
                id: Some("a".into()),
                parent: None,
                ts: None,
                ordinal: 0,
                body: Body::User {
                    text: "hello".into(),
                    attachments: vec![],
                },
            },
            Record {
                id: Some("b".into()),
                parent: Some("a".into()),
                ts: None,
                ordinal: 1,
                body: Body::Reasoning { text: "hmm".into() },
            },
            Record {
                id: Some("c".into()),
                parent: Some("b".into()),
                ts: None,
                ordinal: 2,
                body: Body::Assistant {
                    text: "world".into(),
                    model: None,
                },
            },
        ];
        c
    }

    #[test]
    fn parse_and_apply() {
        assert_eq!(
            Selectors::parse("kind:reasoning")
                .unwrap()
                .apply(&conv().records),
            vec![1]
        );
        assert_eq!(
            Selectors::parse("index:0-1")
                .unwrap()
                .apply(&conv().records),
            vec![0, 1]
        );
        assert_eq!(
            Selectors::parse("id:c").unwrap().apply(&conv().records),
            vec![2]
        );
        assert_eq!(
            Selectors::parse("matching:^wor")
                .unwrap()
                .apply(&conv().records),
            vec![2]
        );
        assert_eq!(
            Selectors::parse("kind:user,index:2")
                .unwrap()
                .apply(&conv().records),
            vec![0, 2]
        );
    }

    #[test]
    fn rejects_bad() {
        assert!(Selectors::parse("kind:nope").is_err());
        assert!(Selectors::parse("index:5-2").is_err());
        assert!(Selectors::parse("bogus").is_err());
        assert!(Selectors::parse("matching:[").is_err());
    }
}
