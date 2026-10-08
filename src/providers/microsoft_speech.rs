//! Speech SDK-compatible STT framing, without a second key or a Foundry deployment.
//! Wire contract: Microsoft Speech SDK 1.52.0's WebsocketMessageFormatter,
//! SpeechContext and ServiceRecognizerBase (MIT-licensed reference implementation).
//! https://github.com/microsoft/cognitive-services-speech-sdk-js
use std::collections::BTreeMap;

use serde_json::{Value, json};
use tungstenite::Message;

const MAX_TEXT: usize = 2 * 1024 * 1024;
const MAX_HEADERS: usize = 4096;
const MAX_SEGMENTS: usize = 10_000;
const INVALID: &str = "Invalid Microsoft Speech response.";

pub(super) struct SpeechProtocol {
    request_id: String,
    started: bool,
    finishing: bool,
    ended: bool,
    completed: bool,
    retained_bytes: usize,
    segments: BTreeMap<(u64, u64), String>,
}

impl SpeechProtocol {
    pub(super) fn new(request_id: String) -> Self {
        Self {
            request_id,
            started: false,
            finishing: false,
            ended: false,
            completed: false,
            retained_bytes: 0,
            segments: BTreeMap::new(),
        }
    }

    fn headers(&self, path: &str, content_type: Option<&str>) -> String {
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("UTC timestamps can be formatted as RFC3339");
        let mut headers = format!(
            "Path: {path}\r\nX-RequestId: {}\r\nX-Timestamp: {timestamp}\r\n",
            self.request_id
        );
        if let Some(content_type) = content_type {
            headers.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        headers
    }

    fn text(&self, path: &str, body: Value) -> Message {
        Message::Text(format!("{}\r\n{body}", self.headers(path, Some("application/json"))).into())
    }

    fn binary(&self, audio: &[u8], content_type: Option<&str>) -> Message {
        let header = self.headers("audio", content_type);
        let mut bytes = Vec::with_capacity(2 + header.len() + audio.len());
        bytes.extend_from_slice(&(header.len() as u16).to_be_bytes());
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(audio);
        Message::Binary(bytes.into())
    }

    pub(super) fn setup(&self) -> [Message; 3] {
        [
            self.text(
                "speech.config",
                json!({"context":{"system":{
                    "name":"Hex", "version":env!("CARGO_PKG_VERSION")
                }}}),
            ),
            self.text(
                "speech.context",
                json!({
                    "model":{"name":"mai-transcribe-2-streaming"},
                    "phraseDetection":{"mode":"Conversation"}
                }),
            ),
            self.binary(&pcm_header(), Some("audio/x-wav")),
        ]
    }

    pub(super) fn audio(&self, pcm: &[u8]) -> Message {
        self.binary(pcm, None)
    }

    pub(super) fn finish(&mut self) -> Message {
        self.finishing = true;
        // Speech signals EOF using an audio frame with no body, not a socket close.
        self.binary(&[], None)
    }

    pub(super) fn event(&mut self, message: &str) -> Result<Option<String>, &'static str> {
        if message.len() > MAX_TEXT || self.completed {
            return Err(INVALID);
        }
        let (headers, body) = message.split_once("\r\n\r\n").ok_or(INVALID)?;
        if headers.len() > MAX_HEADERS {
            return Err(INVALID);
        }
        let mut path = None;
        let mut request_id = None;
        for line in headers.split("\r\n") {
            let (name, value) = line.split_once(':').ok_or(INVALID)?;
            let slot = if name.trim().eq_ignore_ascii_case("path") {
                &mut path
            } else if name.trim().eq_ignore_ascii_case("x-requestid") {
                &mut request_id
            } else {
                continue;
            };
            if slot.replace(value.trim()).is_some() {
                return Err(INVALID);
            }
        }
        if !request_id.is_some_and(|id| id.eq_ignore_ascii_case(&self.request_id)) {
            return Err("Microsoft Speech returned a response for another recording.");
        }
        let path = path.ok_or(INVALID)?.to_ascii_lowercase();
        match path.as_str() {
            "turn.start" if !self.started => self.started = true,
            "turn.start" => return Err(INVALID),
            "speech.hypothesis" | "speech.startdetected" | "speech.enddetected" => {
                if !self.started || self.ended {
                    return Err(INVALID);
                }
            }
            "speech.phrase" => {
                if !self.started || self.ended {
                    return Err(INVALID);
                }
                let value: Value = serde_json::from_str(body).map_err(|_| INVALID)?;
                match value["RecognitionStatus"].as_str() {
                    Some("Success") => self.retain(&value)?,
                    Some("EndOfDictation") if self.finishing => self.ended = true,
                    Some("EndOfDictation") => {
                        return Err("Microsoft Speech ended before the recording was finished.");
                    }
                    Some("NoMatch") => {}
                    _ => return Err("Microsoft Speech rejected the transcription."),
                }
            }
            "turn.end" => {
                if !self.started || !self.finishing || !self.ended {
                    return Err("Microsoft Speech did not acknowledge the complete recording.");
                }
                self.completed = true;
                return Ok(Some(
                    self.segments
                        .values()
                        .map(|text| text.trim())
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join(" "),
                ));
            }
            "error" | "speech.error" | "session.error" => {
                return Err("Microsoft Speech rejected the transcription.");
            }
            _ => {}
        }
        Ok(None)
    }

