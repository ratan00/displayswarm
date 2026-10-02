//! `SPA_PARAM_Tag` key/value tags carried on a stream.
//!
//! Tags are how a producer annotates a stream with information that is neither
//! format nor buffer layout, and how a consumer answers back. They arrive as an
//! ordinary param on the `param_changed` callback, so nothing subscribes to
//! anything extra to receive them.
//!
//! The pod is defined in `spa/param/tag.h` as an object of type
//! `SPA_TYPE_OBJECT_ParamTag` with two properties:
//!
//! ```text
//! SPA_PARAM_TAG_direction : Id   (enum spa_direction)
//! SPA_PARAM_TAG_info      : Struct( Int n_items, (String key, String value)* )
//! ```
//!
//! libspa 0.10 does not wrap this: `ParamType` has no `Tag` constant, and the
//! `spa_tag_parse` / `spa_tag_info_parse` helpers are static inline C that its
//! bindings do not expose. The raw constants and the pod (de)serializer are all
//! present though, so the object is read and written directly here rather than
//! through raw FFI.
//!
//! # Direction
//!
//! A tag carries the direction it describes. Tags a producer publishes about
//! its own output are `Output`; tags a consumer sends back upstream are
//! `Input`. Mutter, for example, publishes `org.gnome.scale` on its virtual
//! monitor stream as an output tag, and reads `org.gnome.preferred-scale` as an
//! input tag to decide what scale to apply.
//!
//! # Lifetime
//!
//! Tags are live stream state, not durable data. A value describes the stream
//! as it is now, is re-delivered when it changes, and is meaningless once the
//! stream is gone. Nothing here persists anything, and a consumer should not
//! either: caching a tag past the stream that published it is the same mistake
//! as caching a buffer mapping past the buffer.

use std::collections::HashMap;
use std::io::Cursor;

use libspa::pod::deserialize::PodDeserializer;
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Pod, Property, Value};
use tracing::{debug, trace};

/// Which side of the stream a set of tags describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagDirection {
    /// Tags describing data flowing out of the producer. What a compositor
    /// publishes about the stream it is producing.
    Output,
    /// Tags flowing back toward the producer. What a consumer asks for.
    Input,
}

impl TagDirection {
    fn from_raw(raw: u32) -> Self {
        // spa_direction: SPA_DIRECTION_INPUT = 0, SPA_DIRECTION_OUTPUT = 1.
        if raw == libspa_sys::SPA_DIRECTION_OUTPUT {
            Self::Output
        } else {
            Self::Input
        }
    }

    fn as_raw(self) -> u32 {
        match self {
            Self::Output => libspa_sys::SPA_DIRECTION_OUTPUT,
            Self::Input => libspa_sys::SPA_DIRECTION_INPUT,
        }
    }
}

/// The tags currently published on a stream, as last delivered by the producer.
///
/// This is a snapshot of live state. It is replaced wholesale each time the
/// producer publishes a new tag param, because that is what the producer sends:
/// a complete set, not a delta.
#[derive(Debug, Clone, Default)]
pub struct StreamTags {
    direction: Option<TagDirection>,
    items: HashMap<String, String>,
}

impl StreamTags {
    /// Look up a tag by key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.items.get(key).map(String::as_str)
    }

    /// Look up a tag and parse it as a float.
    ///
    /// Tag values are always strings on the wire, so numeric tags such as
    /// GNOME's `org.gnome.scale` need parsing. Returns `None` when the key is
    /// absent or its value is not a number.
    #[must_use]
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.parse().ok()
    }

    /// Every tag, for logging or for a consumer that wants to inspect the set.
    #[must_use]
    pub fn items(&self) -> &HashMap<String, String> {
        &self.items
    }

    /// Which side these tags describe, when the producer said.
    #[must_use]
    pub fn direction(&self) -> Option<TagDirection> {
        self.direction
    }

    /// Whether any tags are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Parse a `SPA_PARAM_Tag` pod into its direction and key/value pairs.
///
/// Returns `None` when the pod is not a tag object or cannot be read. A
/// malformed tag is not worth failing a stream over: the producer is
/// annotating, not negotiating, so the correct response is to carry on without
/// the annotation.
#[must_use]
pub fn parse_tag_pod(pod: &Pod) -> Option<StreamTags> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;

    let Value::Object(object) = value else {
        return None;
    };
    if object.type_ != libspa_sys::SPA_TYPE_OBJECT_ParamTag {
        return None;
    }

    let mut tags = StreamTags::default();

    for property in &object.properties {
        match property.key {
            libspa_sys::SPA_PARAM_TAG_direction => {
                if let Value::Id(id) = property.value {
                    tags.direction = Some(TagDirection::from_raw(id.0));
                }
            }
            libspa_sys::SPA_PARAM_TAG_info => {
                let Value::Struct(fields) = &property.value else {
                    continue;
                };
                // Struct( Int n_items, (String key, String value)* ). The
                // declared count is trusted only as far as the strings that
                // actually follow it, so a truncated pod yields fewer pairs
                // rather than an error.
                let mut iter = fields.iter();
                let Some(Value::Int(_n_items)) = iter.next() else {
                    continue;
                };
                while let (Some(Value::String(key)), Some(Value::String(val))) = (iter.next(), iter.next()) {
                    trace!("stream tag {key}={val}");
                    tags.items.insert(key.clone(), val.clone());
                }
            }
            _ => {}
        }
    }

    Some(tags)
}

