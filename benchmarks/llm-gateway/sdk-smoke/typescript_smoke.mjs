#!/usr/bin/env node
// Exercise the gateway through the official OpenAI TypeScript SDK. The output
// is deliberately metadata-only; provider output and security material never
// enter qualification evidence.
import OpenAI from "openai";
import { VERSION } from "openai/version";

function required(name) {
  const value = (process.env[name] || "").trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

async function exercise(client, model) {
  const models = await client.models.list();
  const modelsOk = models.data.some((item) => item.id === model);

  const buffered = await client.chat.completions.create({
    model,
    messages: [{ role: "user", content: "Reply with the word ready." }],
    max_tokens: 16,
  });

  const stream = await client.chat.completions.create({
    model,
    messages: [{ role: "user", content: "Reply with the word stream." }],
    max_tokens: 16,
    stream: true,
    stream_options: { include_usage: true },
  });
  let sawChunk = false;
  let sawFinish = false;
  let sawUsage = false;
  for await (const chunk of stream) {
    sawChunk = true;
    sawFinish ||= chunk.choices.some((choice) => choice.finish_reason != null);
    sawUsage ||= chunk.usage != null;
  }

  const tool = await client.chat.completions.create({
    model,
    messages: [{ role: "user", content: "Call readiness_check with value ready." }],
    tools: [{
      type: "function",
      function: {
        name: "readiness_check",
        description: "Records a readiness value.",
        parameters: {
          type: "object",
          properties: { value: { type: "string" } },
          required: ["value"],
          additionalProperties: false,
        },
      },
    }],
    tool_choice: "required",
    max_tokens: 64,
  });

  return {
    models: modelsOk,
    bufferedChat: buffered.choices.length > 0,
    streamingChat: sawChunk && sawFinish && sawUsage,
    toolCall: Boolean(tool.choices[0]?.message?.tool_calls?.length),
  };
}

const client = new OpenAI({
  baseURL: required("LLM_SDK_BASE_URL"),
  apiKey: required("LLM_SDK_API_KEY"),
  maxRetries: 0,
  timeout: 30_000,
});
const providers = {
  openai: await exercise(client, required("LLM_SDK_OPENAI_MODEL")),
  anthropic: await exercise(client, required("LLM_SDK_ANTHROPIC_MODEL")),
};
const passed = Object.values(providers).every((operations) =>
  Object.values(operations).every(Boolean));
process.stdout.write(`${JSON.stringify({
  client: "typescript",
  sdkPackage: "openai",
  sdkVersion: VERSION,
  providers,
  status: passed ? "pass" : "fail",
})}\n`);
