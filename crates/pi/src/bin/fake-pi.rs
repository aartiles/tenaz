//! A scripted stand-in for `pi --mode rpc`, the protocol half of the task
//! loop's `fake-pi.mjs`: it speaks Pi's JSON lines on stdin and stdout and
//! plays a short script chosen by the prompt text, so the adapter can be
//! tested without a model. `--version` prints a version like Pi does. When
//! `FAKE_PI_ARGS` names a file, the command line is written there.
//!
//! Prompts: `tool` (one tool turn with streaming updates, then a reply),
//! `loop` (tool turns until aborted), `steer` (tool turns until steered,
//! then a reply quoting the steer), `error` (an engine error with a capacity
//! message), `reject` (the prompt is refused), `handled` (no run starts),
//! `stderr` (writes a diagnostic line), `linger` (ignores stdin EOF for a
//! while, so a close must kill it), anything else (a reply echoing it).

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

struct Shared {
    aborted: AtomicBool,
    linger: AtomicBool,
    steers: Mutex<Vec<String>>,
}

fn emit(value: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("fake-pi 0.0.1");
        return;
    }
    if let Some(path) = std::env::var_os("FAKE_PI_ARGS") {
        let _ = std::fs::write(path, args[1..].join("\n"));
    }
    let shared = Arc::new(Shared {
        aborted: AtomicBool::new(false),
        linger: AtomicBool::new(false),
        steers: Mutex::new(Vec::new()),
    });
    let (tx, rx) = mpsc::channel::<String>();
    let reader = shared.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(cmd) = serde_json::from_str::<Value>(&line) else {
                emit(
                    &json!({"type": "response", "command": "parse", "success": false, "error": "Failed to parse command"}),
                );
                continue;
            };
            let kind = cmd["type"].as_str().unwrap_or_default().to_owned();
            match kind.as_str() {
                "prompt" => {
                    let message = cmd["message"].as_str().unwrap_or_default().to_owned();
                    let mut response = json!({"type": "response", "command": "prompt", "success": true, "data": {"disposition": "started"}});
                    if let Some(id) = cmd.get("id") {
                        response["id"] = id.clone();
                    }
                    match message.as_str() {
                        "reject" => {
                            response["success"] = json!(false);
                            response["error"] = json!("Model not found: fake/fake");
                            response.as_object_mut().unwrap().remove("data");
                            emit(&response);
                        }
                        "handled" => {
                            response["data"]["disposition"] = json!("handled");
                            emit(&response);
                        }
                        _ => {
                            emit(&response);
                            emit(&json!({"type": "agent_start"}));
                            let _ = tx.send(message);
                        }
                    }
                }
                "abort" => {
                    reader.aborted.store(true, Ordering::SeqCst);
                    emit(&json!({"type": "response", "command": "abort", "success": true}));
                }
                "steer" => {
                    reader
                        .steers
                        .lock()
                        .unwrap()
                        .push(cmd["message"].as_str().unwrap_or_default().to_owned());
                    emit(
                        &json!({"type": "response", "command": "steer", "success": true, "data": {"disposition": "queued"}}),
                    );
                }
                "clear_queue" => {
                    emit(
                        &json!({"type": "response", "command": "clear_queue", "success": true, "data": {"steering": [], "followUp": []}}),
                    );
                }
                other => {
                    emit(&json!({"type": "response", "command": other, "success": true}));
                }
            }
        }
        if reader.linger.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(30));
        }
        // Dropping `tx` ends the player loop: an orderly shutdown.
    });
    let mut player = Player {
        shared,
        call: 0,
        prompts: 0,
    };
    while let Ok(prompt) = rx.recv() {
        player.play(&prompt);
    }
}

struct Player {
    shared: Arc<Shared>,
    call: u32,
    prompts: u32,
}

impl Player {
    fn aborted(&self) -> bool {
        self.shared.aborted.load(Ordering::SeqCst)
    }

    fn take_steer(&self) -> Option<String> {
        let mut steers = self.shared.steers.lock().unwrap();
        if steers.is_empty() {
            None
        } else {
            Some(steers.remove(0))
        }
    }

