//! Item id normalization for one WebSocket connection (or one HTTP
//! `POST /responses`, which carries a single request).
//!
//! Copilot gives every streamed event of one output item a different opaque
//! id: `output_item.added`, each delta, `output_item.done` and the matching
//! `response.completed.output[i]` never agree, and even `response.id` changes
//! between `created` and `completed`. Inside one response only `output_index`
//! (and `call_id`) is stable, so the relay keys ids on its own per-request
//! stream id plus `output_index`: `copilot-<stream uuid>-<output_index>`.
//!
//! The generated ids never contain `_`. Codex drops any item id without an
//! `A_B` shape before replaying history upstream, so relay ids never reach
//! Copilot; [`Normalizer::upstream`] strips the few that might slip through
//! anyway. `response.id`, `call_id` and everything else are left alone:
//! `previous_response_id` must stay Copilot's real completed id.
//!
//! The namespace rotates on every `response.create`, so ids are only right
//! while one request at a time is in flight on a connection. Codex keeps it
//! that way: it holds the socket's stream lock from sending
//! `response.create` until the terminal event
//! (`codex-api/src/endpoint/responses_websocket.rs:280-345`) and drops the
//! connection if it gives up earlier. `response.interrupt` is sent inside
//! that window and asks for the same response to end, so it does not rotate.

use serde_json::Value;

use crate::{CODEX_AUTO_REVIEW, ITEM_ID_PREFIX};

/// Per-connection state: one id namespace per `response.create` request,
/// which assumes the requests on one connection never overlap (see the
/// module docs).
#[derive(Debug, Default, Clone)]
pub struct Normalizer {
    /// Stream id of the request whose events are flowing now. `None` until the
    /// first `response.create` (or the first id is needed).
    stream: Option<String>,
}

/// Whether `id` was minted by the relay (`copilot-...`).
pub fn is_relay_id(id: &str) -> bool {
    id.strip_prefix(ITEM_ID_PREFIX)
        .is_some_and(|rest| rest.starts_with('-'))
}

impl Normalizer {
    /// A normalizer that has seen no request yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A normalizer with a fixed stream id, for deterministic tests.
    pub fn with_stream(stream: impl Into<String>) -> Self {
        Self {
            stream: Some(stream.into()),
        }
    }

    /// The current stream id, if one was assigned.
    pub fn stream(&self) -> Option<&str> {
        self.stream.as_deref()
    }

    /// Start a new id namespace: called for every `response.create`, because
    /// `output_index` restarts at 0 in each response.
    pub fn new_request(&mut self) {
        self.stream = Some(fresh_stream());
    }

    /// The stable id of output item `output_index` in the current request.
    pub fn make_id(&mut self, output_index: u64) -> String {
        let stream = self.stream.get_or_insert_with(fresh_stream);
        format!("{ITEM_ID_PREFIX}-{stream}-{output_index}")
    }

    /// Rewrites the item ids of one server event in place. Returns whether
    /// anything was touched (callers forward the original text otherwise).
    ///
    /// * events with an integer `output_index`: `item.id` and/or `item_id`;
    /// * events whose `response.output` is an array (`response.completed`,
    ///   `.incomplete`, `.failed`): entry `i`, if it has an `id`, gets the id
    ///   of `output_index` `i`. Codex never reads that array (its
    ///   `ResponseCompleted` takes only `id` and `usage`), so position is
    ///   good enough; it is rewritten only so no Copilot item id leaks out.
    pub fn downstream(&mut self, event: &mut Value) -> bool {
        let Some(obj) = event.as_object_mut() else {
            return false;
        };
        let mut changed = false;

        if let Some(index) = obj.get("output_index").and_then(Value::as_u64) {
            if let Some(item) = obj.get_mut("item").and_then(Value::as_object_mut) {
                item.insert("id".to_string(), Value::String(self.make_id(index)));
                changed = true;
            }
            if let Some(item_id) = obj.get_mut("item_id").filter(|v| v.is_string()) {
                *item_id = Value::String(self.make_id(index));
                changed = true;
            }
        }

        if let Some(output) = obj
            .get_mut("response")
            .and_then(|r| r.get_mut("output"))
            .and_then(Value::as_array_mut)
        {
            for (index, item) in (0u64..).zip(output.iter_mut()) {
                // An item Copilot sent without an id stays without one.
                if let Some(id) = item.as_object_mut().and_then(|i| i.get_mut("id")) {
                    *id = Value::String(self.make_id(index));
                    changed = true;
                }
            }
        }
        changed
    }

