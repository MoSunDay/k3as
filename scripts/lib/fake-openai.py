#!/usr/bin/env python3
"""Fake OpenAI-compatible server (INF2, TODO T7.6, decision Q62).

Standalone python3-stdlib-only stand-in for `llama-server`: enough of the
OpenAI HTTP surface for the inference fake e2e (scripts/inference-fake-e2e.sh)
to assert the full serving path -- readiness probe, model listing, chat
completions (non-streaming JSON and streaming SSE) -- through a Kubernetes
NodePort, WITHOUT real GPU hardware or a real GGUF model. Real-hardware
evidence for T7.6 is recorded separately (Q62: fake tests are fake-scope
evidence only).

Endpoints:
  GET  /health              -> 200 {"status":"ok"} (readiness probe target)
  GET  /v1/models           -> OpenAI list shape, one fake model alias
  POST /v1/chat/completions -> honors body.stream: one assistant reply
                               (JSON) or SSE chunks terminated by [DONE]

Usage: fake-openai.py --port 8080 [--model fake-model]
"""
import argparse
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def log(msg):
    """Structured-ish stderr line (stdout stays reserved for the test)."""
    sys.stderr.write("[fake-openai] %s %s\n" % (time.strftime("%H:%M:%S"), msg))
    sys.stderr.flush()


def make_handler(model):
    """Build a request-handler class bound to one advertised model alias."""

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        model_alias = model

        def log_message(self, fmt, *args):
            log("%s %s" % (self.address_string(), fmt % args))

        def _send_json(self, code, body):
            raw = json.dumps(body).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            if self.path == "/health":
                self._send_json(200, {"status": "ok"})
                return
            if self.path == "/v1/models":
                self._send_json(200, {"object": "list", "data": [
                    {"id": self.model_alias, "object": "model",
                     "owned_by": "fake"}]})
                return
            self._send_json(404, {"error": {"message":
                                            "not found: %s" % self.path}})

        def do_POST(self):
            if self.path != "/v1/chat/completions":
                self._send_json(404, {"error": {"message":
                                                "not found: %s" % self.path}})
                return
            length = int(self.headers.get("Content-Length") or 0)
            try:
                body = json.loads(self.rfile.read(length) or b"{}")
            except json.JSONDecodeError:
                self._send_json(400, {"error": {"message": "invalid JSON"}})
                return
            messages = body.get("messages") or []
            user_msg = ""
            if messages and isinstance(messages[-1], dict):
                user_msg = str(messages[-1].get("content", ""))
            reply = "echo:%s" % user_msg
            if body.get("stream"):
                self._stream_completion(reply, int(time.time()))
                return
            self._send_json(200, {
                "id": "chatcmpl-fake", "object": "chat.completion",
                "created": int(time.time()),
                "model": body.get("model", self.model_alias),
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant",
                                         "content": reply}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1,
                          "total_tokens": 2},
            })

        def _stream_completion(self, reply, created):
            """Server-sent events: one content chunk per word, then [DONE]."""
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for word in reply.split(" "):
                chunk = {"id": "chatcmpl-fake",
                         "object": "chat.completion.chunk",
                         "created": created, "model": self.model_alias,
                         "choices": [{"index": 0, "finish_reason": None,
                                      "delta": {"content": word + " "}}]}
                self._write_chunk("data: %s\n\n" % json.dumps(chunk))
            self._write_chunk("data: [DONE]\n\n")
            self._write_chunk("")  # terminating zero-length chunk

        def _write_chunk(self, text):
            raw = text.encode()
            self.wfile.write(("%x\r\n" % len(raw)).encode() + raw + b"\r\n")
            self.wfile.flush()

    return Handler


def main():
    parser = argparse.ArgumentParser(description="fake OpenAI server")
    parser.add_argument("--port", type=int, default=8080, help="listen port")
    parser.add_argument("--model", default="fake-model",
                        help="model alias to advertise")
    args = parser.parse_args()
    server = ThreadingHTTPServer(("0.0.0.0", args.port),
                                 make_handler(args.model))
    log("listening on 0.0.0.0:%d model=%s" % (args.port, args.model))
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        log("shutdown")
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
