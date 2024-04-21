use crate::config::constants::{WAKU_APP_NAME, WAKU_ENCODING, WAKU_ENC_VERSION};

use super::get_current_time_nanos;
use base64::{prelude::BASE64_STANDARD, Engine};
use serde::{Deserialize, Serialize};

/// A Waku message, as defined by [14/WAKU2-MESSAGE](https://github.com/vacp2p/rfc-index/blob/main/waku/standards/core/14/message.md).
#[derive(Serialize, Deserialize, Debug)]
pub struct Message {
    /// The message payload as a base64 (with padding) encoded data string.
    payload: String,
    /// Message content topic for optional content-based filtering.
    content_topic: String,
    /// Message version. Used to indicate type of payload encryption. Default version is 0 (no payload encryption).
    version: u8,
    /// The time at which the message is generated by its sender. This field holds the Unix epoch time in nanoseconds as a 64-bits integer value.
    timestamp: u128,
    /// This flag indicates the transient nature of the message. Indicates if the message is eligible to be stored by the STORE protocol.
    ephemeral: bool,
    // meta: Option<Vec<u8>>, // TODO: metadata?
}

/// Creates a Waku Message with the given message and content topic.
pub fn create_message(payload: impl AsRef<[u8]>, topic: &str, ephemeral: Option<bool>) -> Message {
    Message {
        payload: BASE64_STANDARD.encode(payload),
        content_topic: create_content_topic(topic),
        version: WAKU_ENC_VERSION,
        timestamp: get_current_time_nanos(),
        ephemeral: ephemeral.unwrap_or(false),
    }
}

pub fn parse_message_payload(message: &Message) -> Vec<u8> {
    BASE64_STANDARD
        .decode(&message.payload)
        .expect("Could not decode")
}

/// A [Content Topic](https://docs.waku.org/learn/concepts/content-topics) is represented as a string with the form:
///
/// ```sh
/// /app-name/version/content-topic/encoding
/// /waku/2/default-waku/proto # example
/// ```
///
/// `app-name` defaults to `dria` unless specified otherwise with the second argument.
#[inline]
pub fn create_content_topic(topic: &str) -> String {
    format!(
        "/{}/{}/{}/{}",
        WAKU_APP_NAME, WAKU_ENC_VERSION, topic, WAKU_ENCODING
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_content_topic() {
        let topic = "default-waku";

        let expected = "/dria/0/default-waku/proto".to_string();
        assert_eq!(create_content_topic(topic), expected);
    }

    #[test]
    fn test_create_message() {
        let payload_plain = "Hello, world!";
        let payload = payload_plain.as_bytes();
        let topic = "my-content-topic";
        let message = create_message(payload, topic, None);
        assert_eq!(message.payload, "SGVsbG8sIHdvcmxkIQ=="); // "Hello, world!" in base64
        assert_eq!(message.content_topic, "/dria/0/my-content-topic/proto");

        assert_eq!(message.version, WAKU_ENC_VERSION, "Incorrect version.");
        assert!(!message.ephemeral, "Should not be ephemeral by default.");
        assert!(message.timestamp > 0);

        let payload_decoded = parse_message_payload(&message);
        assert_eq!(payload, payload_decoded.as_slice());
    }
}
