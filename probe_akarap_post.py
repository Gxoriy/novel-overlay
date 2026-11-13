"""验证 akarap POST 搜索，并顺带摸书页/目录/正文。"""

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


def post(url, data):
    body = urllib.parse.urlencode(data).encode()
    req = urllib.request.Request(url, data=body, headers={"User-Agent": UA, "Content-Type": "application/x-www-form-urlencoded"})
    with OPENER.open(req, timeout=20) as resp:
        return resp.status, resp.read().decode("utf-8", "ignore"), resp.geturl()


def get(url):
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with OPENER.open(req, timeout=20) as resp:
        return resp.read().decode("utf-8", "ignore")


def anchors(html):
    return [
        (h, re.sub(r"<[^>]+>", "", t).strip())
        for h, t in re.findall(r'<a\s[^>]*href="([^"]+)"[^>]*>(.*?)</a>', html, re.S | re.I)
    ]


out = []
status, html, final_url = post("https://www.akarap.cc/search.html", {"s": KEY})
hits = [(h, t) for h, t in anchors(html) if KEY in t]
out.append(f"POST /search.html [{status}] final={final_url} len={len(html)} 命中 {len(hits)}: {hits[:6]}")
with open(r"F:\MyProjet\novel-overlay\probe\akarap-search.html", "w", encoding="utf-8") as handle:
    handle.write(html)

# 第一本书：/heji/xx/
book = next((h for h, t in hits if re.match(r"^/heji/\d+/?$", h)), None)
if book:
    book_url = urllib.parse.urljoin("https://www.akarap.cc/", book)
    toc_html = get(book_url)
    chapters = [(h, t) for h, t in anchors(toc_html) if re.match(r"^/heji/\d+/\d+\.html$", h)]
    out.append(f"\n书页 {book_url} len={len(toc_html)} 章节 {len(chapters)}: {chapters[:3]}")
    # 章节是否分页（/heji/70/ls2.html 之类）
    pages = [(h, t) for h, t in anchors(toc_html) if re.match(r"^/heji/\d+/ls\d+\.html$", h)]
    out.append(f"目录分页链接 {len(pages)}: {pages[:4]}")
    with open(r"F:\MyProjet\novel-overlay\probe\akarap-toc.html", "w", encoding="utf-8") as handle:
        handle.write(toc_html)

    if chapters:
        chapter_url = urllib.parse.urljoin(book_url, chapters[0][0])
        chapter_html = get(chapter_url)
        text = re.sub(r"<script.*?</script>|<style.*?</style>", " ", chapter_html, flags=re.S)
        text = re.sub(r"<[^>]+>", "", text)
        out.append(f"\n正文 {chapter_url} len={len(chapter_html)} 汉字 {len(re.findall(r'[\u4e00-\u9fff]', text))}")
        with open(r"F:\MyProjet\novel-overlay\probe\akarap-chapter.html", "w", encoding="utf-8") as handle:
            handle.write(chapter_html)

with open(r"F:\MyProjet\novel-overlay\probe\akarap-post.txt", "w", encoding="utf-8") as handle:
    handle.write("\n".join(out))
print("ok")
