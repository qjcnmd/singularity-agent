use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const TINY_PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

pub fn find_tool_result<'a>(body: &'a Value, call_id: &str) -> &'a Value {
    body.get("messages")
        .and_then(Value::as_array)
        .expect("messages array")
        .iter()
        .filter_map(|msg| msg.get("content").and_then(Value::as_array))
        .flatten()
        .find(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("tool_use_id").and_then(Value::as_str) == Some(call_id)
        })
        .unwrap_or_else(|| panic!("missing tool_result for {call_id}"))
}

fn sse_data(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

pub fn messages_text_sse(
    thinking: Option<(&str, &str)>,
    text: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    stop_reason: &str,
) -> Vec<String> {
    let mut events = vec![sse_data(&json!({
        "type": "message_start",
        "message": {
            "usage": {
                "input_tokens": input_tokens,
                "cache_read_input_tokens": cache_read_tokens,
                "output_tokens": 1
            }
        }
    }))];
    let mut index = 0u64;
    if let Some((thought, signature)) = thinking {
        events.push(sse_data(&json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "thinking", "thinking": "", "signature": "" }
        })));
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "thinking_delta", "thinking": thought }
        })));
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "signature_delta", "signature": signature }
        })));
        events.push(sse_data(&json!({ "type": "content_block_stop", "index": index })));
        index += 1;
    }
    if !text.is_empty() {
        events.push(sse_data(&json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": "" }
        })));
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "text_delta", "text": text }
        })));
        events.push(sse_data(&json!({ "type": "content_block_stop", "index": index })));
    }
    events.push(sse_data(&json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason },
        "usage": { "output_tokens": output_tokens }
    })));
    events.push(sse_data(&json!({ "type": "message_stop" })));
    events
}

pub fn messages_tools_sse(
    preface: Option<&str>,
    calls: Vec<(&str, &str, Value)>,
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: &str,
) -> Vec<String> {
    let mut events = vec![sse_data(&json!({
        "type": "message_start",
        "message": { "usage": { "input_tokens": input_tokens, "output_tokens": 1 } }
    }))];
    let mut index = 0u64;
    if let Some(text) = preface {
        events.push(sse_data(&json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": text }
        })));
        events.push(sse_data(&json!({ "type": "content_block_stop", "index": index })));
        index += 1;
    }
    for (id, name, args) in calls {
        events.push(sse_data(&json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
        })));
        let raw_json = serde_json::to_string(&args).expect("serialize tool args");
        let mid = raw_json.floor_char_boundary(raw_json.len() / 2);
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "input_json_delta", "partial_json": &raw_json[..mid] }
        })));
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "input_json_delta", "partial_json": &raw_json[mid..] }
        })));
        events.push(sse_data(&json!({ "type": "content_block_stop", "index": index })));
        index += 1;
    }
    events.push(sse_data(&json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason },
        "usage": { "output_tokens": output_tokens }
    })));
    events.push(sse_data(&json!({ "type": "message_stop" })));
    events
}

