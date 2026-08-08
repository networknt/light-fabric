#!/usr/bin/env node
import OpenAI from "openai";
import { VERSION } from "openai/version";

function required(name) {
  const value = (process.env[name] || "").trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

const client = new OpenAI({
  baseURL: required("LLM_SDK_BASE_URL"),
  apiKey: required("LLM_SDK_API_KEY"),
  maxRetries: 0,
  timeout: 30_000,
});
const model = required("LLM_SDK_EMBEDDING_MODEL");
const dimensions = Number.parseInt(required("LLM_SDK_EMBEDDING_DIMENSIONS"), 10);
const single = await client.embeddings.create({ model, input: "ready", encoding_format: "float" });
const batch = await client.embeddings.create({ model, input: ["one", "two"], encoding_format: "float" });
const encoded = await client.embeddings.create({ model, input: "ready", encoding_format: "base64" });
const explicit = await client.embeddings.create({
  model,
  input: "ready",
  encoding_format: "float",
  dimensions,
});
const operations = {
  singleFloat: single.data.length === 1 && single.data[0].index === 0,
  batchFloat: batch.data.map((item) => item.index).join(",") === "0,1",
  singleBase64: encoded.data.length === 1 && typeof encoded.data[0].embedding === "string",
  explicitDimensions: explicit.data[0].embedding.length === dimensions,
};
process.stdout.write(`${JSON.stringify({
  client: "typescript",
  sdkPackage: "openai",
  sdkVersion: VERSION,
  operations,
  status: Object.values(operations).every(Boolean) ? "pass" : "fail",
})}\n`);
