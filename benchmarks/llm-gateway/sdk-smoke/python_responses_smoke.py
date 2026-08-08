#!/usr/bin/env python3
"""Exercise /v1/responses with the official OpenAI Python SDK."""

import importlib.metadata
import json
import os

from openai import OpenAI


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise RuntimeError(f"{name} is required")
    return value


def main() -> None:
    client = OpenAI(base_url=required("LLM_SDK_BASE_URL"), api_key=required("LLM_SDK_API_KEY"), max_retries=0, timeout=30.0)
    model = required("LLM_SDK_RESPONSES_MODEL")
    simple = client.responses.create(model=model, input="Reply with ready.", store=False)
    typed = client.responses.create(model=model, input=[{"role": "user", "content": [{"type": "input_text", "text": "Reply with ready."}]}], store=False)
    first = client.responses.create(model=model, input="Call weather.", tools=[{"type": "function", "name": "weather", "description": "Weather", "parameters": {"type": "object", "properties": {}}}], store=False)
    call = next((item for item in first.output if item.type == "function_call"), None)
    loop_ok = False
    if call is not None:
        second = client.responses.create(model=model, input=[
            {"role": "user", "content": "Call weather."},
            {"type": "function_call", "call_id": call.call_id, "name": call.name, "arguments": call.arguments},
            {"type": "function_call_output", "call_id": call.call_id, "output": "sunny"},
        ], store=False)
        loop_ok = second.status in ("completed", "incomplete") and bool(second.output_text.strip())
    event_types = []
    with client.responses.stream(model=model, input="Reply with ready.", store=False) as stream:
        for event in stream:
            event_types.append(event.type)
    operations = {
        "stringInput": simple.status in ("completed", "incomplete"),
        "typedInput": typed.status in ("completed", "incomplete"),
        "functionLoop": loop_ok,
        "streaming": "response.created" in event_types and "response.completed" in event_types,
    }
    print(json.dumps({"client": "python", "sdkPackage": "openai", "sdkVersion": importlib.metadata.version("openai"), "operations": operations, "status": "pass" if all(operations.values()) else "fail"}, sort_keys=True))


if __name__ == "__main__":
    main()
