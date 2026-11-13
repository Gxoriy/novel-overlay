"""akarap.cc 三层结构探测（直连）：搜索表单、书页、目录、正文。"""

import re
import ssl
import urllib.parse
import urllib.request

UA = "NovelOverlay/0.1 (personal reading tool; +https://example.invalid/novel-overlay)"
KEY = "斗破苍穹"
CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE
OPENER = urllib.request.build_opener(urllib.request.HTTPSHandler(context=CTX))
CJK = re.compile(r"[\u4e00-\u9fff]")


def get(url, timeout=20):
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with OPENER.open(req, timeout=timeout) as resp:
        return resp.read().decode("utf-8", "ignore")


def anchors(html):
    return [
        (h, re.sub(r"<[^>]+>", "", t).strip())
        for h, t in re.findall(r'<a\s[^>]*href="([^"]+)"[^>]*>(.*?)</a>', html, re.S | re.I)
    ]


out = []
home = get("https://www.akarap.cc/xiaoshuo")
out.append(f"===== /xiaoshuo len={len(home)}")
out.append("forms: " + str(re.findall(r"<form[^>]*>", home)[:3]))
out.append("inputs: " + str([re.sub(r"\s+", " ", i)[:120] for i in re.findall(r"<input[^>]*>", home)[:5]]))
out.append("search 相关 js/url: " + str([s for s in re.findall(r"['\"]([^'\"]*search[^'\"]*)['\"]", home, re.I)][:6]))
hits = anchors(home)
out.append(f"anchors {len(hits)}，前 20: {hits[:20]}")
book_like = list(dict.fromkeys([h for h, t in hits if re.search(r"/\d+(_\d+)?(/)?\.?html?$|/book", h)]))
out.append(f"疑似书页 {len(book_like)}: {book_like[:10]}")

# 尝试常见搜索路径
for path, param, enc in [
    ("/search.php", "q", "utf-8"),
    ("/search", "keyword", "utf-8"),
    ("/s", "q", "utf-8"),
    ("/xiaoshuo/search", "keyword", "utf-8"),
    ("/e/search/index.php", "keyboard", "utf-8"),
]:
    url = f"https://www.akarap.cc{path}?{param}=" + urllib.parse.quote(KEY, encoding=enc)
    try:
        html = get(url)
        hits2 = [(h, t) for h, t in anchors(html) if KEY in t]
        out.append(f"搜索尝试 {path} -> len={len(html)} 命中 {len(hits2)}: {hits2[:3]}")
        if hits2:
            with open(r"F:\MyProjet\novel-overlay\probe\akarap-search.html", "w", encoding="utf-8") as handle:
                handle.write(html)
            break
    except Exception as exc:  # noqa: BLE001
        out.append(f"搜索尝试 {path} -> ERR {str(exc)[:60]}")

with open(r"F:\MyProjet\novel-overlay\probe\akarap-structure.txt", "w", encoding="utf-8") as handle:
    handle.write("\n".join(out))
print("ok")