    fn retain(&mut self, value: &Value) -> Result<(), &'static str> {
        let offset = value["Offset"].as_u64().ok_or(INVALID)?;
        let duration = value["Duration"]
            .as_u64()
            .filter(|duration| *duration > 0)
            .ok_or(INVALID)?;
        offset.checked_add(duration).ok_or(INVALID)?;
        let text = value["DisplayText"].as_str().ok_or(INVALID)?;
        let segment = (offset, duration);
        let replaced = self.segments.get(&segment).map_or(0, String::len);
        let new_size = self
            .retained_bytes
            .saturating_sub(replaced)
            .saturating_add(text.len());
        if new_size.saturating_add(self.segments.len()) >= MAX_TEXT
            || (!self.segments.contains_key(&segment) && self.segments.len() >= MAX_SEGMENTS)
        {
            return Err("Microsoft Speech response exceeded its limit.");
        }
        self.segments.insert(segment, text.to_owned());
        self.retained_bytes = new_size;
        Ok(())
    }
}

fn pcm_header() -> Vec<u8> {
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&36u32.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&16_000u32.to_le_bytes());
    header.extend_from_slice(&32_000u32.to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&0u32.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "0123456789abcdef0123456789abcdef";
    fn event(path: &str, body: Value) -> String {
        format!("Path: {path}\r\nX-RequestId: {ID}\r\n\r\n{body}")
    }
    fn segment(offset: u64, text: &str) -> String {
        event(
            "speech.phrase",
            json!({"RecognitionStatus":"Success", "Offset":offset,"Duration":10,"DisplayText":text}),
        )
    }
    fn started() -> SpeechProtocol {
        let mut state = SpeechProtocol::new(ID.into());
        state.event(&event("turn.start", json!({}))).unwrap();
        state
    }
    #[test]
    fn pauses_and_partials_never_release_text_before_exact_eof_acknowledgement() {
        let mut state = started();
        assert!(
            state
                .event(&event("speech.hypothesis", json!({"Text":"never paste"})))
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .event(&segment(0, "First sentence."))
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .event(&segment(0, "Corrected sentence."))
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .event(&segment(20, "After a pause."))
                .unwrap()
                .is_none()
        );
        state.finish();
        assert!(
            state
                .event(&event(
                    "speech.phrase",
                    json!({"RecognitionStatus":"EndOfDictation"})
                ))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state.event(&event("turn.end", json!({}))).unwrap(),
            Some("Corrected sentence. After a pause.".into())
        );
        assert!(state.event(&event("turn.end", json!({}))).is_err());
    }
    #[test]
    fn missing_acknowledgements_and_cross_recording_frames_are_rejected() {
        for finish in [false, true] {
            let mut state = started();
            state.event(&segment(0, "Only a segment")).unwrap();
            if finish {
                state.finish();
            }
            assert!(state.event(&event("turn.end", json!({}))).is_err());
        }
        let mut state = started();
        assert!(
            state
                .event(&event(
                    "speech.phrase",
                    json!({"RecognitionStatus":"EndOfDictation"})
                ))
                .is_err()
        );
        assert!(
            state
                .event(&segment(0, "wrong session").replace(ID, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
                .is_err()
        );
    }
    #[test]
    fn malformed_unbounded_and_duplicate_headers_never_become_transcripts() {
        let mut state = started();
        for raw in [
            "{}".to_owned(),
            event("speech.phrase", json!({"RecognitionStatus":"Success"})),
            format!("Path: speech.phrase\r\nPath: turn.end\r\nX-RequestId: {ID}\r\n\r\n{{}}"),
            segment(u64::MAX, "overflow"),
            segment(0, &"x".repeat(MAX_TEXT)),
        ] {
            assert!(state.event(&raw).is_err());
        }
    }
    #[test]
    fn binary_audio_has_length_prefixed_headers_and_eof_has_no_pcm() {
        let mut state = started();
        let Message::Binary(bytes) = state.audio(&[1, 2, 3, 4]) else {
            panic!()
        };
        let offset = 2 + u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
        assert_eq!(&bytes[offset..], &[1, 2, 3, 4]);
        assert!(
            std::str::from_utf8(&bytes[2..offset])
                .unwrap()
                .contains("Path: audio\r\n")
        );
        let Message::Binary(end) = state.finish() else {
            panic!()
        };
        assert_eq!(end.len(), 2 + u16::from_be_bytes([end[0], end[1]]) as usize);
        assert_eq!(pcm_header().len(), 44);
    }
}
