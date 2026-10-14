# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "langchain==1.4.5",
#   "langchain-google-genai==4.4.2",
#   "opentelemetry-sdk==1.45.1",
#   "opentelemetry-exporter-otlp-proto-grpc==1.45.1",
# ]
# ///
"""External LangChain agent smoke test; not part of the Breakspan runtime.

Start `cargo run -p breakspan-cli -- listen`, then:
    uv run examples/langchain_gemini_agent.py

GEMINI_API_KEY is read locally, or prompted for without saving. Only operation
metadata is exported, in one batch after the agent finishes (including failures).
Run credential-free tests with the same command plus `--self-test`.
"""

import argparse
import getpass
import math
import os
import sys
import threading
import unittest
import warnings
from pathlib import Path
from typing import Literal

from google.genai.errors import APIError
from langchain.agents import create_agent
from langchain_core.callbacks import BaseCallbackHandler
from langchain_core.messages import AIMessage, ToolMessage
from langchain_core.tools import tool
from langchain_google_genai import ChatGoogleGenerativeAI
from langsmith import tracing_context
from opentelemetry import trace
from opentelemetry.context import Context
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor, SpanExportResult
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from opentelemetry.sdk.trace.sampling import ALWAYS_ON
from opentelemetry.trace import SpanKind, StatusCode

MODEL = "gemini-3.5-flash-lite"
ENDPOINT = "http://127.0.0.1:4317"
PROMPT = "Use the calculator tool to multiply 12345 by 6789, then report the answer."
SYSTEM_PROMPT = (
    "You are a calculator agent. Always use the calculator tool for arithmetic. "
    "After receiving the tool result, give a short final answer."
)


@tool
def calculator(
    operation: Literal["add", "subtract", "multiply", "divide"], a: float, b: float
) -> float:
    """Apply an arithmetic operation to two finite numbers, a and b."""
    if not math.isfinite(a) or not math.isfinite(b):
        raise ValueError("Operands must be finite numbers.")
    if operation == "add":
        result = a + b
    elif operation == "subtract":
        result = a - b
    elif operation == "multiply":
        result = a * b
    elif operation == "divide":
        if b == 0:
            raise ValueError("Cannot divide by zero.")
        result = a / b
    else:
        raise ValueError("Unsupported operation.")
    if not math.isfinite(result):
        raise ValueError("Result must be finite.")
    return result


def provider_http_status(error):
    # LangChain preserves server codes but wraps client errors with `from e`.
    # Inspect only SDK errors and numeric HTTP codes, never messages/headers/body.
    for _ in range(8):
        if isinstance(error, APIError):
            code = error.code
            if type(code) is int and 400 <= code <= 599:
                return code
        error = error.__cause__
        if error is None:
            break
    return None


def agent_failure_message(error):
    code = provider_http_status(error)
    label = type(error).__name__
    if code is not None:
        label += f", HTTP {code}"
    hints = {
        400: "Gemini rejected the request. Check the API key and model/request settings.",
        401: "Gemini rejected authentication. Verify GEMINI_API_KEY locally.",
        403: "Gemini denied access. Check API-key restrictions and model permissions.",
        404: f"Gemini could not find the model/API route. Verify access to {MODEL}.",
        429: "Gemini reported a rate/quota limit. Check your quota and wait before retrying.",
    }
    if code is not None and code >= 500:
        hint = (
            "Gemini returned a server error, possibly a backend outage/overload or a "
            "request-specific failure. Retry later; if persistent, test the model in "
            "Google AI Studio. This example does not retry or switch models automatically."
        )
    else:
        hint = hints.get(
            code,
            f"Check your API key, access to {MODEL}, quota, and network. "
            "No more specific safe diagnostic is available.",
        )
    return f"Agent failed ({label}). {hint} Raw error details remain redacted."


def mark_error(span, error_type):
    # Never attach exception messages, stack traces, or exception events.
    span.set_status(StatusCode.ERROR)
    span.set_attribute("error.type", error_type)


class OperationSpans(BaseCallbackHandler):
    """Example-only callback: model/tool operations are direct agent children.

    LangGraph's internal chain/node runs are deliberately omitted. Explicit
    parent context and run-ID tracking also work for tool callbacks on workers.
    """

    run_inline = True

    def __init__(self, tracer, agent_span):
        self.tracer = tracer
        self.parent = trace.set_span_in_context(agent_span, Context())
        self.spans = {}
        self.lock = threading.Lock()

    def _start(self, run_id, name, attributes, kind=SpanKind.INTERNAL):
        span = self.tracer.start_span(
            name, context=self.parent, attributes=attributes, kind=kind
        )
        with self.lock:
            self.spans[run_id] = span

    def _end(self, run_id, error=None):
        with self.lock:
            span = self.spans.pop(run_id, None)
        if span is not None:
            if error is not None:
                mark_error(span, type(error).__name__)
            else:
                span.set_status(StatusCode.OK)
            span.end()

    def on_chat_model_start(self, serialized, messages, *, run_id, **kwargs):
        self._start(
            run_id,
            f"chat {MODEL}",
            {
                "gen_ai.operation.name": "chat",
                "gen_ai.provider.name": "gcp.gemini",
                "gen_ai.request.model": MODEL,
            },
            SpanKind.CLIENT,
        )

    def on_llm_end(self, response, *, run_id, **kwargs):
        self._end(run_id)

    def on_llm_error(self, error, *, run_id, **kwargs):
        self._end(run_id, error)

    def on_tool_start(self, serialized, input_str, *, run_id, **kwargs):
        self._start(
            run_id,
            "execute_tool calculator",
            {
                "gen_ai.operation.name": "execute_tool",
                "gen_ai.tool.name": "calculator",
            },
        )

    def on_tool_end(self, output, *, run_id, **kwargs):
        self._end(run_id)

    def on_tool_error(self, error, *, run_id, **kwargs):
        self._end(run_id, error)

    def close(self):
        with self.lock:
            remaining = list(self.spans.values())
            self.spans.clear()
        for span in remaining:
            mark_error(span, "IncompleteOperation")
            span.end()


