"""akarap.cc 直连（不走代理）测试。"""

import ssl
import urllib.error
import urllib.request

UA = "NovelOverlay/0.1 (personal reading tool; +https://example.invalid/novel-overlay)"
CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE
# 不挂代理
OPENER = urllib.request.build_opener(urllib.request.HTTPSHandler(context=CTX))

for url in ["https://www.akarap.cc/xiaoshuo", "https://akarap.cc/xiaoshuo"]:
    print(f"\n===== {url}")
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    try:
        with OPENER.open(req, timeout=20) as resp:
            raw = resp.read()
            print("status", resp.status, "len", len(raw))
            print("server:", resp.headers.get("Server"), "| cf-ray:", resp.headers.get("CF-Ray"))
            print(raw[:300].decode("utf-8", "ignore"))
    except urllib.error.HTTPError as exc:
        print("status", exc.code, "| body:", exc.read()[:200].decode("utf-8", "ignore"))
    except Exception as exc:  # noqa: BLE001
        print("ERR", str(exc)[:100])
