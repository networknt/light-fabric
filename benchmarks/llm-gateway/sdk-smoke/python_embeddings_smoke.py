#!/usr/bin/env python3
"""Exercise /v1/embeddings with the official OpenAI Python SDK."""

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
    client = OpenAI(
        base_url=required("LLM_SDK_BASE_URL"),
        api_key=required("LLM_SDK_API_KEY"),
        max_retries=0,
        timeout=30.0,
    )
    model = required("LLM_SDK_EMBEDDING_MODEL")
    dimensions = int(required("LLM_SDK_EMBEDDING_DIMENSIONS"))
    single = client.embeddings.create(model=model, input="ready", encoding_format="float")
    batch = client.embeddings.create(model=model, input=["one", "two"], encoding_format="float")
    encoded = client.embeddings.create(model=model, input="ready", encoding_format="base64")
    explicit = client.embeddings.create(
        model=model,
        input="ready",
        encoding_format="float",
        dimensions=dimensions,
    )
    operations = {
        "singleFloat": len(single.data) == 1 and single.data[0].index == 0,
        "batchFloat": [item.index for item in batch.data] == [0, 1],
        "singleBase64": len(encoded.data) == 1 and isinstance(encoded.data[0].embedding, str),
        "explicitDimensions": len(explicit.data[0].embedding) == dimensions,
    }
    print(json.dumps({
        "client": "python",
        "sdkPackage": "openai",
        "sdkVersion": importlib.metadata.version("openai"),
        "operations": operations,
        "status": "pass" if all(operations.values()) else "fail",
    }, sort_keys=True))


if __name__ == "__main__":
    main()
