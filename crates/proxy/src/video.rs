//! Video files in a request. An agent such as Qwen Code sends a video it
//! read as a `video_url` part holding the whole file as a `data:video/`
//! URL. The local engines Tenaz drives do not decode one: Sushi refuses
//! the request, and the agent sends it again with every turn. The proxy
//! puts a note in its place that says how to see it (ADR 0024).

use serde_json::Value;

/// What a video file is replaced with.
pub(crate) const VIDEO_NOTE: &str = "[Tenaz] A video file was here, but this engine does not take video files. To see it, extract frames as images (for example with `ffmpeg -vf fps=1`) and read those.";

/// Replaces each content part of `request`'s messages that holds a video
/// file with [`VIDEO_NOTE`]; true when any was replaced.
pub(crate) fn replace_video_files(request: &mut serde_json::Map<String, Value>) -> bool {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut replaced = false;
    let parts = messages
        .iter_mut()
        .filter_map(|m| m.get_mut("content").and_then(Value::as_array_mut))
        .flatten();
    for part in parts.filter(|p| is_video_file(p)) {
        *part = serde_json::json!({"type": "text", "text": VIDEO_NOTE});
        replaced = true;
    }
    replaced
}

/// A `video_url` part whose URL is a video file, not a list of frames.
fn is_video_file(part: &Value) -> bool {
    if part.get("type").and_then(Value::as_str) != Some("video_url") {
        return false;
    }
    part["video_url"]["url"]
        .as_str()
        .is_some_and(|url| url.starts_with("data:video/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(content: &Value) -> serde_json::Map<String, Value> {
        let value = serde_json::json!({"messages": [
            {"role": "system", "content": "plain text"},
            {"role": "user", "content": content},
        ]});
        value.as_object().unwrap().clone()
    }

    fn part(url: &str) -> Value {
        serde_json::json!({"type": "video_url", "video_url": {"url": url}})
    }

    /// The video file is replaced by the note; the parts around it stay.
    #[test]
    fn a_video_file_becomes_a_note() {
        let text = serde_json::json!({"type": "text", "text": "see"});
        let mut request = request(&serde_json::json!([
            text,
            part("data:video/mp4;base64,AAAA")
        ]));
        assert!(replace_video_files(&mut request));
        let content = &request["messages"][1]["content"];
        assert_eq!(content[0], text);
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], VIDEO_NOTE);
    }

    /// A video in an earlier tool result is replaced too, and a second pass
    /// changes nothing: every request carries the same note, so the
    /// engine's cached prefix stays valid.
    #[test]
    fn every_video_file_is_replaced_once_and_for_all() {
        let value = serde_json::json!({"messages": [
            {"role": "tool", "tool_call_id": "1", "content": [part("data:video/mp4;base64,AA")]},
            {"role": "user", "content": [part("data:video/webm;base64,BB")]},
        ]});
        let mut request = value.as_object().unwrap().clone();
        assert!(replace_video_files(&mut request));
        let once = request.clone();
        assert_eq!(once["messages"][0]["content"][0]["text"], VIDEO_NOTE);
        assert_eq!(once["messages"][1]["content"][0]["text"], VIDEO_NOTE);
        assert!(!replace_video_files(&mut request));
        assert_eq!(request, once);
    }

    /// Frames, a remote video, images and plain text are left alone.
    #[test]
    fn what_is_not_a_video_file_is_kept() {
        let frames = serde_json::json!({"type": "video", "video": ["data:image/png;base64,AA"]});
        let image = serde_json::json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}});
        let content = serde_json::json!([frames, image, part("https://example.com/a.mp4")]);
        let mut request = request(&content);
        assert!(!replace_video_files(&mut request));
        assert_eq!(request["messages"][1]["content"], content);
    }
}
