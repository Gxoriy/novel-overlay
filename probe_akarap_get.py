"""akarap GET 搜索尝试 + 目录 ul 结构确认。"""

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


def get(url, data=None):
    if data is not None:
        req = urllib.request.Request(url, data=data, headers={"User-Agent": UA, "Content-Type": "application/x-www-form-urlencoded"})
    else:
        req = urllib.request.Request(url, headers={"User-Agent": UA})
    with OPENER.open(req, timeout=20) as resp:
        return resp.read().decode("utf-8", "ignore")


out = []
enc = urllib.parse.quote(KEY)
for url in [
    f"https://www.akarap.cc/search.html?s={enc}",
    f"https://www.akarap.cc/search.html?searchkey={enc}",
    f"https://www.akarap.cc/search.html?keyword={enc}",
]:
    try:
        html = get(url)
        hits = [(h, t) for h, t in re.findall(r'<a\s[^>]*href="([^"]+)"[^>]*>(.*?)</a>', html, re.S | re.I)
                if KEY in re.sub(r"<[^>]+>", "", t)]
        out.append(f"GET {url[25:]} -> len={len(html)} 命中 {len(hits)}: {hits[:3]}")
    except Exception as exc:  # noqa: BLE001
        out.append(f"GET {url[25:]} -> ERR {str(exc)[:60]}")

# 确认书页里 ul.section-list 的数量与首尾章节
toc = get("https://www.akarap.cc/shuwu/251128/")
uls = re.findall(r'<ul class="fix section-list">(.*?)</ul>', toc, re.S)
out.append(f"\nul.section-list 数量 {len(uls)}")
for i, ul in enumerate(uls):
    links = re.findall(r'<a href="(/shuwu/251128/\d+\.html)">([^<]+)</a>', ul)
    out.append(f"  ul[{i}] {len(links)} 条，首 {links[:2]} 尾 {links[-2:]}")

with open(r"F:\MyProjet\novel-overlay\probe\akarap-get.txt", "w", encoding="utf-8") as handle:
    handle.write("\n".join(out))
print("ok")