    /// Prepares one client frame for Copilot. Only `response.create` frames
    /// are touched: they open a new id namespace, `codex-auto-review` becomes
    /// `review_model` (CAPI does not serve it), and relay ids in `input[]` are
    /// removed (the key itself, not set to null, which is exactly what Codex
    /// sends for ids it strips). Returns whether the frame must be
    /// re-serialized; otherwise the original text is forwarded byte for byte.
    pub fn upstream(&mut self, request: &mut Value, review_model: Option<&str>) -> bool {
        if request.get("type").and_then(Value::as_str) != Some("response.create") {
            return false;
        }
        self.upstream_http(request, review_model)
    }

    /// Prepares the JSON body of an HTTP `POST /responses` for Copilot. That
    /// body is a `response.create` frame without its `type`, so it gets the
    /// same treatment as [`Normalizer::upstream`], unconditionally: a new id
    /// namespace, the reviewer rewrite and relay id removal. Returns whether
    /// the body must be re-serialized.
    pub fn upstream_http(&mut self, request: &mut Value, review_model: Option<&str>) -> bool {
        self.new_request();
        let Some(obj) = request.as_object_mut() else {
            return false;
        };
        let mut changed = false;

        if let Some(review) = review_model {
            if let Some(model) = obj
                .get_mut("model")
                .filter(|m| m.as_str() == Some(CODEX_AUTO_REVIEW))
            {
                *model = Value::String(review.to_string());
                changed = true;
            }
        }

        if let Some(input) = obj.get_mut("input").and_then(Value::as_array_mut) {
            for item in input.iter_mut().filter_map(Value::as_object_mut) {
                if item
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(is_relay_id)
                {
                    // shift_remove keeps the other keys in their original order.
                    item.shift_remove("id");
                    changed = true;
                }
            }
        }
        changed
    }
}

