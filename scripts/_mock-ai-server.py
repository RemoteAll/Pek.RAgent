# -*- coding: utf-8 -*-
"""AI 接口模拟服务（本地联调用）：OpenAI 兼容 /v1/chat/completions。

用途：在没有真实模型 Key 的情况下验证 Pek.RAgent「AI 助手」链路
（快照组装 → HTTP 请求 → 响应解析 → 面板展示 → 审计）。

    python scripts/_mock-ai-server.py [端口]     # 默认 5688

面板配置（「⚙ 配置」页 → AI 相关项）：
    AI 接口地址 = http://127.0.0.1:5688/v1
    AI 模型     = mock
    AI API Key  = 任意非空（如 test-key）
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 5688


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw.decode("utf-8"))
        except Exception:
            body = {}
        messages = body.get("messages", [])
        snap = any(
            m.get("role") == "system" and "【服务器实况快照】" in m.get("content", "")
            for m in messages
        )
        last = ""
        for m in reversed(messages):
            if m.get("role") == "user":
                last = m.get("content", "")
                break
        auth = self.headers.get("Authorization", "")
        sysmsg = next((m.get("content", "") for m in messages if m.get("role") == "system"), "")
        head = ""
        if "【服务器实况快照】" in sysmsg:
            head = sysmsg[sysmsg.find("【服务器实况快照】"):][:200].replace("\n", " / ")
        reply = (
            f"【模拟答复】共收到 {len(messages)} 条消息；含快照={snap}；"
            f"鉴权头={'有' if auth.startswith('Bearer ') else '无'}；"
            f"模型={body.get('model')}；你的问题：{last[:100]}"
            + (f"｜快照头：{head}" if head else "")
        )
        payload = json.dumps(
            {
                "model": body.get("model", "mock"),
                "choices": [
                    {
                        "message": {"role": "assistant", "content": reply},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 123, "completion_tokens": 45, "total_tokens": 168},
            },
            ensure_ascii=False,
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    print(f"AI mock server on http://127.0.0.1:{PORT}/v1/chat/completions")
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
