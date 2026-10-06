use anyhow::{Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const DEFAULT_MODEL: &str = "gpt-6-sol";

const INSTRUCTIONS: &str = "You are an image generation assistant running inside the Codex backend. Always satisfy the request by invoking the image_generation tool exactly once. Do not respond with text only.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Queued,
    Generating,
    Finishing,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Generating => "generating",
            Phase::Finishing => "finishing",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub prompt: String,
    pub quality: String,
    pub size: Option<String>,
    pub model: String,
    pub image_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Generated {
    pub bytes: Vec<u8>,
    pub format: String,
    pub revised_prompt: Option<String>,
}

pub fn originator() -> String {
    std::env::var("FUCKINGGEN_ORIGINATOR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "fuckinggen".to_string())
}

pub fn build_request(spec: &Spec, refs: &[String]) -> Value {
    let mut content = vec![json!({"type": "input_text", "text": spec.prompt})];
    for reference in refs {
        content.push(json!({"type": "input_image", "image_url": reference}));
    }
    let mut tool = json!({
        "type": "image_generation",
        "output_format": "png",
        "quality": spec.quality,
    });
    if let Some(size) = &spec.size {
        tool["size"] = json!(size);
    }
    if let Some(model) = &spec.image_model {
        tool["model"] = json!(model);
    }
    json!({
        "model": spec.model,
        "instructions": INSTRUCTIONS,
        "input": [{"role": "user", "content": content}],
        "tools": [tool],
        "tool_choice": {"type": "image_generation"},
        "stream": true,
        "store": false,
    })
}

/// Send one generation request and stream it to completion.
pub fn run(
    agent: &ureq::Agent,
    token: &str,
    account_id: Option<&str>,
    body: &Value,
    timeout: Duration,
    on_phase: &mut dyn FnMut(Phase),
) -> Result<Generated> {
    static NEVER_CANCEL: AtomicBool = AtomicBool::new(false);
    run_cancellable(
        agent,
        token,
        account_id,
        body,
        timeout,
        &NEVER_CANCEL,
        on_phase,
    )
}

/// Like [`run`], but polls `cancel` between stream events so interactive
/// callers can abort a generation in flight.
#[allow(clippy::too_many_arguments)]
pub fn run_cancellable(
    agent: &ureq::Agent,
    token: &str,
    account_id: Option<&str>,
    body: &Value,
    timeout: Duration,
    cancel: &AtomicBool,
    on_phase: &mut dyn FnMut(Phase),
) -> Result<Generated> {
    let mut request = agent
        .post(RESPONSES_ENDPOINT)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream")
        .set("originator", &originator())
        .set("Authorization", &format!("Bearer {token}"));
    if let Some(account) = account_id {
        request = request.set("ChatGPT-Account-Id", account);
    }
    let response = request
        .send_json(body.clone())
        .map_err(|err| anyhow::anyhow!(crate::http::describe_error(err)))?;
    let deadline = Instant::now() + timeout;
    parse_stream(
        BufReader::new(response.into_reader()),
        deadline,
        cancel,
        on_phase,
    )
}

enum Outcome {
    None,
    Image(Generated),
    Failed(String),
}

/// Read the SSE stream line by line until the image arrives, the backend
/// reports a failure, the deadline passes, or the caller cancels.
pub fn parse_stream<R: BufRead>(
    mut reader: R,
    deadline: Instant,
    cancel: &AtomicBool,
    on_phase: &mut dyn FnMut(Phase),
) -> Result<Generated> {
    on_phase(Phase::Queued);
    let mut data_lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut phase = Phase::Queued;
    let mut last_text: Option<String> = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled by user");
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for the image (raise --timeout if the backend is slow)");
        }
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            if data_lines.is_empty() {
                continue;
            }
            let data = data_lines.join("\n");
            data_lines.clear();
            if data == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            match handle_event(&value, &mut phase, on_phase, &mut last_text) {
                Outcome::None => {}
                Outcome::Image(image) => return Ok(image),
                Outcome::Failed(message) => bail!("{message}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    match last_text {
        Some(text) => bail!(
            "the backend finished without an image; it said: {}",
            snippet(&text, 300)
        ),
        None => bail!("the backend ended the stream without an image"),
    }
}

fn handle_event(
    value: &Value,
    phase: &mut Phase,
    on_phase: &mut dyn FnMut(Phase),
    last_text: &mut Option<String>,
) -> Outcome {
    let kind = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match kind {
        "response.image_generation_call.in_progress"
        | "response.image_generation_call.generating"
        | "response.image_generation_call.partial_image" => {
            set_phase(phase, on_phase, Phase::Generating);
            Outcome::None
        }
        "response.image_generation_call.completed" => {
            set_phase(phase, on_phase, Phase::Finishing);
            Outcome::None
        }
        "response.output_text.done" => {
            if let Some(text) = value.get("text").and_then(|v| v.as_str())
                && !text.trim().is_empty()
            {
                *last_text = Some(text.to_string());
            }
            Outcome::None
        }
        "response.output_item.done" => {
            let Some(item) = value.get("item") else {
                return Outcome::None;
            };
            if item.get("type").and_then(|v| v.as_str()) != Some("image_generation_call") {
                return Outcome::None;
            }
            set_phase(phase, on_phase, Phase::Finishing);
            let Some(encoded) = item
                .get("result")
                .and_then(|v| v.as_str())
                .filter(|data| !data.is_empty())
            else {
                return Outcome::Failed(
                    "the backend produced an image call without image data".to_string(),
                );
            };
            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                Ok(bytes) => Outcome::Image(Generated {
                    bytes,
                    format: item
                        .get("output_format")
                        .and_then(|v| v.as_str())
                        .unwrap_or("png")
                        .to_string(),
                    revised_prompt: item
                        .get("revised_prompt")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                }),
                Err(err) => {
                    Outcome::Failed(format!("backend returned undecodable image data: {err}"))
                }
            }
        }
        "response.failed" | "error" => Outcome::Failed(extract_error(value)),
        _ => Outcome::None,
    }
}

fn set_phase(phase: &mut Phase, on_phase: &mut dyn FnMut(Phase), next: Phase) {
    if *phase != next {
        *phase = next;
        on_phase(next);
    }
}

fn extract_error(value: &Value) -> String {
    value
        .pointer("/response/error/message")
        .and_then(|v| v.as_str())
        .or_else(|| value.pointer("/error/message").and_then(|v| v.as_str()))
        .or_else(|| value.get("message").and_then(|v| v.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("backend error: {}", snippet(&value.to_string(), 300)))
}

pub fn snippet(input: &str, max: usize) -> String {
    crate::auth::truncate(input, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn stream(data: &str) -> Result<Generated, anyhow::Error> {
        parse_stream(
            BufReader::new(Cursor::new(data.as_bytes().to_vec())),
            Instant::now() + Duration::from_secs(5),
            &AtomicBool::new(false),
            &mut |_| {},
        )
    }

    #[test]
    fn request_shape_includes_refs_and_tool_options() {
        let spec = Spec {
            prompt: "a cat".into(),
            quality: "high".into(),
            size: Some("1024x1024".into()),
            model: "gpt-6-sol".into(),
            image_model: Some("gpt-image-2.5".into()),
        };
        let refs = vec!["data:image/png;base64,AAAA".to_string()];
        let body = build_request(&spec, &refs);
        assert_eq!(body["model"], "gpt-6-sol");
        assert_eq!(body["tools"][0]["type"], "image_generation");
        assert_eq!(body["tools"][0]["quality"], "high");
        assert_eq!(body["tools"][0]["size"], "1024x1024");
        assert_eq!(body["tools"][0]["model"], "gpt-image-2.5");
        assert_eq!(body["tool_choice"]["type"], "image_generation");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(body["store"], false);
    }

    #[test]
    fn omits_size_and_image_model_when_unset() {
        let spec = Spec {
            prompt: "x".into(),
            quality: "auto".into(),
            size: None,
            model: "gpt-6-sol".into(),
            image_model: None,
        };
        let body = build_request(&spec, &[]);
        assert!(body["tools"][0].get("size").is_none());
        assert!(body["tools"][0].get("model").is_none());
    }

    #[test]
    fn parses_stream_and_collects_phases() {
        let payload = base64::engine::general_purpose::STANDARD.encode(b"PNGDATA");
        let stream_text = format!(
            "data: {{\"type\":\"response.created\"}}\n\n\
             : keepalive\n\n\
             data: {{\"type\":\"response.image_generation_call.in_progress\"}}\n\n\
             data: {{\"type\":\"response.image_generation_call.generating\"}}\n\n\
             data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"message\"}}}}\n\n\
             data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"image_generation_call\",\"result\":\"{payload}\",\"output_format\":\"png\",\"revised_prompt\":\"rev\"}}}}\n\n\
             data: [DONE]\n\n"
        );
        let mut phases = Vec::new();
        let image = parse_stream(
            BufReader::new(Cursor::new(stream_text.into_bytes())),
            Instant::now() + Duration::from_secs(5),
            &AtomicBool::new(false),
            &mut |phase| phases.push(phase),
        )
        .unwrap();
        assert_eq!(image.bytes, b"PNGDATA");
        assert_eq!(image.format, "png");
        assert_eq!(image.revised_prompt.as_deref(), Some("rev"));
        assert_eq!(
            phases,
            vec![Phase::Queued, Phase::Generating, Phase::Finishing]
        );
    }

    #[test]
    fn surfaces_backend_failures() {
        let err = stream("data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"));
    }

    #[test]
    fn reports_text_only_streams() {
        let err = stream(
            "data: {\"type\":\"response.output_text.done\",\"text\":\"cannot help with that\"}\n\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("without an image"));
        assert!(err.contains("cannot help"));
    }

    #[test]
    fn honours_cancellation() {
        let cancel = AtomicBool::new(true);
        let err = parse_stream(
            BufReader::new(Cursor::new(b"data: {\"type\":\"x\"}\n\n".to_vec())),
            Instant::now() + Duration::from_secs(5),
            &cancel,
            &mut |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"));
    }
}
