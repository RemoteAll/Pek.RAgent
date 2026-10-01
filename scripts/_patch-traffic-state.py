#!/usr/bin/env python3
"""一次性对账：把"历史缺头差额"补进 Pek.RAgent 流量状态（对齐宝塔 site_total 的 sent_bytes 口径）。

用法: _patch-traffic-state.py <DELTA> <站点名>
"""
import json
import sys

P = "/www/Agent/Data/web_traffic.json"

delta = int(sys.argv[1])
site_name = sys.argv[2]
with open(P, "r", encoding="utf-8") as f:
    data = json.load(f)
site = data["sites"][site_name]
site["today"]["bytes"] += delta
site["totalBytes"] += delta
with open(P, "w", encoding="utf-8") as f:
    json.dump(data, f, ensure_ascii=False, indent=2)
print("patched: today.bytes={} totalBytes={}".format(site["today"]["bytes"], site["totalBytes"]))
