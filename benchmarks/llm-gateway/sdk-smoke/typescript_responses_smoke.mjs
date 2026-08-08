#!/usr/bin/env node
import OpenAI from "openai";
import { VERSION } from "openai/version";

function required(name) {
  const value = (process.env[name] || "").trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

const client = new OpenAI({ baseURL: required("LLM_SDK_BASE_URL"), apiKey: required("LLM_SDK_API_KEY"), maxRetries: 0, timeout: 30_000 });
const model = required("LLM_SDK_RESPONSES_MODEL");
const simple = await client.responses.create({ model, input: "Reply with ready.", store: false });
const typed = await client.responses.create({ model, input: [{ role: "user", content: [{ type: "input_text", text: "Reply with ready." }] }], store: false });
const first = await client.responses.create({ model, input: "Call weather.", tools: [{ type: "function", name: "weather", description: "Weather", parameters: { type: "object", properties: {} } }], store: false });
const call = first.output.find((item) => item.type === "function_call");
let loopOk = false;
if (call) {
  const second = await client.responses.create({ model, input: [
    { role: "user", content: "Call weather." },
    { type: "function_call", call_id: call.call_id, name: call.name, arguments: call.arguments },
    { type: "function_call_output", call_id: call.call_id, output: "sunny" },
  ], store: false });
  loopOk = ["completed", "incomplete"].includes(second.status) && second.output_text.trim().length > 0;
}
const stream = await client.responses.create({ model, input: "Reply with ready.", store: false, stream: true });
const eventTypes = [];
for await (const event of stream) eventTypes.push(event.type);
const operations = {
  stringInput: ["completed", "incomplete"].includes(simple.status),
  typedInput: ["completed", "incomplete"].includes(typed.status),
  functionLoop: loopOk,
  streaming: eventTypes.includes("response.created") && eventTypes.includes("response.completed"),
};
process.stdout.write(`${JSON.stringify({ client: "typescript", sdkPackage: "openai", sdkVersion: VERSION, operations, status: Object.values(operations).every(Boolean) ? "pass" : "fail" })}\n`);
