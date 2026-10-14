"""Credential-free tests: uv run examples/langchain_gemini_agent.py --self-test."""

import io
import math
import os
import unittest
from concurrent.futures import ThreadPoolExecutor
from contextlib import redirect_stderr, redirect_stdout
from unittest.mock import patch
from uuid import uuid4

import grpc
import langchain_gemini_agent as demo
from google.genai import types
from google.genai.errors import ClientError, ServerError
from langchain_core.language_models.fake_chat_models import FakeMessagesListChatModel
from langchain_core.messages import AIMessage
from opentelemetry.proto.collector.trace.v1 import (
    trace_service_pb2,
    trace_service_pb2_grpc,
)
from opentelemetry.trace import StatusCode


class ToolCallingModel(FakeMessagesListChatModel):
    def bind_tools(self, tools, **kwargs):
        return self


def calculator_model(a=12345, b=6789, operation="multiply"):
    return ToolCallingModel(
        responses=[
            AIMessage(
                content="",
                tool_calls=[
                    {
                        "name": "calculator",
                        "args": {"operation": operation, "a": a, "b": b},
                        "id": "calculator-call",
                    }
                ],
            ),
            AIMessage(content="83810205"),
        ]
    )


class ExampleTests(unittest.TestCase):
    def setUp(self):
        self.provider, self.collector = demo.collected_telemetry()
        self.tracer = self.provider.get_tracer("test")
        self.addCleanup(self.provider.shutdown)

    def spans(self):
        return self.collector.get_finished_spans()

    def test_calculator_operations_and_rejected_values(self):
        for operation, expected in [
            ("add", 9),
            ("subtract", 3),
            ("multiply", 18),
            ("divide", 2),
        ]:
            with self.subTest(operation=operation):
                self.assertEqual(
                    demo.calculator.invoke({"operation": operation, "a": 6, "b": 3}),
                    expected,
                )
        for operation, a, b in [
            ("divide", 1, 0),
            ("multiply", math.inf, 1),
            ("add", math.nan, 1),
            ("multiply", 1e308, 1e308),
            ("execute", 1, 2),
        ]:
            with (
                self.subTest(operation=operation, a=a, b=b),
                self.assertRaises(ValueError),
            ):
                demo.calculator.invoke({"operation": operation, "a": a, "b": b})

    def test_real_langchain_agent_callbacks_and_metadata_only_tree(self):
        sentinel = "PRIVATE-PROMPT-CREDENTIAL-CONTENT"
        with patch.dict(os.environ, {"OTEL_RESOURCE_ATTRIBUTES": f"secret={sentinel}"}):
            provider, collector = demo.collected_telemetry()
        self.addCleanup(provider.shutdown)
        with (
            patch.dict(os.environ, {"LANGSMITH_TRACING": "true"}),
            patch("langsmith.Client.create_run") as cloud_trace,
        ):
            answer = demo.run_agent(
                calculator_model(), provider.get_tracer("test"), sentinel
            )
        cloud_trace.assert_not_called()
        self.assertEqual(answer, "83810205")
        spans = collector.get_finished_spans()
        self.assertEqual(len(spans), 4)
        root = next(span for span in spans if span.parent is None)
        self.assertEqual(root.attributes["gen_ai.operation.name"], "invoke_agent")
        children = sorted(
            (span for span in spans if span.parent), key=lambda span: span.start_time
        )
        self.assertEqual(
            [span.attributes["gen_ai.operation.name"] for span in children],
            ["chat", "execute_tool", "chat"],
        )
        for span in spans:
            self.assertEqual(span.context.trace_id, root.context.trace_id)
            self.assertEqual(span.status.status_code, StatusCode.OK)
            self.assertGreaterEqual(span.end_time, span.start_time)
            self.assertEqual(span.events, ())
            self.assertNotIn(sentinel, repr(span.attributes))
            self.assertNotIn("83810205", repr(span.attributes))
            self.assertEqual(
                dict(span.resource.attributes),
                {"service.name": "breakspan-langchain-gemini-demo"},
            )
        for span in children:
            self.assertEqual(span.parent.span_id, root.context.span_id)
        self.assertEqual(
            children[0].attributes["gen_ai.request.model"], "gemini-3.8-flash"
        )
        self.assertEqual(children[1].attributes["gen_ai.tool.name"], "calculator")

    def test_no_tool_call_is_not_a_successful_smoke_test(self):
        model = ToolCallingModel(
            responses=[AIMessage(content="I calculated mentally.")]
        )
        with self.assertRaisesRegex(RuntimeError, "calculator call"):
            demo.run_agent(model, self.tracer)
        self.assertEqual(len(self.spans()), 2)
        root = next(span for span in self.spans() if span.parent is None)
        self.assertEqual(root.status.status_code, StatusCode.ERROR)
        self.assertEqual(root.attributes["error.type"], "RuntimeError")
        self.assertEqual(root.events, ())

    def test_empty_final_answer_is_an_agent_error(self):
        model = calculator_model()
        model.responses[-1] = AIMessage(content="")
        with self.assertRaisesRegex(RuntimeError, "final answer"):
            demo.run_agent(model, self.tracer)
        root = next(span for span in self.spans() if span.parent is None)
        self.assertEqual(root.status.status_code, StatusCode.ERROR)

    def test_interruption_closes_model_and_agent_without_exception_events(self):
        class InterruptedModel(ToolCallingModel):
            def _generate(self, *args, **kwargs):
                raise KeyboardInterrupt()

        with self.assertRaises(KeyboardInterrupt):
            demo.run_agent(InterruptedModel(responses=[]), self.tracer)
        self.assertEqual(len(self.spans()), 2)
        for span in self.spans():
            self.assertEqual(span.status.status_code, StatusCode.ERROR)
            self.assertEqual(span.attributes["error.type"], "KeyboardInterrupt")
            self.assertEqual(span.events, ())

    def test_tool_failure_closes_agent_and_tool_without_error_content(self):
        with self.assertRaises(ValueError):
            demo.run_agent(calculator_model(a=1, b=0, operation="divide"), self.tracer)
        spans = self.spans()
        self.assertEqual(len(spans), 3)
        tool = next(
            span
            for span in spans
            if span.attributes["gen_ai.operation.name"] == "execute_tool"
        )
        root = next(span for span in spans if span.parent is None)
        for span in [tool, root]:
            self.assertEqual(span.status.status_code, StatusCode.ERROR)
            self.assertEqual(span.attributes["error.type"], "ValueError")
            self.assertIsNone(span.status.description)
            self.assertEqual(span.events, ())

    def test_callbacks_correlate_parallel_tools_and_clean_up_incomplete_model(self):
        root = self.tracer.start_span("root")
        callbacks = demo.OperationSpans(self.tracer, root)
        first, second, model = uuid4(), uuid4(), uuid4()
        callbacks.on_chat_model_start({}, [["secret prompt"]], run_id=model)
        with ThreadPoolExecutor(max_workers=2) as pool:
            list(
                pool.map(
                    lambda run: callbacks.on_tool_start(
                        {"secret": "key"}, "secret args", run_id=run
                    ),
                    [first, second],
                )
            )
            list(
                pool.map(
                    lambda run: callbacks.on_tool_end("secret output", run_id=run),
                    [second, first],
                )
            )
        callbacks.close()
        callbacks.close()
        root.end()
        spans = self.spans()
        self.assertEqual(len(spans), 4)
        model_span = next(
            span
            for span in spans
            if span.attributes.get("gen_ai.operation.name") == "chat"
        )
        self.assertEqual(model_span.attributes["error.type"], "IncompleteOperation")
        for span in spans[:-1]:
            self.assertEqual(span.parent.span_id, root.context.span_id)
            self.assertNotIn("secret", repr(span.attributes))
            self.assertEqual(span.events, ())

    def test_model_callback_errors_are_redacted(self):
        root = self.tracer.start_span("root")
        callbacks = demo.OperationSpans(self.tracer, root)
        run = uuid4()
        callbacks.on_chat_model_start({}, [], run_id=run)
        callbacks.on_llm_error(RuntimeError("secret API key and prompt"), run_id=run)
        callbacks.close()
        root.end()
        span = self.spans()[0]
        self.assertEqual(span.status.status_code, StatusCode.ERROR)
        self.assertEqual(span.attributes["error.type"], "RuntimeError")
        self.assertNotIn("secret", repr(span.attributes))
        self.assertIsNone(span.status.description)
        self.assertEqual(span.events, ())

    def test_otlp_grpc_exports_agent_spans_to_loopback(self):
        class Receiver(trace_service_pb2_grpc.TraceServiceServicer):
            def Export(self, request, context):
                self.request = request
                self.metadata = dict(context.invocation_metadata())
                return trace_service_pb2.ExportTraceServiceResponse()

        receiver = Receiver()
        server = grpc.server(ThreadPoolExecutor(max_workers=1))
        trace_service_pb2_grpc.add_TraceServiceServicer_to_server(receiver, server)
        port = server.add_insecure_port("127.0.0.1:0")
        self.assertNotEqual(port, 0)
        server.start()
        self.addCleanup(lambda: server.stop(0).wait())
        demo.run_agent(calculator_model(), self.tracer)
        with patch.dict(
            os.environ,
            {
                "OTEL_EXPORTER_OTLP_HEADERS": "authorization=private-general-key",
                "OTEL_EXPORTER_OTLP_TRACES_HEADERS": "authorization=private-trace-key",
            },
        ):
            self.assertTrue(
                demo.export_trace(self.collector, f"http://127.0.0.1:{port}")
            )
        self.assertNotIn("authorization", receiver.metadata)
        self.assertNotIn("private-", repr(receiver.metadata))
        self.assertEqual(receiver.metadata["x-breakspan-demo"], "langchain-gemini")
        spans = [
            span
            for resource in receiver.request.resource_spans
            for scope in resource.scope_spans
            for span in scope.spans
        ]
        self.assertEqual(len(spans), 4)
        root = next(span for span in spans if not span.parent_span_id)
        self.assertEqual(len(root.trace_id), 16)
        self.assertEqual(len(root.span_id), 8)
        self.assertTrue(all(span.trace_id == root.trace_id for span in spans))
        self.assertTrue(
            all(
                span.parent_span_id == root.span_id
                for span in spans
                if span is not root
            )
        )

    def test_gemini_sdk_function_calling_and_request_limits_without_network(self):
        model = demo.ChatGoogleGenerativeAI(
            model=demo.MODEL,
            api_key="offline-placeholder",
            vertexai=False,
            timeout=30,
            max_retries=0,
            max_tokens=1024,
            thinking_level="low",
        )
        self.addCleanup(model.client.close)
        responses = [
            types.GenerateContentResponse(
                candidates=[
                    types.Candidate(
                        content=types.Content(
                            role="model",
                            parts=[
                                types.Part(
                                    function_call=types.FunctionCall(
                                        name="calculator",
                                        args={
                                            "operation": "multiply",
                                            "a": 12345,
                                            "b": 6789,
                                        },
                                    ),
                                )
                            ],
                        ),
                        finish_reason="STOP",
                    )
                ]
            ),
            types.GenerateContentResponse(
                candidates=[
                    types.Candidate(
                        content=types.Content(
                            role="model", parts=[types.Part(text="83810205")]
                        ),
                        finish_reason="STOP",
                    )
                ]
            ),
        ]
        with patch.object(
            model.client.models, "generate_content", side_effect=responses
        ) as request:
            self.assertEqual(demo.run_agent(model, self.tracer), "83810205")
        self.assertEqual(request.call_count, 2)
        self.assertEqual(len(self.spans()), 4)
        for call in request.call_args_list:
            self.assertEqual(call.kwargs["model"], "gemini-3.8-flash")
            config = call.kwargs["config"]
            self.assertEqual(config.http_options.timeout, 30000)
            self.assertEqual(config.http_options.retry_options.attempts, 0)
            self.assertEqual(config.max_output_tokens, 1024)
            self.assertEqual(config.thinking_config.thinking_level, "LOW")
            self.assertEqual(
                config.tools[0].function_declarations[0].name, "calculator"
            )

    def test_real_sdk_errors_report_safe_http_codes_without_leaking_content(self):
        for code, hint in [
            (400, "rejected the request"),
            (401, "rejected authentication"),
            (403, "denied access"),
            (404, "could not find"),
            (429, "rate/quota limit"),
            (500, "server error"),
            (503, "server error"),
        ]:
            with self.subTest(code=code):
                model = demo.ChatGoogleGenerativeAI(
                    model=demo.MODEL,
                    api_key="offline-placeholder",
                    vertexai=False,
                    timeout=30,
                    max_retries=0,
                    max_tokens=1024,
                    thinking_level="low",
                )
                self.addCleanup(model.client.close)
                error_type = ClientError if code < 500 else ServerError
                error = error_type(
                    code,
                    {
                        "error": {
                            "status": "PRIVATE-STATUS",
                            "message": "PRIVATE-KEY PRIVATE-PROMPT",
                        }
                    },
                )
                spans = []

                def export(collector, endpoint):
                    spans.extend(collector.get_finished_spans())
                    return True

                stdout, stderr = io.StringIO(), io.StringIO()
                with (
                    patch.dict(os.environ, {"GEMINI_API_KEY": "PRIVATE-KEY"}),
                    patch.object(demo, "ChatGoogleGenerativeAI", return_value=model),
                    patch.object(
                        model.client.models, "generate_content", side_effect=error
                    ) as request,
                    patch.object(demo, "export_trace", side_effect=export),
                    redirect_stdout(stdout),
                    redirect_stderr(stderr),
                ):
                    self.assertEqual(demo.main([]), 1)
                self.assertEqual(request.call_count, 1)
                self.assertIn(f"HTTP {code}", stderr.getvalue())
                self.assertIn(hint, stderr.getvalue())
                self.assertNotIn("PRIVATE-", stdout.getvalue() + stderr.getvalue())
                self.assertEqual(len(spans), 2)
                for span in spans:
                    self.assertEqual(span.status.status_code, StatusCode.ERROR)
                    self.assertEqual(span.events, ())
                    self.assertNotIn("PRIVATE-", repr(span.attributes))

    def test_status_extraction_ignores_arbitrary_fields_and_handles_cause_cycles(self):
        error = RuntimeError("PRIVATE-KEY")
        error.code = 503  # Only a real SDK error is trusted for the code.
        self.assertIsNone(demo.provider_http_status(error))
        error.__cause__ = error
        self.assertIsNone(demo.provider_http_status(error))
        for invalid in [True, "PRIVATE-CODE", 503.0, 999]:
            sdk_error = ServerError(503, {})
            sdk_error.code = invalid
            with self.subTest(code=invalid):
                self.assertIsNone(demo.provider_http_status(sdk_error))
                self.assertNotIn("PRIVATE-", demo.agent_failure_message(sdk_error))
        wrapped = RuntimeError("PRIVATE-WRAPPER")
        wrapped.__cause__ = ServerError(503, {"message": "PRIVATE-KEY"})
        self.assertEqual(demo.provider_http_status(wrapped), 503)
        self.assertNotIn("PRIVATE-", demo.agent_failure_message(wrapped))

    def test_agent_iteration_limit_closes_all_spans(self):
        responses = []
        for index in range(10):
            message = calculator_model().responses[0]
            message.tool_calls[0]["id"] = f"call-{index}"
            responses.append(message)
        model = ToolCallingModel(responses=responses)
        with self.assertRaises(Exception) as failure:
            demo.run_agent(model, self.tracer)
        self.assertEqual(type(failure.exception).__name__, "GraphRecursionError")
        root = next(span for span in self.spans() if span.parent is None)
        self.assertEqual(root.status.status_code, StatusCode.ERROR)
        self.assertEqual(root.attributes["error.type"], "GraphRecursionError")
        self.assertTrue(all(span.end_time is not None for span in self.spans()))

    def test_export_failure_does_not_report_success_and_shuts_down_exporter(self):
        demo.run_agent(calculator_model(), self.tracer)
        with (
            patch.dict(
                os.environ, {"OTEL_EXPORTER_OTLP_HEADERS": "Authorization=private-key"}
            ),
            patch.object(demo, "OTLPSpanExporter") as exporter,
        ):
            exporter.return_value.export.return_value = demo.SpanExportResult.FAILURE
            self.assertFalse(demo.export_trace(self.collector, demo.ENDPOINT))
        self.assertEqual(
            exporter.call_args.kwargs["headers"],
            {"x-breakspan-demo": "langchain-gemini"},
        )
        self.assertEqual(exporter.call_args.kwargs["timeout"], 2)
        exporter.return_value.shutdown.assert_called_once()

    def test_missing_receiver_returns_export_failure(self):
        server = grpc.server(ThreadPoolExecutor(max_workers=1))
        port = server.add_insecure_port("127.0.0.1:0")
        self.assertNotEqual(port, 0)
        server.start()
        server.stop(0).wait()
        demo.run_agent(calculator_model(), self.tracer)
        self.assertFalse(demo.export_trace(self.collector, f"http://127.0.0.1:{port}"))

    def test_export_exception_still_closes_model_and_redacts_error(self):
        run_agent = demo.run_agent

        def invoke(model, tracer, prompt):
            return run_agent(calculator_model(), tracer, prompt)

        stderr = io.StringIO()
        with (
            patch.dict(os.environ, {"GEMINI_API_KEY": "private-key"}),
            patch.object(demo, "ChatGoogleGenerativeAI") as model,
            patch.object(demo, "run_agent", side_effect=invoke),
            patch.object(demo, "export_trace", side_effect=RuntimeError("private-key")),
            redirect_stdout(io.StringIO()),
            redirect_stderr(stderr),
        ):
            self.assertEqual(demo.main([]), 1)
        model.return_value.client.close.assert_called_once()
        self.assertNotIn("private-key", stderr.getvalue())
        self.assertIn("OTLP export failed (RuntimeError)", stderr.getvalue())

    def test_no_credentials_does_not_construct_model_or_export(self):
        with (
            patch.dict(os.environ, {}, clear=True),
            patch.object(demo.sys.stdin, "isatty", return_value=False),
            patch.object(demo, "ChatGoogleGenerativeAI") as model,
            patch.object(demo, "export_trace") as export,
            redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(demo.main([]), 2)
        model.assert_not_called()
        export.assert_not_called()

    def test_key_prompt_does_not_fall_back_to_echoed_input(self):
        with (
            patch.dict(os.environ, {}, clear=True),
            patch.object(demo.sys.stdin, "isatty", return_value=True),
            patch.object(
                demo.getpass, "getpass", side_effect=demo.getpass.GetPassWarning
            ),
            patch.object(demo, "ChatGoogleGenerativeAI") as model,
            redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(demo.main([]), 2)
            self.assertNotIn("GEMINI_API_KEY", os.environ)
        model.assert_not_called()

    def test_main_uses_requested_model_and_credentials_and_closes_client(self):
        with (
            patch.dict(
                os.environ,
                {"GEMINI_API_KEY": "private-key", "GOOGLE_API_KEY": "wrong-key"},
            ),
            patch.object(demo, "ChatGoogleGenerativeAI") as model,
            patch.object(demo, "run_agent", return_value="answer"),
            redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(demo.main([]), 0)
        kwargs = model.call_args.kwargs
        self.assertEqual(kwargs["model"], "gemini-3.8-flash")
        self.assertEqual(kwargs["api_key"], "private-key")
        self.assertFalse(kwargs["vertexai"])
        self.assertEqual(kwargs["max_retries"], 0)
        self.assertEqual(kwargs["timeout"], 30)
        self.assertEqual(kwargs["thinking_level"], "low")
        model.return_value.client.close.assert_called_once()

    def test_main_exports_failure_trace_and_redacts_provider_error(self):
        def fail(model, tracer, prompt):
            with tracer.start_as_current_span(
                "failed", record_exception=False, set_status_on_exception=False
            ):
                raise RuntimeError("private-key private-prompt")

        stdout, stderr = io.StringIO(), io.StringIO()
        with (
            patch.dict(os.environ, {"GEMINI_API_KEY": "private-key"}),
            patch.object(demo, "ChatGoogleGenerativeAI") as model,
            patch.object(demo, "run_agent", side_effect=fail),
            patch.object(demo, "export_trace", return_value=False) as export,
            redirect_stdout(stdout),
            redirect_stderr(stderr),
        ):
            self.assertEqual(demo.main([]), 1)
        export.assert_called_once()
        model.return_value.client.close.assert_called_once()
        self.assertNotIn("private-key", stdout.getvalue() + stderr.getvalue())
        self.assertNotIn("private-prompt", stdout.getvalue() + stderr.getvalue())
        self.assertIn("RuntimeError", stderr.getvalue())
        self.assertIn("OTLP export failed", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