    fn turn(&mut self, tool: &str, args: &Value, result: &str) {
        self.call += 1;
        let id = format!("call_{}", self.call);
        let input = 1000 + u64::from(self.call) * 10;
        emit(&json!({"type": "turn_start"}));
        emit(
            &json!({"type": "message_start", "message": {"role": "assistant", "content": [], "stopReason": "pending"}}),
        );
        emit(
            &json!({"type": "message_update", "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "pl"}}),
        );
        emit(
            &json!({"type": "message_update", "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "an"}}),
        );
        emit(
            &json!({"type": "tool_execution_start", "toolCallId": id, "toolName": tool, "args": args}),
        );
        emit(
            &json!({"type": "tool_execution_end", "toolCallId": id, "toolName": tool, "result": {"content": [{"type": "text", "text": result}]}, "isError": false, "durationMs": 1}),
        );
        let message = json!({
            "role": "assistant",
            "content": [{"type": "thinking", "thinking": "plan"}, {"type": "toolCall", "id": id, "name": tool, "arguments": args}],
            "stopReason": "toolUse",
            "usage": {"input": input, "output": 20, "cacheRead": 0, "cacheWrite": 0, "totalTokens": input + 20}
        });
        emit(&json!({"type": "message_end", "message": message}));
        emit(&json!({"type": "turn_end", "message": message, "toolResults": []}));
        std::thread::sleep(Duration::from_millis(2));
    }

    fn finish(&self, text: &str) {
        let aborted = self.aborted();
        let message = json!({
            "role": "assistant",
            "content": [{"type": "thinking", "thinking": "wrap up"}, {"type": "text", "text": text}],
            "stopReason": if aborted { "aborted" } else { "stop" },
            "usage": {"input": 1200, "output": 10, "cacheRead": if self.prompts > 1 { 900 } else { 0 }, "cacheWrite": 0}
        });
        emit(&json!({"type": "turn_start"}));
        emit(
            &json!({"type": "message_update", "usage": {}, "assistantMessageEvent": {"type": "text_delta", "contentIndex": 1, "delta": text}}),
        );
        emit(&json!({"type": "message_end", "message": message}));
        emit(&json!({"type": "turn_end", "message": message, "toolResults": []}));
        emit(&json!({"type": "agent_end", "messages": [], "willRetry": false}));
        emit(&json!({"type": "agent_settled", "aborted": aborted}));
    }

    fn play(&mut self, prompt: &str) {
        self.prompts += 1;
        self.shared.aborted.store(false, Ordering::SeqCst);
        match prompt {
            "tool" => {
                // A U+2028 inside the result: a record boundary only for readers that split on more than LF.
                self.turn("bash", &json!({"command": "ls"}), "a.txt\u{2028}b.txt");
                self.finish("Done.");
            }
            "loop" => {
                while !self.aborted() {
                    self.turn("bash", &json!({"command": "ls checks"}), "T000.smoke.sh");
                }
                self.finish("stopped");
            }
            "steer" => {
                let mut steer = None;
                while !self.aborted() && steer.is_none() {
                    self.turn("read", &json!({"path": "big.md"}), "x");
                    steer = self.take_steer();
                }
                self.finish(&format!("steered: {}", steer.unwrap_or_default()));
            }
            "error" => {
                self.turn("read", &json!({"path": "big.md"}), &"x".repeat(100));
                let message = json!({
                    "role": "assistant", "content": [], "stopReason": "error",
                    "errorMessage": "400: {\"message\":\"Prompt (98353 tokens) requires ~4406MB GPU memory but only ~3494MB is available. Maximum context at KV8 and prefill chunk 512: 61488 tokens.\"}",
                    "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
                });
                emit(&json!({"type": "turn_start"}));
                emit(&json!({"type": "message_end", "message": message}));
                emit(&json!({"type": "turn_end", "message": message, "toolResults": []}));
                emit(&json!({"type": "agent_end", "messages": [], "willRetry": false}));
                emit(&json!({"type": "agent_settled", "aborted": false}));
            }
            "stderr" => {
                eprintln!("fake stderr: diagnostics go here");
                self.finish("logged");
            }
            "linger" => {
                self.shared.linger.store(true, Ordering::SeqCst);
                self.finish("lingering");
            }
            other => self.finish(&format!("echo: {other}")),
        }
    }
}