pub fn responses_sse(
    thinking: Option<(&str, &str)>,
    text: Option<&str>,
    calls: Vec<(&str, &str, Value)>,
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    truncated: bool,
) -> Vec<String> {
    let mut events = Vec::new();
    let mut output = Vec::new();
    if let Some((summary, encrypted)) = thinking {
        events.push(sse_data(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": summary
        })));
        output.push(json!({
            "id": "rs_test_1",
            "type": "reasoning",
            "summary": [{ "type": "summary_text", "text": summary }],
            "encrypted_content": encrypted
        }));
    }
    if let Some(body) = text {
        events.push(sse_data(&json!({
            "type": "response.output_text.delta",
            "delta": body
        })));
        output.push(json!({
            "id": "msg_test_1",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": body }]
        }));
    }
    for (idx, (call_id, name, args)) in calls.into_iter().enumerate() {
        let raw_args = serde_json::to_string(&args).expect("serialize args");
        events.push(sse_data(&json!({
            "type": "response.function_call_arguments.delta",
            "delta": raw_args
        })));
        output.push(json!({
            "id": format!("fc_{idx}"),
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": raw_args
        }));
    }
    let terminal_type = if truncated {
        "response.incomplete"
    } else {
        "response.completed"
    };
    let mut response = json!({
        "id": "resp_test_1",
        "status": if truncated { "incomplete" } else { "completed" },
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "input_tokens_details": { "cached_tokens": cached_tokens }
        }
    });
    if truncated {
        response["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }
    events.push(sse_data(&json!({ "type": terminal_type, "response": response })));
    events
}

pub fn write_test_mcp_script(dir: &Path) -> PathBuf {
    let script = dir.join("test_mcp_server.mjs");
    let source = r#"
import { createInterface } from 'node:readline';

const tools = [
  {
    name: 'echo',
    description: 'Echo text with optional delay_ms',
    inputSchema: {
      type: 'object',
      properties: {
        text: { type: 'string' },
        delay_ms: { type: 'integer' }
      },
      required: ['text']
    }
  },
  {
    name: 'fail_tool',
    description: 'Always returns an error result',
    inputSchema: { type: 'object', properties: {} }
  },
  {
    name: 'large_output',
    description: 'Returns repetitive text of kb kilobytes',
    inputSchema: {
      type: 'object',
      properties: { kb: { type: 'integer' } },
      required: ['kb']
    }
  },
  {
    name: 'ping',
    description: 'Zero-arg ping tool',
    inputSchema: { type: 'object', properties: {} }
  },
  {
    name: 'echo_image',
    description: 'Returns text plus a tiny PNG image',
    inputSchema: { type: 'object', properties: {} }
  }
];

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + '\n');
}

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on('line', async (line) => {
  if (!line.trim()) return;
  let req;
  try { req = JSON.parse(line); } catch { return; }
  const { id, method, params } = req;
  if (method === 'initialize') {
    send({
      jsonrpc: '2.0',
      id,
      result: {
        protocolVersion: params?.protocolVersion ?? '2025-03-26',
        capabilities: { tools: {} },
        serverInfo: { name: 'test-mcp', version: '1.0.0' },
        instructions: 'TEST_MCP_INSTRUCTIONS_MARKER'
      }
    });
  } else if (method === 'notifications/initialized') {
    // no-op
  } else if (method === 'tools/list') {
    send({ jsonrpc: '2.0', id, result: { tools } });
  } else if (method === 'tools/call') {
    const name = params?.name;
    const args = params?.arguments ?? {};
    if (name === 'echo') {
      const delay = Number(args.delay_ms ?? 0);
      if (delay > 0) await new Promise((r) => setTimeout(r, delay));
      send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: `ECHO:${args.text ?? ''}` }] } });
    } else if (name === 'fail_tool') {
      send({ jsonrpc: '2.0', id, result: { isError: true, content: [{ type: 'text', text: 'SIMULATED_MCP_FAILURE' }] } });
    } else if (name === 'large_output') {
      const kb = Math.max(1, Number(args.kb ?? 16));
      const chunk = 'L'.repeat(1020) + '\n';
      send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: chunk.repeat(kb) }] } });
    } else if (name === 'ping') {
      send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: 'PONG' }] } });
    } else if (name === 'echo_image') {
      send({
        jsonrpc: '2.0',
        id,
        result: {
          content: [
            { type: 'text', text: 'MCP_IMAGE_TOOL_TEXT' },
            {
              type: 'image',
              data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
              mimeType: 'image/png'
            }
          ]
        }
      });
    } else {
      send({ jsonrpc: '2.0', id, error: { code: -32601, message: `Unknown tool ${name}` } });
    }
  } else if (id !== undefined) {
    send({ jsonrpc: '2.0', id, result: {} });
  }
});
"#;
    std::fs::write(&script, source.trim_start()).expect("write test MCP script");
    let display = script.to_string_lossy();
    PathBuf::from(display.strip_prefix(r"\\?\").unwrap_or(&display))
}
