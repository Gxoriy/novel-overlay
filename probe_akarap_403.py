"""看 akarap.cc 403 的具体响应：headers + 响应体片段。"""

import ssl
import urllib.error
import urllib.request

PROXY = "http://127.0.0.1:7897"
UA = "NovelOverlay/0.1 (personal reading tool; +https://example.invalid/novel-overlay)"
CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE
OPENER = urllib.request.build_opener(
    urllib.request.ProxyHandler({"http": PROXY, "https": PROXY}),
    urllib.request.HTTPSHandler(context=CTX),
)

for url in ["https://www.akarap.cc/xiaoshuo", "https://www.akarap.cc/", "https://www.akarap.cc/robots.txt"]:
    print(f"\n===== {url}")
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    try:
        with OPENER.open(req, timeout=20) as resp:
            print("status", resp.status)
            raw = resp.read()
    except urllib.error.HTTPError as exc:
        print("status", exc.code)
        print("headers:", dict(exc.headers))
        raw = exc.read()
        print("body 前 600 字节：")
        print(raw[:600].decode("utf-8", "ignore"))
    except Exception as exc:  # noqa: BLE001
        print("ERR", str(exc)[:100])
