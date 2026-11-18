"""对比 akarap 搜索：原始 UTF-8 body vs 百分号编码 body。"""

import ssl
import urllib.request

UA = "NovelOverlay/0.1 (personal reading tool; +https://example.invalid/novel-overlay)"
CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE
OPENER = urllib.request.build_opener(urllib.request.HTTPSHandler(context=CTX))

KEY = "斗破苍穹"
RAW = ("s=" + KEY).encode("utf-8")
ENCODED = ("s=%E6%96%97%E7%A0%B4%E8%8B%8D%E7%A9%B9").encode("ascii")

for label, body in [("raw-utf8", RAW), ("urlencoded", ENCODED)]:
    req = urllib.request.Request(
        "https://www.akarap.cc/search.html",
        data=body,
        headers={"User-Agent": UA, "Content-Type": "application/x-www-form-urlencoded"},
    )
    with OPENER.open(req, timeout=20) as resp:
        html = resp.read().decode("utf-8", "ignore")
    hits = html.count("/shuwu/")
    print(f"{label}: len={len(html)} shuwu链接={hits} 有结果卡片={'txt-list' in html}")
