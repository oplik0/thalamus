use super::*;

fn protocol_text(value: &Value, anthropic: bool) -> bool {
    if anthropic {
        value["content"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|v| v["type"] == "text" && text(&v["text"]))
        })
    } else {
        value["output"].as_array().is_some_and(|items| {
            items.iter().any(|v| {
                v["content"].as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|p| p["type"] == "output_text" && text(&p["text"]))
                })
            })
        })
    }
}

fn reports_blue(value: &Value, anthropic: bool) -> bool {
    let contains = |v: &Value| {
        v.as_str()
            .is_some_and(|s| s.to_ascii_lowercase().contains("blue"))
    };
    if anthropic {
        value["content"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|v| v["type"] == "text" && contains(&v["text"]))
        })
    } else {
        value["output"].as_array().is_some_and(|items| {
            items.iter().any(|v| {
                v["content"].as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|p| p["type"] == "output_text" && contains(&p["text"]))
                })
            })
        })
    }
}

fn protocol_body(config: &Config, anthropic: bool, input: &str) -> Value {
    if anthropic {
        json!({"model":config.model,"messages":[{"role":"user","content":input}],"max_tokens":128})
    } else {
        json!({"model":config.model,"input":input,"max_output_tokens":128})
    }
}

pub(super) fn protocol_checks(r: &mut Runner<'_>, checks: &mut Vec<Check>, budget: u32) {
    for (enabled, anthropic, prefix, path) in [
        (r.config.responses, false, "responses", "responses"),
        (r.config.anthropic, true, "anthropic", "messages"),
    ] {
        let names = [
            prefix.to_owned(),
            format!("{prefix}_authentication"),
            format!("{prefix}_authorization"),
            format!("{prefix}_streaming"),
            format!("{prefix}_tool_loop"),
            format!("{prefix}_context_limit"),
        ];
        if !enabled {
            for name in names {
                checks.push(unsupported(&name, "client", "protocol not configured"));
            }
            continue;
        }
        checks.push(check(
            &names[1],
            "user_visible",
            (|| {
                let (s, _) = r.post(
                    path,
                    "thalamus-invalid-validation-key",
                    protocol_body(r.config, anthropic, "Hello"),
                    false,
                )?;
                if s == 401 || s == 403 {
                    Ok(format!("invalid credential rejected: HTTP {s}"))
                } else {
                    Err(format!("invalid credential was not rejected: HTTP {s}"))
                }
            })(),
        ));
        checks.push(check(
            &names[2],
            "user_visible",
            (|| {
                let mut b = protocol_body(r.config, anthropic, "Hello");
                b["model"] = json!(r.config.forbidden_model);
                let (s, _) = r.post(path, r.key, b, false)?;
                if s == 403 {
                    Ok("forbidden model rejected: HTTP 403".into())
                } else {
                    Err(format!("expected HTTP 403 for forbidden model, got {s}"))
                }
            })(),
        ));
        checks.push(check(
            &names[0],
            "user_visible",
            (|| {
                let v = r.json(
                    path,
                    protocol_body(r.config, anthropic, "Reply with a short greeting."),
                )?;
                if protocol_text(&v, anthropic) {
                    Ok("authenticated response returned content".into())
                } else {
                    Err("response returned no content".into())
                }
            })(),
        ));
        checks.push(check(
            &names[3],
            "client",
            (|| {
                let mut b = protocol_body(r.config, anthropic, "Reply with a short greeting.");
                b["stream"] = json!(true);
                let (s, data) = r.post(path, r.key, b, true)?;
                if !(200..300).contains(&s) {
                    return Err(format!("HTTP {s}"));
                }
                let mut content = false;
                let mut done = false;
                for line in data.lines() {
                    if let Some(d) = line.strip_prefix("data:") {
                        let d = d.trim();
                        if d == "[DONE]" {
                            continue;
                        }
                        let v: Value = serde_json::from_str(d).map_err(|_| "invalid SSE JSON")?;
                        if anthropic {
                            content |=
                                v["type"] == "content_block_delta" && text(&v["delta"]["text"]);
                            done |= v["type"] == "message_stop";
                            if v["type"] == "error" {
                                return Err("stream reported error".into());
                            }
                        } else {
                            content |=
                                v["type"] == "response.output_text.delta" && text(&v["delta"]);
                            done |= v["type"] == "response.completed";
                            if v["type"] == "error" || v["type"] == "response.failed" {
                                return Err("stream reported error".into());
                            }
                        }
                    }
                }
                if content && done {
                    Ok("SSE content and protocol terminal event received".into())
                } else {
                    Err("stream missing content or terminal event".into())
                }
            })(),
        ));
        if r.config.tool_parser.is_none() {
            checks.push(unsupported(
                &names[4],
                "client",
                "no tool parser configured",
            ));
        } else {
            checks.push(check(&names[4], "client", (|| {
                r.reserve_tool()?;
                let prompt = "Call the lookup tool for key alpha, then report its result.";
                let mut b = protocol_body(r.config, anthropic, prompt);
                let schema = json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"]});
                if anthropic {
                    b["tools"] = json!([{"name":"lookup","description":"Look up a key","input_schema":schema}]);
                    b["tool_choice"] = json!({"type":"tool","name":"lookup"});
                } else {
                    b["tools"] = json!([{"type":"function","name":"lookup","parameters":schema}]);
                    b["tool_choice"] = json!({"type":"function","name":"lookup"});
                    b["store"] = json!(true);
                }
                let first = r.json(path, b)?;
                let items = if anthropic { first["content"].as_array() } else { first["output"].as_array() }.ok_or("missing tool output")?;
                let tool_type = if anthropic { "tool_use" } else { "function_call" };
                let calls: Vec<_> = items.iter().filter(|v| v["type"] == tool_type).collect();
                if calls.len() != 1 || calls[0]["name"] != "lookup" { return Err("expected exactly one lookup call".into()); }
                let call = calls[0];
                let args: Value = if anthropic { call["input"].clone() } else {
                    serde_json::from_str(call["arguments"].as_str().ok_or("missing tool arguments")?).map_err(|_| "invalid tool arguments")?
                };
                if args["key"] != "alpha" { return Err("unexpected tool arguments".into()); }
                let reply = if anthropic {
                    let id = call["id"].as_str().filter(|s| !s.is_empty()).ok_or("missing tool id")?;
                    r.json(path, json!({"model":r.config.model,"max_tokens":128,"messages":[{"role":"user","content":prompt},{"role":"assistant","content":items},{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"alpha is blue"}]}]}))?
                } else {
                    let id = call["call_id"].as_str().filter(|s| !s.is_empty()).ok_or("missing call id")?;
                    let response_id = first["id"].as_str().filter(|s| !s.is_empty()).ok_or("missing response id")?;
                    r.json(path, json!({"model":r.config.model,"max_output_tokens":128,"previous_response_id":response_id,"input":[{"type":"function_call_output","call_id":id,"output":"alpha is blue"}]}))?
                };
                if !reports_blue(&reply, anthropic) { return Err("tool continuation did not report the tool result".into()); }
                Ok("tool call and continuation succeeded".into())
            })()));
        }
        checks.push(check(
            &names[5],
            "engine",
            (|| {
                let v = r.json(
                    path,
                    protocol_body(r.config, anthropic, &r.config.context_prompt),
                )?;
                let tokens = v
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .ok_or("missing prompt token usage; cannot establish accepted context")?;
                if tokens < u64::from(r.config.context_tokens) || tokens > u64::from(budget) {
                    return Err(format!(
                        "prompt token count {tokens} outside configured target and budget"
                    ));
                }
                if !protocol_text(&v, anthropic) {
                    return Err("context request returned no content".into());
                }
                Ok(format!(
                    "accepted {tokens} prompt tokens; quality under load not measured"
                ))
            })(),
        ));
    }
}
