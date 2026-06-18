//! Parser for OpenAI **harmony** response format (gpt-oss).
//!
//! gpt-oss does not emit Qwen `<tool_call>` markup. Instead its output is a
//! sequence of channel-tagged messages delimited by special tokens which, when
//! the run is detokenized **keeping** special tokens, render as literal markers:
//!
//! ```text
//! <|channel|>analysis<|message|>{chain-of-thought}<|end|>
//! <|start|>assistant<|channel|>final<|message|>{the user-visible answer}<|return|>
//! ```
//!
//! A tool call is a `commentary` message addressed to a function, terminated by
//! `<|call|>` (which the gateway also treats as a stop token, so it is absent
//! from the captured text — the body then runs to end-of-string):
//!
//! ```text
//! <|channel|>commentary to=functions.get_weather <|constrain|>json<|message|>{"city":"Paris"}<|call|>
//! ```
//!
//! We parse the marker-bearing text (not the raw token ids) so the same routine
//! drives both incremental streaming (re-parse each token, stream the growing
//! `final` text) and the final pass (extract tool calls / the clean answer).

const CHANNEL: &str = "<|channel|>";
const MESSAGE: &str = "<|message|>";
/// Markers that terminate a message body (besides the start of the next message).
const TERMINATORS: &[&str] = &["<|end|>", "<|return|>", "<|call|>", CHANNEL, "<|start|>"];

#[derive(Debug, Default, PartialEq)]
pub struct HarmonyParsed {
    /// Concatenated `final` channel text — the user-visible answer.
    pub final_text: String,
    /// `commentary to=functions.X` calls as `(name, raw_json_args)`.
    pub tool_calls: Vec<(String, String)>,
    /// Concatenated `analysis` channel text (chain-of-thought).
    pub analysis: String,
}

/// Parse harmony output from text detokenized WITH special tokens preserved.
pub fn parse_harmony(marked: &str) -> HarmonyParsed {
    let mut out = HarmonyParsed::default();
    let mut rest = marked;

    while let Some(ci) = rest.find(CHANNEL) {
        let after_channel = &rest[ci + CHANNEL.len()..];
        // The header runs up to `<|message|>`; without one the message is
        // incomplete (still streaming the header) — nothing to route yet.
        let Some(mi) = after_channel.find(MESSAGE) else {
            break;
        };
        let header = after_channel[..mi].trim();
        let body_region = &after_channel[mi + MESSAGE.len()..];

        // Body ends at the earliest terminator, else end-of-string (a tool call's
        // `<|call|>` / a final's `<|return|>` are stop tokens, so absent here).
        let body_end = TERMINATORS
            .iter()
            .filter_map(|t| body_region.find(t))
            .min()
            .unwrap_or(body_region.len());
        let body = &body_region[..body_end];

        // A `to=functions.NAME` recipient means a tool call — check it FIRST,
        // regardless of the channel name. gpt-oss-20b is not perfectly consistent
        // with the format and sometimes attaches the recipient to the WRONG channel
        // (e.g. `analysis to=functions.exec_command<|channel|>commentary…`); routing
        // by channel name first would mis-file that as analysis and DROP the call,
        // stalling the agent. A bare `commentary` (preamble, no recipient) is ignored.
        if let Some(name) = function_recipient(header) {
            out.tool_calls.push((name, first_json_args(body)));
        } else if header.starts_with("final") {
            out.final_text.push_str(body);
        } else if header.starts_with("analysis") {
            out.analysis.push_str(body);
        }

        // Advance to the terminator so the next iteration finds the next channel.
        rest = &body_region[body_end..];
    }

    out
}