fn fresh_stream() -> String {
    uuid::Uuid::now_v7().hyphenated().to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Ids below are shortened copies of real Copilot ids from the WS / SSE
    // captures: base64 with `+` and `/`, a fresh one on every event.
    const ADDED: &str = "poZfyGL9vcy1pu7Tg8VbTI3OVFNHub+Q2a/1";
    const PART: &str = "oQu4nYQ4JBeDRyhsAQJfrOc9ofsd5r/kW+2";
    const DELTA1: &str = "FVrVSHQ1Kx+sCnvHKAm8TQlKTd9jFu7Y";
    const DELTA2: &str = "IM6EH44jqyyyt0UUQx4jLG9dA4fj3l/a";
    const DONE: &str = "Nj7fc2AD8R7Z0BF4y2GFdO6dP0vPue+=";
    const COMPLETED: &str = "j0jgZocJufSFMLBk1aX9sXGrybxGzn==";
    const RESP_CREATED: &str = "BA5Y1YfjB6JJcVb9djHNXc5dWlEyo9ycFKI9C+zYG6G/";
    const RESP_COMPLETED: &str = "iDrglUtKTodD+5ilsH6iR+EgFvyRx8nZ/0xtZIts";

    fn id_of(event: &Value) -> &str {
        event
            .get("item")
            .and_then(|i| i.get("id"))
            .or_else(|| event.get("item_id"))
            .and_then(Value::as_str)
            .expect("event carries an item id")
    }

    /// One message at output_index 0, as in ws-enterprise-base.json.
    fn message_stream() -> Vec<Value> {
        vec![
            json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,
                   "item":{"content":[],"id":ADDED,"phase":"final_answer","role":"assistant",
                           "status":"in_progress","type":"message"}}),
            json!({"type":"response.content_part.added","sequence_number":3,"content_index":0,
                   "item_id":PART,"output_index":0,
                   "part":{"annotations":[],"logprobs":[],"text":"","type":"output_text"}}),
            json!({"type":"response.output_text.delta","sequence_number":4,"content_index":0,
                   "delta":"ID","item_id":DELTA1,"logprobs":[],"obfuscation":"V2z1JfyBPnSyiR",
                   "output_index":0}),
            json!({"type":"response.output_text.delta","sequence_number":5,"content_index":0,
                   "delta":"_OK","item_id":DELTA2,"logprobs":[],"obfuscation":"O2KEj4LBuxHV",
                   "output_index":0}),
            json!({"type":"response.output_item.done","sequence_number":10,"output_index":0,
                   "item":{"content":[{"annotations":[],"logprobs":[],"text":"ID_OK",
                                       "type":"output_text"}],
                           "id":DONE,"phase":"final_answer","role":"assistant",
                           "status":"completed","type":"message"}}),
        ]
    }

    #[test]
    fn every_event_of_one_message_gets_the_same_id() {
        let mut n = Normalizer::with_stream("s1");
        let mut events = message_stream();
        for e in &mut events {
            assert!(n.downstream(e));
        }
        for e in &events {
            assert_eq!(id_of(e), "copilot-s1-0", "{e}");
        }
        // Everything except the id is untouched, key order included.
        assert_eq!(events[2]["delta"], "ID");
        assert_eq!(events[2]["obfuscation"], "V2z1JfyBPnSyiR");
        assert_eq!(events[2]["sequence_number"], 4);
        let keys: Vec<_> = events[0]["item"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["content", "id", "phase", "role", "status", "type"]);
        assert_eq!(events[4]["item"]["content"][0]["text"], "ID_OK");
    }

    #[test]
    fn reasoning_and_message_items_are_keyed_by_output_index() {
        // Shape of sse-enterprise-mini.json: a reasoning item at 0, then the
        // message at 1, each with its own rotating ids.
        let mut n = Normalizer::with_stream("s2");
        let mut added = json!({"type":"response.output_item.added","output_index":0,
            "item":{"content":[],"encrypted_content":"Zol6N/5yR+wbUUfmu0g6QNRfyKiQf7",
                    "id":"MBiNiY39rKG6OVOTEGKrqa+2WVymdR","summary":[],"type":"reasoning"}});
        let mut done = json!({"type":"response.output_item.done","output_index":0,
            "item":{"content":[],"encrypted_content":"/cjYLr9og2ea3fJcC0DZv5Y7UQNEgX",
                    "id":"gp6U8+Jf42nyXOk0Jl0wyYLN7F8SKS","summary":[],"type":"reasoning"}});
        let mut summary = json!({"type":"response.reasoning_summary_text.delta",
            "item_id":"8MfhLlSf1czQ+zZ8fG9c1BpToAQXN2","output_index":0,
            "summary_index":0,"delta":"Thinking"});
        let mut message = json!({"type":"response.output_item.added","output_index":1,
            "item":{"content":[],"id":"CIdumIM04aJtIkOnC/tYAXAEKQohPy","role":"assistant",
                    "status":"in_progress","type":"message"}});
        for e in [&mut added, &mut done, &mut summary, &mut message] {
            n.downstream(e);
        }
        assert_eq!(id_of(&added), "copilot-s2-0");
        assert_eq!(id_of(&done), "copilot-s2-0");
        assert_eq!(id_of(&summary), "copilot-s2-0");
        assert_eq!(id_of(&message), "copilot-s2-1");
        // The opaque reasoning blob is Copilot's and must survive verbatim.
        assert_eq!(
            done["item"]["encrypted_content"],
            "/cjYLr9og2ea3fJcC0DZv5Y7UQNEgX"
        );
        assert_eq!(summary["summary_index"], 0);
    }

    #[test]
    fn custom_tool_input_deltas_follow_the_item_not_the_ctc_id() {
        // ws-enterprise-custom.json: the deltas share a native `ctc_` id that
        // matches neither added nor done; call_id is stable and stays as is.
        let mut n = Normalizer::with_stream("s3");
        let mut added = json!({"type":"response.output_item.added","output_index":0,
            "item":{"call_id":"call_ov3935SMdyJIf0L9JoQtEfzY","id":"br88fWLVSWCqrh5lXP3PthIhz+CHwM",
                    "input":"","name":"probe_echo","status":"in_progress","type":"custom_tool_call"}});
        let mut delta = json!({"type":"response.custom_tool_call_input.delta","delta":"ID",
            "item_id":"ctc_02665d66bce2fa59016ac5ff4a80ac87d2880af5e58c6b3e5e",
            "obfuscation":"ueIabsSFKsLFxk","output_index":0,"sequence_number":3});
        let mut input_done = json!({"type":"response.custom_tool_call_input.done",
            "input":"ID_PROBE_OK\n",
            "item_id":"ctc_02665d66bce2fa59016ac5ff4a80ac87d2880af5e58c6b3e5e",
            "output_index":0,"sequence_number":8});
        let mut done = json!({"type":"response.output_item.done","output_index":0,
            "item":{"call_id":"call_ov3935SMdyJIf0L9JoQtEfzY","id":"ZqDaislY0bT5OzpCELqngqOfoMiG1K",
                    "input":"ID_PROBE_OK\n","name":"probe_echo","status":"completed",
                    "type":"custom_tool_call"}});
        for e in [&mut added, &mut delta, &mut input_done, &mut done] {
            n.downstream(e);
            assert_eq!(id_of(e), "copilot-s3-0");
        }
        assert_eq!(added["item"]["call_id"], "call_ov3935SMdyJIf0L9JoQtEfzY");
        assert_eq!(done["item"]["call_id"], "call_ov3935SMdyJIf0L9JoQtEfzY");
        assert_eq!(delta["delta"], "ID");
    }

    #[test]
    fn completed_output_ids_match_the_streamed_ones_and_response_id_is_kept() {
        let mut n = Normalizer::with_stream("s4");
        let mut created = json!({"type":"response.created","sequence_number":0,
            "response":{"id":RESP_CREATED,"status":"in_progress","output":[]}});
        let mut completed = json!({"type":"response.completed","sequence_number":11,
            "response":{"id":RESP_COMPLETED,"status":"completed","output":[
                {"content":[],"encrypted_content":"oMJQDd+aATo/Ik++mzrAjJbmThX0nX",
                 "id":"Ri6OY3b3rZE48EgaThqzU7FeXiO1Hr","summary":[],"type":"reasoning"},
                {"content":[{"annotations":[],"logprobs":[],"text":"ID_OK","type":"output_text"}],
                 "id":COMPLETED,"phase":"final_answer","role":"assistant",
                 "status":"completed","type":"message"}]}});
        assert!(!n.downstream(&mut created));
        assert!(n.downstream(&mut completed));
        assert_eq!(created["response"]["id"], RESP_CREATED);
        assert_eq!(completed["response"]["id"], RESP_COMPLETED);
        let out = &completed["response"]["output"];
        assert_eq!(out[0]["id"], "copilot-s4-0");
        assert_eq!(out[1]["id"], "copilot-s4-1");
        assert_eq!(
            out[0]["encrypted_content"],
            "oMJQDd+aATo/Ik++mzrAjJbmThX0nX"
        );
        assert_eq!(out[1]["content"][0]["text"], "ID_OK");

        // incomplete / failed carry their partial output the same way.
        for kind in ["response.incomplete", "response.failed"] {
            let mut e = json!({"type":kind,"response":{"id":"r","output":[{"id":"x"}]}});
            n.downstream(&mut e);
            assert_eq!(e["response"]["output"][0]["id"], "copilot-s4-0");
            assert_eq!(e["response"]["id"], "r");
        }
    }

    #[test]
    fn output_items_without_an_id_are_left_alone() {
        let mut n = Normalizer::with_stream("n");
        let mut completed = json!({"type":"response.completed","response":{"output":[
            {"type":"reasoning","summary":[]},{"id":"y","type":"message"},"not an item"]}});
        assert!(n.downstream(&mut completed));
        let output = &completed["response"]["output"];
        assert_eq!(output[0], json!({"type":"reasoning","summary":[]}));
        assert_eq!(output[1]["id"], "copilot-n-1");
        assert_eq!(output[2], "not an item");

        let original = json!({"type":"response.completed","response":{"output":[{"type":"x"}]}});
        let mut e = original.clone();
        assert!(!n.downstream(&mut e), "nothing to rewrite");
        assert_eq!(e, original);
    }

    #[test]
    fn events_without_an_output_index_pass_through() {
        let mut n = Normalizer::with_stream("s5");
        let original = json!({"type":"response.in_progress","sequence_number":1,
            "response":{"id":"u2S/3PDXZruFdRoqIbPw3eB9lTiLrB","status":"in_progress"}});
        let mut e = original.clone();
        assert!(!n.downstream(&mut e));
        assert_eq!(e, original);

        let original = json!({"type":"error","error":{"code":"rate_limited","item_id":"keep"}});
        let mut e = original.clone();
        assert!(!n.downstream(&mut e));
        assert_eq!(e, original);

        // A non-integer output_index is not something to key on.
        let original = json!({"type":"x","output_index":"0","item_id":"keep"});
        let mut e = original.clone();
        assert!(!n.downstream(&mut e));
        assert_eq!(e, original);

        let mut e = json!(["not", "an", "event"]);
        assert!(!n.downstream(&mut e));
    }

    #[test]
    fn generated_ids_never_look_prefixed_to_codex() {
        // Codex keeps only ids shaped `A_B`; ours must be dropped by it.
        let mut n = Normalizer::new();
        n.new_request();
        let id = n.make_id(3);
        assert!(id.starts_with("copilot-"));
        assert!(id.ends_with("-3"));
        assert!(!id.contains('_'), "{id}");
        assert!(is_relay_id(&id));
        assert!(!is_relay_id("copilotx-1"));
        assert!(!is_relay_id("msg_abc"));
    }

    #[test]
    fn ids_are_minted_lazily_when_no_request_was_seen() {
        let mut n = Normalizer::new();
        assert!(n.stream().is_none());
        let mut e = json!({"output_index":0,"item_id":"a"});
        n.downstream(&mut e);
        let stream = n.stream().expect("stream minted").to_string();
        assert_eq!(e["item_id"], format!("copilot-{stream}-0"));
        // And it stays put until the next request.
        assert_eq!(n.make_id(0), format!("copilot-{stream}-0"));
    }

    #[test]
    fn each_response_create_opens_a_new_namespace() {
        let mut n = Normalizer::new();
        let mut req = json!({"type":"response.create","model":"gpt-6-astra","input":[]});
        n.upstream(&mut req, None);
        let first = n.make_id(0);
        n.upstream(&mut req, None);
        let second = n.make_id(0);
        assert_ne!(first, second);

        // Anything else on the socket leaves the namespace alone.
        let mut other = json!({"type":"session.update"});
        assert!(!n.upstream(&mut other, None));
        assert_eq!(n.make_id(0), second);
    }

    #[test]
    fn upstream_strips_relay_ids_and_rewrites_the_reviewer() {
        let mut n = Normalizer::new();
        let mut req = json!({"type":"response.create","model":CODEX_AUTO_REVIEW,
        "previous_response_id":"iDrglUtKTodD+5ilsH6iR+EgFvyRx8",
        "input":[
            {"type":"message","id":"copilot-0196-aaaa-0","role":"assistant","content":[]},
            {"type":"message","id":"msg_keep","role":"user","content":[]},
            {"type":"function_call_output","call_id":"call_1","output":"ok"},
            "not an object"
        ]});
        assert!(n.upstream(&mut req, Some("gpt-6-luna")));
        assert_eq!(req["model"], "gpt-6-luna");
        let first = req["input"][0].as_object().unwrap();
        assert!(!first.contains_key("id"), "relay id removed, not nulled");
        let keys: Vec<_> = first.keys().collect();
        assert_eq!(keys, ["type", "role", "content"]);
        assert_eq!(req["input"][1]["id"], "msg_keep");
        assert_eq!(req["input"][2]["call_id"], "call_1");
        // previous_response_id is Copilot's real id and must not change.
        assert_eq!(
            req["previous_response_id"],
            "iDrglUtKTodD+5ilsH6iR+EgFvyRx8"
        );
    }

    #[test]
    fn http_bodies_are_rewritten_like_response_create_frames() {
        // The HTTP body has no `type`; `upstream` would leave it alone.
        let original = json!({"model":CODEX_AUTO_REVIEW,"stream":true,"input":[
            {"type":"message","id":"copilot-0196-aaaa-1","role":"assistant","content":[]},
            {"type":"message","id":"msg_keep","role":"user","content":[]}]});
        let mut n = Normalizer::new();
        let mut frame = original.clone();
        assert!(!n.upstream(&mut frame, Some("gpt-6-luna")));
        assert!(n.stream().is_none(), "no namespace for a non-create frame");

        let mut body = original.clone();
        assert!(n.upstream_http(&mut body, Some("gpt-6-luna")));
        assert_eq!(body["model"], "gpt-6-luna");
        assert!(body["input"][0].get("id").is_none());
        assert_eq!(body["input"][1]["id"], "msg_keep");
        let first = n.stream().expect("namespace opened").to_string();

        // Every body opens its own namespace, even one with nothing to change.
        let mut plain = json!({"model":"gpt-6-astra","input":[]});
        assert!(!n.upstream_http(&mut plain, Some("gpt-6-luna")));
        assert_ne!(n.stream(), Some(first.as_str()));
        let mut not_an_object = json!([1]);
        assert!(!n.upstream_http(&mut not_an_object, None));
    }

    #[test]
    fn upstream_leaves_ordinary_requests_alone() {
        let mut n = Normalizer::new();
        let original = json!({"type":"response.create","model":"gpt-6-astra",
            "input":[{"type":"message","id":"msg_1","role":"user","content":[]}]});
        let mut req = original.clone();
        assert!(!n.upstream(&mut req, Some("gpt-6-luna")));
        assert_eq!(req, original);

        // Without a configured reviewer the slug goes through verbatim.
        let mut req = json!({"type":"response.create","model":CODEX_AUTO_REVIEW,"input":[]});
        assert!(!n.upstream(&mut req, None));
        assert_eq!(req["model"], CODEX_AUTO_REVIEW);
    }
}