def run_agent(model, tracer, prompt=PROMPT):
    # Disable LangSmith tracing even if enabled in the caller's environment.
    with (
        tracing_context(enabled=False),
        tracer.start_as_current_span(
            "invoke_agent calculator",
            context=Context(),
            attributes={
                "gen_ai.operation.name": "invoke_agent",
                "gen_ai.agent.name": "langchain-calculator",
            },
            record_exception=False,
            set_status_on_exception=False,
        ) as root,
    ):
        callbacks = OperationSpans(tracer, root)
        try:
            agent = create_agent(
                model=model, tools=[calculator], system_prompt=SYSTEM_PROMPT
            )
            result = agent.invoke(
                {"messages": [{"role": "user", "content": prompt}]},
                config={"callbacks": [callbacks], "recursion_limit": 8},
            )
            messages = result["messages"]
            if not any(
                isinstance(message, ToolMessage)
                and message.name == "calculator"
                and message.status == "success"
                for message in messages
            ):
                raise RuntimeError("Agent did not complete a calculator call.")
            if (
                not isinstance(messages[-1], AIMessage)
                or messages[-1].tool_calls
                or not messages[-1].text.strip()
            ):
                raise RuntimeError("Agent did not produce a final answer.")
            root.set_status(StatusCode.OK)
            return messages[-1].text
        except BaseException as error:
            mark_error(root, type(error).__name__)
            raise
        finally:
            callbacks.close()


def collected_telemetry():
    # Resource.create would also read arbitrary OTEL_RESOURCE_ATTRIBUTES.
    provider = TracerProvider(
        resource=Resource({"service.name": "breakspan-langchain-gemini-demo"}),
        sampler=ALWAYS_ON,
        shutdown_on_exit=False,
    )
    collector = InMemorySpanExporter()
    provider.add_span_processor(SimpleSpanProcessor(collector))
    return provider, collector


def export_trace(collector, endpoint):
    # An empty header map falls back to OTEL_* auth headers in this SDK.
    # One harmless explicit header prevents that fallback.
    exporter = OTLPSpanExporter(
        endpoint=endpoint,
        insecure=True,
        headers={"x-breakspan-demo": "langchain-gemini"},
        timeout=2,
    )
    try:
        return (
            exporter.export(collector.get_finished_spans()) == SpanExportResult.SUCCESS
        )
    finally:
        exporter.shutdown()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("prompt", nargs="?", default=PROMPT)
    parser.add_argument(
        "--endpoint", default=ENDPOINT, help="plaintext OTLP/gRPC receiver"
    )
    parser.add_argument(
        "--self-test", action="store_true", help="offline tests; no Gemini calls"
    )
    args = parser.parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.discover(
            str(Path(__file__).parent), pattern="test_langchain_gemini_agent.py"
        )
        return (
            0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
        )

    key = os.environ.get("GEMINI_API_KEY")
    if not key:
        if not sys.stdin.isatty():
            print(
                "Set GEMINI_API_KEY locally before running this example.",
                file=sys.stderr,
            )
            return 2
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("error", getpass.GetPassWarning)
                key = getpass.getpass("Gemini API key (hidden, not saved): ")
        except getpass.GetPassWarning:
            print(
                "Hidden input unavailable. Set GEMINI_API_KEY locally instead.",
                file=sys.stderr,
            )
            return 2
        except (EOFError, KeyboardInterrupt):
            return 130
        if not key.strip():
            print("A Gemini API key is required.", file=sys.stderr)
            return 2

    provider, collector = collected_telemetry()
    model = None
    exit_code = 0
    try:
        model = ChatGoogleGenerativeAI(
            model=MODEL,
            api_key=key,
            vertexai=False,
            timeout=30,
            max_retries=0,
            max_tokens=1024,
            thinking_level="low",
        )
        print(f"Running LangChain calculator agent with {MODEL}...")
        print(
            run_agent(
                model,
                provider.get_tracer("breakspan.langchain-demo", "0.1.0"),
                args.prompt,
            )
        )
    except KeyboardInterrupt:
        exit_code = 130
    except Exception as error:
        # Provider errors may contain credentials, request payloads or content.
        print(agent_failure_message(error), file=sys.stderr)
        exit_code = 1
    finally:
        try:
            if collector.get_finished_spans():
                if export_trace(collector, args.endpoint):
                    print(
                        "OTLP export completed. Check the Breakspan terminal for the trace."
                    )
                else:
                    print(
                        "OTLP export failed. Start Breakspan and check --endpoint.",
                        file=sys.stderr,
                    )
                    exit_code = exit_code or 1
        except Exception as error:
            print(
                f"OTLP export failed ({type(error).__name__}). Check --endpoint.",
                file=sys.stderr,
            )
            exit_code = exit_code or 1
        finally:
            provider.shutdown()
            if model is not None and model.client is not None:
                model.client.close()
    return exit_code


if __name__ == "__main__":
    # Keep model answers printable when Windows redirects stdout to a pipe.
    sys.stdout.reconfigure(encoding="utf-8")
    raise SystemExit(main())