/// gpt-oss-20b sometimes appends trailing junk after the tool-call arguments object (observed: a
/// stray `\n"}` after a complete `{...}`), which makes the args invalid JSON ("Extra data") so the
/// agent (codex/opencode/claude) drops the call and stalls. Take the FIRST complete JSON value and
/// discard the rest; a non-JSON body is left untouched (downstream loose/repair paths can still try).
fn first_json_args(body: &str) -> String {
    let body = body.trim();
    let mut de = serde_json::Deserializer::from_str(body).into_iter::<serde_json::Value>();
    match de.next() {
        // Truncate at the end of the first complete value (keep original bytes — don't reorder
        // keys / reformat), dropping any trailing junk.
        Some(Ok(_)) => body[..de.byte_offset()].trim_end().to_string(),
        _ => body.to_string(),
    }
}

/// Extract `NAME` from a `commentary to=functions.NAME ...` header, if present.
fn function_recipient(header: &str) -> Option<String> {
    const KEY: &str = "to=functions.";
    let start = header.find(KEY)? + KEY.len();
    let name: String = header[start..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '<')
        .collect();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_answer_strips_analysis() {
        let s = "<|channel|>analysis<|message|>The user asks for the capital.<|end|>\
                 <|start|>assistant<|channel|>final<|message|>The capital of France is Paris.<|return|>";
        let p = parse_harmony(s);
        assert_eq!(p.final_text, "The capital of France is Paris.");
        assert!(p.tool_calls.is_empty());
        assert!(p.analysis.contains("capital"));
    }

    #[test]
    fn tool_call_with_constrain_and_no_terminator() {
        // `<|call|>` is a stop token, so the captured body runs to end-of-string.
        let s = "<|channel|>analysis<|message|>need weather<|end|>\
                 <|start|>assistant<|channel|>commentary to=functions.get_weather <|constrain|>json<|message|>{\"city\":\"Paris\"}";
        let p = parse_harmony(s);
        assert_eq!(p.tool_calls, vec![("get_weather".into(), "{\"city\":\"Paris\"}".into())]);
        assert!(p.final_text.is_empty());
    }

    #[test]
    fn incremental_final_is_prefix_stable() {
        // Mid-stream: header seen, partial body.
        let a = "<|channel|>final<|message|>The capital";
        let b = "<|channel|>final<|message|>The capital of France is Paris.";
        assert!(parse_harmony(b).final_text.starts_with(&parse_harmony(a).final_text));
    }

    #[test]
    fn recipient_on_wrong_channel_still_parses_as_tool_call() {
        // gpt-oss-20b sometimes attaches the recipient to the analysis channel and
        // doubles the channel marker; the call must still be extracted, not dropped.
        let s = "<|channel|>analysis<|message|>Let's inspect.<|end|><|start|>assistant\
                 <|channel|>analysis to=functions.exec_command<|channel|>commentary<|constrain|>json\
                 <|message|>{\"cmd\":\"ls -R\",\"workdir\":\"./\"}";
        let p = parse_harmony(s);
        assert_eq!(p.tool_calls, vec![("exec_command".into(), "{\"cmd\":\"ls -R\",\"workdir\":\"./\"}".into())]);
    }

    #[test]
    fn bare_commentary_preamble_ignored() {
        let s = "<|channel|>commentary<|message|>Let me think out loud.<|end|>";
        assert!(parse_harmony(s).tool_calls.is_empty());
    }

    #[test]
    fn tool_call_trailing_junk_after_json_dropped() {
        // gpt-oss-20b sometimes appends a stray `\n"}` after a complete args object → the raw body
        // is invalid JSON ("Extra data") and the agent drops the call. We keep the first value only.
        let s = "<|start|>assistant<|channel|>commentary to=functions.apply_unified_diff \
                 <|constrain|>json<|message|>{\"diff\":\"--- a\\n+++ b\"}\n\"}<|call|>";
        let p = parse_harmony(s);
        assert_eq!(p.tool_calls.len(), 1, "call must not be dropped");
        let (name, args) = &p.tool_calls[0];
        assert_eq!(name, "apply_unified_diff");
        assert_eq!(args, "{\"diff\":\"--- a\\n+++ b\"}", "trailing junk not dropped: {args}");
        // and it's valid JSON now
        let v: serde_json::Value = serde_json::from_str(args).expect("valid JSON");
        assert_eq!(v["diff"], "--- a\n+++ b");
    }
}
