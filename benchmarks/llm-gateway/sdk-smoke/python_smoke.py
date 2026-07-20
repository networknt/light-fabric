#!/usr/bin/env python3
"""Exercise the gateway through the official OpenAI Python SDK.

The script emits only booleans and SDK metadata. Provider output, prompts,
headers, URLs, credentials, and physical deployment identifiers are never
written to qualification evidence.
"""

import importlib.metadata
import json
import os

from openai import OpenAI


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise RuntimeError(f"{name} is required")
    return value


def exercise(client: OpenAI, model: str) -> dict[str, bool]:
    models = client.models.list()
    models_ok = any(item.id == model for item in models.data)

    buffered = client.chat.completions.create(
        model=model,
        messages=[{"role": "user", "content": "Reply with the word ready."}],
        max_tokens=16,
    )
    buffered_ok = bool(buffered.choices)

    stream = client.chat.completions.create(
        model=model,
        messages=[{"role": "user", "content": "Reply with the word stream."}],
        max_tokens=16,
        stream=True,
        stream_options={"include_usage": True},
    )
    saw_chunk = False
    saw_finish = False
    saw_usage = False
    for chunk in stream:
        saw_chunk = True
        saw_finish = saw_finish or any(choice.finish_reason is not None for choice in chunk.choices)
        saw_usage = saw_usage or chunk.usage is not None

    tool = client.chat.completions.create(
        model=model,
        messages=[{"role": "user", "content": "Call readiness_check with value ready."}],
        tools=[{
            "type": "function",
            "function": {
                "name": "readiness_check",
                "description": "Records a readiness value.",
                "parameters": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"],
                    "additionalProperties": False,
                },
            },
        }],
        tool_choice="required",
        max_tokens=64,
    )
    tool_ok = bool(tool.choices and tool.choices[0].message.tool_calls)

    return {
        "models": models_ok,
        "bufferedChat": buffered_ok,
        "streamingChat": saw_chunk and saw_finish and saw_usage,
        "toolCall": tool_ok,
    }


def main() -> None:
    client = OpenAI(
        base_url=required("LLM_SDK_BASE_URL"),
        api_key=required("LLM_SDK_API_KEY"),
        max_retries=0,
        timeout=30.0,
    )
    providers = {
        "openai": exercise(client, required("LLM_SDK_OPENAI_MODEL")),
        "anthropic": exercise(client, required("LLM_SDK_ANTHROPIC_MODEL")),
    }
    passed = all(all(operations.values()) for operations in providers.values())
    print(json.dumps({
        "client": "python",
        "sdkPackage": "openai",
        "sdkVersion": importlib.metadata.version("openai"),
        "providers": providers,
        "status": "pass" if passed else "fail",
    }, sort_keys=True))


if __name__ == "__main__":
    main()