/// Serialize a `SPA_PARAM_Tag` pod carrying the given key/value pairs.
///
/// Used to answer a producer that reads tags back, which is how a consumer
/// states a preference rather than merely observing one.
///
/// # Errors
///
/// Returns an error if the pod cannot be serialized.
pub fn build_tag_pod(
    direction: TagDirection,
    items: &[(String, String)],
) -> Result<Vec<u8>, crate::error::PipeWireError> {
    let mut info_fields = Vec::with_capacity(items.len() * 2 + 1);
    info_fields.push(Value::Int(i32::try_from(items.len()).unwrap_or(i32::MAX)));
    for (key, value) in items {
        info_fields.push(Value::String(key.clone()));
        info_fields.push(Value::String(value.clone()));
    }

    let object = Object {
        type_: libspa_sys::SPA_TYPE_OBJECT_ParamTag,
        id: libspa_sys::SPA_PARAM_Tag,
        properties: vec![
            Property::new(
                libspa_sys::SPA_PARAM_TAG_direction,
                Value::Id(libspa::utils::Id(direction.as_raw())),
            ),
            Property::new(libspa_sys::SPA_PARAM_TAG_info, Value::Struct(info_fields)),
        ],
    };

    let serialized = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| crate::error::PipeWireError::InvalidParameter(format!("failed to serialize tag pod: {e:?}")))?;

    debug!("built tag pod with {} item(s)", items.len());
    Ok(serialized.0.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_key_value_pairs() {
        let items = vec![
            ("org.gnome.scale".to_string(), "1.5".to_string()),
            ("media.name".to_string(), "screen".to_string()),
        ];
        let bytes = build_tag_pod(TagDirection::Output, &items).expect("serialize");
        let pod = Pod::from_bytes(&bytes).expect("pod from bytes");

        let tags = parse_tag_pod(pod).expect("parse");
        assert_eq!(tags.direction(), Some(TagDirection::Output));
        assert_eq!(tags.get("org.gnome.scale"), Some("1.5"));
        assert_eq!(tags.get("media.name"), Some("screen"));
        assert_eq!(tags.items().len(), 2);
    }

    #[test]
    fn parses_numeric_tags() {
        let items = vec![("org.gnome.scale".to_string(), "2".to_string())];
        let bytes = build_tag_pod(TagDirection::Output, &items).expect("serialize");
        let pod = Pod::from_bytes(&bytes).expect("pod from bytes");

        let tags = parse_tag_pod(pod).expect("parse");
        assert!((tags.get_f64("org.gnome.scale").expect("scale") - 2.0).abs() < f64::EPSILON);
        assert_eq!(tags.get_f64("missing"), None);
    }

    #[test]
    fn input_direction_round_trips() {
        let items = vec![("org.gnome.preferred-scale".to_string(), "1.25".to_string())];
        let bytes = build_tag_pod(TagDirection::Input, &items).expect("serialize");
        let pod = Pod::from_bytes(&bytes).expect("pod from bytes");

        let tags = parse_tag_pod(pod).expect("parse");
        assert_eq!(tags.direction(), Some(TagDirection::Input));
        assert_eq!(tags.get("org.gnome.preferred-scale"), Some("1.25"));
    }

    #[test]
    fn empty_tag_set_is_valid() {
        let bytes = build_tag_pod(TagDirection::Output, &[]).expect("serialize");
        let pod = Pod::from_bytes(&bytes).expect("pod from bytes");

        let tags = parse_tag_pod(pod).expect("parse");
        assert!(tags.is_empty());
    }

    #[test]
    fn rejects_a_pod_that_is_not_a_tag_object() {
        // A Meta param object is a well-formed pod, just not a tag one.
        let object = Object {
            type_: libspa_sys::SPA_TYPE_OBJECT_ParamMeta,
            id: libspa_sys::SPA_PARAM_Meta,
            properties: vec![Property::new(
                libspa_sys::SPA_PARAM_META_type,
                Value::Id(libspa::utils::Id(1)),
            )],
        };
        let serialized = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object)).expect("serialize");
        let bytes = serialized.0.into_inner();
        let pod = Pod::from_bytes(&bytes).expect("pod from bytes");

        assert!(parse_tag_pod(pod).is_none());
    }
}
